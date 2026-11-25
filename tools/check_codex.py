"""Run a real, authenticated Codex worker through a built Linux wfc and private tmux."""
import argparse
import ctypes
import hashlib
import json
import multiprocessing
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import tempfile
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None


def check(condition, reason):
    if not condition:
        raise RuntimeError(reason)


def record(**fields):
    print(json.dumps(fields, ensure_ascii=False), flush=True)


def reap():
    # tmux daemonizes. As the Linux subreaper we own orphaned descendants,
    # including any helpers which outlive the closing pane.
    children = Path('/proc/thread-self/children')
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        try:
            if os.waitpid(-1, os.WNOHANG)[0]:
                continue
        except ChildProcessError:
            return
        for pid in children.read_text().split():
            try:
                os.kill(int(pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(.05)
    raise RuntimeError('test descendants still running')


def exercise(args, root):
    work = root / 'worker cwd'
    work.mkdir()
    state = root / 'router state'
    report = root / 'reports' / 'result file.txt'
    task = root / 'task instructions.txt'
    socket = root / 'tmux.sock'
    home = root / 'home'
    codex_home = home / '.codex'
    codex_home.mkdir(parents=True, mode=0o700)
    if args.auth_file:
        shutil.copyfile(args.auth_file, codex_home / 'auth.json')
    executable = root / 'Codex CLI'
    executable.symlink_to(args.codex.resolve(strict=True))
    config = root / 'launch config.json'
    marker = secrets.token_hex(12)
    task.write_text(
        'The entire task is this shell command. Execute it once in the assigned cwd. '
        'Write only this report and submit it. If the shell cannot run, stop and describe the error.\n'
        f'printf "%s\\n" "{marker}" "$PWD" "$WFC_SAMPLE" "$WFC_RUNTIME_SAMPLE" '
        '"$WFC_DISPATCH" "$WFC_ATTEMPT" > "$WFC_REPORT" && '
        '"$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT"\n',
        encoding='utf-8')
    env = {k: v for k, v in os.environ.items() if k.lower() in
           ('path', 'http_proxy', 'https_proxy', 'no_proxy', 'all_proxy',
            'ssl_cert_file', 'ssl_cert_dir')}
    private_runtime = root / 'runtime'
    private_runtime.mkdir(mode=0o700)
    env.update(HOME=str(home), XDG_RUNTIME_DIR=str(private_runtime), TMPDIR=str(private_runtime),
               LANG='C.UTF-8', TERM='xterm-256color', CODEX_HOME=str(codex_home),
               WFC_STATE=str(state), WFC_TMUX_SOCKET=str(socket), WFC_SAMPLE='inherited',
               WFC_PARENT_SAMPLE='inherited', WFC_SHARED_SAMPLE='inherited',
               WFC_LAUNCH_CONFIG=str(root / 'unselected.json'))
    if not args.auth_file:
        key = os.environ.get('OPENAI_API_KEY')
        check(key, 'provide --auth-file or OPENAI_API_KEY for the private Codex profile')
        login = subprocess.run([executable, '-c', 'cli_auth_credentials_store="file"',
                                'login', '--with-api-key'], input=key.encode(), env=env, cwd=work,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        check(login.returncode == 0, f'private Codex login exited {login.returncode}')
    fixed = ['-c', f'projects.{json.dumps(str(work))}.trust_level="trusted"',
             '-c', 'tui.screen_reader_detection_done=true', '-c', 'check_for_update_on_startup=false']
    runtime = [shutil.which('env'), '--', 'WFC_RUNTIME_SAMPLE=runtime argument']
    config.write_text(json.dumps({
        'env': {'WFC_SAMPLE': 'shared', 'WFC_SHARED_SAMPLE': 'shared'},
        'hosts': {'codex': {'runtime': runtime, 'command': [str(executable), *fixed],
                            'env': {'WFC_SAMPLE': 'host override', 'WFC_ATTEMPT': 'overridden'}}}}))

    def cmd(argv, timeout=15, codes=(0,)):
        p = subprocess.run(argv, cwd=work, env=env, capture_output=True, text=True, timeout=timeout)
        check(p.returncode in codes, f'{Path(argv[0]).name} {argv[1]} exited {p.returncode}')
        return p.stdout.strip()

    def wfc(*args_, **kwargs):
        return json.loads(cmd([args.wfc, *args_], **kwargs))

    tmux = ['tmux', '-f', '/dev/null', '-S', str(socket)]
    version = cmd([executable, '--version'])
    check(version.startswith('codex-cli '), 'expected a Codex CLI version')
    wfc_version = cmd([args.wfc, '--version'])
    check('(Rust,' in wfc_version, 'expected a Rust wfc executable')
    record(codex=version, executable=str(executable.resolve()), wfc=wfc_version,
           binarySha256=digest(args.wfc), tmux=cmd(['tmux', '-V']))
    launch = None
    try:
        # Start the private server without loading the user's tmux configuration.
        cmd([*tmux, 'new-session', '-d', '-s', 'keeper', 'sleep', 'infinity'])
        launch = wfc('dispatch', '--host', 'codex', '--backend', 'tmux', '--cwd', work,
                     '--task', task,
                     '--report', report, '--scope', 'codex-smoke', '--launch-config', config,
                     '--model', args.model, '--effort', 'low')
        ident = launch['dispatch']
        check(launch['create']['result'] == 'accepted' and launch['tmuxSocket'] == str(socket)
              and launch['launchConfig'] == str(config) and launch['cwd'] == str(work)
              and launch['report'] == str(report) and launch['scope'] == 'codex-smoke',
              'wrong launch, report or socket')
        pid = json.loads(Path(launch['create']['receipt']).read_bytes())['pid']
        proc = Path('/proc') / str(pid)
        actual = os.fsdecode((proc / 'cmdline').read_bytes()).rstrip('\0').split('\0')
        flags = [*fixed, '--no-alt-screen', '-s', 'workspace-write', '-a', 'never',
                 '-m', args.model, '-c', 'model_reasoning_effort=low',
                 '--add-dir', str(state), '--add-dir', str(report.parent)]
        check(str(executable) in actual[:-len(flags)-1] and actual[-len(flags)-1:-1] == flags
              and ident in actual[-1], 'wrong live Codex argv')
        check((proc / 'cwd').resolve() == work, 'wrong live cwd')
        expected = {'WFC_PARENT_SAMPLE': 'inherited', 'WFC_SHARED_SAMPLE': 'shared',
                    'WFC_SAMPLE': 'host override', 'WFC_RUNTIME_SAMPLE': 'runtime argument',
                    'WFC_ATTEMPT': launch['attempt'], 'WFC_BIN': str(args.wfc)}
        entries = (proc / 'environ').read_bytes().split(b'\0')
        for key, value in expected.items():
            check(os.fsencode(f'{key}={value}') in entries, f'wrong worker environment for {key}')
        record(dispatch=ident, attempt=launch['attempt'], cwd=str(work), config=str(config),
               socket=str(socket), runtime=runtime, command=str(executable), actualFlags=flags,
               verifiedEnv=expected, pid=pid)
        wait = subprocess.run([args.wfc, 'wait', '--scope', launch['scope'], '--dispatch', ident,
                               '--timeout', str(args.timeout)], cwd=work, env=env,
                              capture_output=True, timeout=args.timeout + 10)
        check(wait.returncode in (0, 3), f'wfc wait exited {wait.returncode}')
        reply = json.loads(wait.stdout)
        if wait.returncode == 3:
            check(reply == {'timedOut': True, 'messages': []}, 'invalid timeout receipt')
            # Never export arbitrary pane text. Only name recognized conditions.
            pane = cmd([*tmux, 'capture-pane', '-p', '-t', f'=wfc-{ident}:'], codes=(0, 1)).lower()
            clues = [s for s in ('trust', 'sign in', 'reconnecting', 'failed', 'error', 'sandbox',
                                  'screen reader', 'permission denied', 'upgrade', 'unauthorized')
                     if s in pane]
            row, = wfc('status', '--dispatch', ident)['dispatches']
            diagnostic = 'app-server socket directory must be a user-owned directory with mode 0700'
            # Match only the fixed host diagnostic in tool results from this private session.
            socket_error = False
            for rollout in (codex_home / 'sessions').rglob('*.jsonl'):
                with rollout.open('rb') as stream:
                    for line in stream:
                        if diagnostic.encode() in line and b'tool_call_output' in line:
                            socket_error = True
            record(timedOut=True, waitExit=3, messages=len(reply['messages']),
                   reportPresent=report.is_file(), worker=row['worker'], paneSignals=clues,
                   hostDiagnostic=diagnostic if socket_error else None)
            raise RuntimeError('scoped wait timed out without a worker receipt')
        msg, = reply['messages']
        for key in ('dispatch', 'attempt', 'round', 'scope', 'role', 'report'):
            check(msg[key] == launch[key], f'wrong {key} on done receipt')
        check(msg['type'] == 'done' and msg['state'] == 'received', 'expected received done')
        body = Path(msg['report']).read_text().splitlines()
        check(body == [marker, str(work), 'host override', 'runtime argument', ident, launch['attempt']],
              'report does not match task, runtime, cwd or attempt')
        row, = wfc('status', '--dispatch', ident)['dispatches']
        check(row['state'] == 'done' and row['rounds'][0]['result']['messageId'] == msg['messageId'],
              'public status did not match wait receipt')
        record(report=str(report), reportLines=body, messageId=msg['messageId'], scope=msg['scope'],
               round=msg['round'], state=msg['state'])
    finally:
        try:
            if launch:
                closed = wfc('close', launch['dispatch'])
                check(closed['result'] == 'completed' and closed['attempt'] == launch['attempt'],
                      'close did not confirm this attempt')
                after, = wfc('status', '--dispatch', closed['dispatch'])['dispatches']
                record(closed=closed['dispatch'], result=closed['result'], worker=after['worker'],
                       backendAlive=after['backendAlive'])
        finally:
            subprocess.run([*tmux, 'kill-server'], env=env, capture_output=True, timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('wfc', type=Path)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--model', required=True)
    parser.add_argument('--auth-file', type=Path, help='copy an existing Codex login into the temporary home')
    parser.add_argument('--timeout', type=int, default=180)
    args = parser.parse_args()
    args.wfc = args.wfc.resolve(strict=True)
    check(args.timeout > 0, 'timeout must be positive')
    check(ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0, 'requires Linux subreaper support')
    config = Path(os.getenv('CODEX_HOME', Path.home() / '.codex')) / 'config.toml'
    before = digest(config)
    os.umask(0o077)
    cancelled = False

    def cancel(signum, frame):
        nonlocal cancelled
        cancelled = True

    # The supervisor owns the directory and adopts daemonized descendants. A
    # TERM only sets its flag, even during fork, wait, reaping or file removal.
    signal.signal(signal.SIGTERM, cancel)
    try:
        with tempfile.TemporaryDirectory(prefix='wfc codex ') as tmp:
            job = multiprocessing.get_context('fork').Process(target=exercise, args=(args, Path(tmp)))
            try:
                job.start()
                while job.is_alive():
                    if cancelled:
                        job.kill()
                    job.join(.1)
                if job.exitcode:
                    raise SystemExit(job.exitcode if job.exitcode > 0 else 128 - job.exitcode)
            finally:
                reap()
                record(descendants='all exited')
    finally:
        try:
            check(digest(config) == before, 'global Codex config changed')
            record(globalConfigUnchanged=True)
        finally:
            signal.signal(signal.SIGTERM, signal.SIG_DFL)
            if cancelled:
                signal.raise_signal(signal.SIGTERM)


if __name__ == '__main__':
    main()
