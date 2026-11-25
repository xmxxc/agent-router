"""Check a downloaded Linux WFC with an explicitly authorized, isolated Codex home."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid


DELEGATION = 'WFC_BUNDLED_CGROUP_ROOT'
CGROUP = Path('/sys/fs/cgroup')
PROCESSES = ('guardian', 'service', 'kernel', 'worker')


def check(condition, reason):
    if not condition:
        raise RuntimeError(reason)


def record(**fields):
    print(json.dumps(fields, ensure_ascii=False), flush=True)


def until(predicate, seconds, reason):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(.05)
    raise RuntimeError(reason)


def read_fixture(path):
    # The worker can only hand back a small, ordinary file at the agreed path.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        check(stat.S_ISREG(os.fstat(fd).st_mode), 'expected a fixture file')
        body = stream.read(4097)
    check(len(body) <= 4096, 'fixture file is too large')
    return body


def file_digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manager_env():
    runtime = Path(f'/run/user/{os.geteuid()}')
    return {'PATH': '/usr/bin:/bin', 'XDG_RUNTIME_DIR': str(runtime),
            'DBUS_SESSION_BUS_ADDRESS': f'unix:path={runtime}/bus'}


def delegation():
    check(sys.platform == 'linux' and os.geteuid() != 0, 'requires an ordinary Linux user')
    if DELEGATION not in os.environ:
        runtime = Path(manager_env()['XDG_RUNTIME_DIR'])
        check((runtime / 'bus').is_socket(),
              f'requires the current user systemd manager or an authorized {DELEGATION}')
        return None
    raw = os.environ[DELEGATION]
    check(raw and Path(raw).is_absolute(), f'{DELEGATION} must be a delegated absolute path')
    root = Path(raw).resolve(strict=True)
    check(CGROUP in root.parents and (root / 'cgroup.controllers').is_file()
          and (root / 'cgroup.kill').is_file() and os.access(root, os.W_OK | os.X_OK)
          and os.access(root / 'cgroup.procs', os.W_OK),
          f'{DELEGATION} must name a writable, authorized cgroup v2 subtree')
    return root


def same_process(identity):
    try:
        fields = Path(f"/proc/{identity['pid']}/stat").read_text().rsplit(') ', 1)[1].split()
    except FileNotFoundError:
        return False
    return int(fields[19]) == identity['start_ticks']


def owned_receipt(row, scope, work, delegated):
    check(row['scope'] == scope and row['cwd'] == str(work)
          and row['host'] == 'codex' and row['backend'] == 'bundled' and row['round'] == 1
          and re.fullmatch(r'd-[0-9a-f]+', row['dispatch'])
          and re.fullmatch(r'a-[0-9a-f]+', row['attempt']),
          'dispatch does not match the private fixture')
    create = row['create']
    ticket = create['ticket']
    check(create['result'] == 'accepted' and row['backendHandle'] == ticket
          and ticket['unit'] == row['dispatch'] and ticket['attempt'] == row['attempt'],
          'missing the original accepted ticket')
    identity = create['identity']
    if delegated is None:
        generation = str(uuid.UUID(ticket['generation']))
        unit = f'wfc-bundled-{generation}.service'
        uid = os.geteuid()
        unit_group = CGROUP / f'user.slice/user-{uid}.slice/user@{uid}.service/app.slice' / unit
        check(identity['userService']['uid'] == uid and identity['userService']['unit'] == unit,
              'user service does not match the original ticket')
        parent = unit_group
    else:
        check(identity.get('userService') is None, 'expected the explicit delegation path')
        unit, unit_group = None, None
        parent = delegated
    containment = identity['containment']
    group = Path(containment['path'])
    check(containment['kind'] == 'cgroup-v2' and group.parent == parent
          and re.fullmatch(r'wfc-[0-9a-f-]{36}', group.name),
          'unexpected containment path')
    boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    check(all(identity[key]['boot'] == boot and identity[key]['pid'] > 0
              and identity[key]['start_ticks'] > 0 for key in PROCESSES),
          'missing original process identities')
    return ticket, identity, group, unit, unit_group


def retired(identity, group, unit, unit_group):
    if group.exists() or any(same_process(identity[key]) for key in PROCESSES):
        return False
    if unit is None:
        return True
    if unit_group.exists():
        return False
    result = subprocess.run(
        ['/usr/bin/systemctl', '--user', '--no-pager', 'show', '--property=LoadState', '--value', '--', unit],
        env=manager_env(), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, timeout=3,
    )
    return result.stdout.strip() == 'not-found'


def run_command(argv, *, cwd, env, timeout, cancel=None):
    deadline = time.monotonic() + timeout
    with subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                          start_new_session=True) as proc:
        while True:
            if cancel and cancel():
                proc.kill()
                proc.wait()
                raise RuntimeError('smoke interrupted')
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                proc.kill()
                proc.wait()
                raise RuntimeError('smoke command timed out')
            try:
                output, _ = proc.communicate(timeout=min(.1, remaining))
                return proc.returncode, output.strip()
            except subprocess.TimeoutExpired:
                continue


def exercise(args, root, delegated, cancelled):
    work = root / 'worker cwd'
    state = root / 'router state'
    report = root / 'reports' / 'result file.txt'
    done_file = report.with_suffix('.done.json')
    task = root / 'task instructions.txt'
    home = root / 'home'
    runtime = root / 'runtime'
    executable = root / 'Codex CLI'
    config = root / 'launch config.json'
    marker = uuid.uuid4().hex
    scope = f'codex-smoke-{marker}'

    def cmd(argv, *, timeout=15, codes=(0,), interruptible=False):
        rc, output = run_command(argv, cwd=work, env=env, timeout=timeout,
                                 cancel=cancelled if interruptible else None)
        check(rc in codes, f'{Path(argv[0]).name} {argv[1]} exited {rc}')
        return rc, output

    def wfc(*argv, **kwargs):
        return json.loads(cmd([args.wfc, *argv], **kwargs)[1])

    def status(dispatch):
        row, = wfc('status', '--dispatch', dispatch)['dispatches']
        return row

    def lifecycle(launch):
        ticket, identity, group, unit, unit_group = owned_receipt(launch, scope, work, delegated)
        group_stat = group.stat()
        check((group_stat.st_dev, group_stat.st_ino) ==
              (identity['containment']['device'], identity['containment']['inode'])
              and all(same_process(identity[key]) for key in PROCESSES),
              'original execution did not survive the dispatch CLI')
        proc = Path(f"/proc/{identity['worker']['pid']}")
        check((proc / 'cwd').resolve(strict=True) == work and
              f'0::/{group.relative_to(CGROUP)}' in
              (proc / 'cgroup').read_text().splitlines(), 'wrong worker cwd or cgroup')
        check(all((Path(f"/proc/{identity[key]['pid']}/exe")).samefile(args.wfc)
                  for key in ('guardian', 'service', 'kernel')), 'bundled processes use a different ELF')
        if unit_group is not None:
            guardian = Path(f"/proc/{identity['guardian']['pid']}")
            check(f'0::/{unit_group.relative_to(CGROUP)}' in (guardian / 'cgroup').read_text().splitlines(),
                  'guardian is outside the original user service')
        record(dispatch=launch['dispatch'], attempt=launch['attempt'], round=1, scope=scope,
               ticketSha256=ticket_digest(ticket), unit=unit,
               workerPid=identity['worker']['pid'], workerStart=identity['worker']['start_ticks'])
        check(launch['launchConfig'] == str(config) and launch['report'] == str(report),
              'wrong launch config or report path')
        check(not cancelled(), 'smoke interrupted')
        # The worker can start before bundled publishes the original ready decision.
        (work / '.dispatch-accepted').touch()
        rc, output = cmd([args.wfc, 'wait', '--scope', scope, '--dispatch', launch['dispatch'],
                          '--timeout', str(args.timeout)], timeout=args.timeout + 10,
                         codes=(0, 3), interruptible=True)
        reply = json.loads(output)
        if rc == 3:
            check(reply == {'timedOut': True, 'messages': []}, 'invalid timeout receipt')
            record(timedOut=True, waitExit=3, messages=0, reportPresent=report.is_file())
            raise RuntimeError('scoped wait timed out without the original done')
        msg, = reply['messages']
        for key in ('dispatch', 'attempt', 'round', 'scope', 'role', 'report'):
            check(msg[key] == launch[key], f'wrong {key} on done receipt')
        check(msg['type'] == 'done' and msg['state'] == 'received', 'expected received done')
        until(done_file.is_file, 5, 'worker did not save its original done receipt')
        done = json.loads(read_fixture(done_file))
        check((done['message'], done['messageId'], done['duplicate']) ==
              (msg['id'], msg['messageId'], False), 'wait did not receive the original done')
        expected = [marker, str(work), 'inherited', 'shared', 'host override', 'runtime argument',
                    str(args.codex_home), str(args.wfc), launch['dispatch'], launch['attempt'],
                    args.elf_sha256]
        check(read_fixture(report) == ('\n'.join(expected) + '\n').encode(),
              'report does not match task, cwd, environment or ELF')
        ack = wfc('ack', done['messageId'])
        check((ack['message'], ack['messageId'], ack['state'], ack['statusVersion'], ack['duplicate']) ==
              (done['message'], done['messageId'], 'processed', 3, False), 'ACK did not process original done')
        after = status(launch['dispatch'])
        saved, = after['messages']
        check(after['backendHandle'] == ticket and after['backendAlive'] is True
              and after['rounds'][-1]['result']['messageId'] == done['messageId']
              and (saved['id'], saved['messageId'], saved['state']) ==
              (done['message'], done['messageId'], 'processed')
              and same_process(identity['worker']), 'original worker or result was lost')
        record(message=done['message'], messageId=done['messageId'], attempt=launch['attempt'],
               receivedState=msg['state'], state=ack['state'], statusVersion=ack['statusVersion'])

    def cleanup(launch, completed):
        if launch is None:
            rows = wfc('status')['dispatches']
            if not rows and not (state / 'bundled').exists():
                return
            check(len(rows) == 1, 'cannot bind cleanup to the original private assignment')
            launch = rows[0]
        ticket, identity, group, unit, unit_group = owned_receipt(launch, scope, work, delegated)
        closed = wfc('close', launch['dispatch'])
        proof = closed['proof']
        check(closed['operation'] == 'close' and closed['result'] == 'completed' and closed['cleanup'] == 'confirmed'
              and (closed['dispatch'], closed['round'], closed['attempt'], closed['ticket']) ==
              (launch['dispatch'], 1, launch['attempt'], ticket)
              and proof['proves'] == 'execution-empty' and proof['identity'] == identity
              and (not completed or proof['state'] == 'closed'),
              'close did not confirm the original ticket and execution')
        after = status(launch['dispatch'])
        check(after['backendHandle'] == ticket and after['backendAlive'] is False
              and after['backendObservation']['cleanup'] == 'confirmed', 'public cleanup status disagreed')
        until(lambda: retired(identity, group, unit, unit_group), 8,
              'original processes, group or user service survived close')
        record(closed=launch['dispatch'], attempt=launch['attempt'], ticketSha256=ticket_digest(closed['ticket']),
               unit=unit, cleanup='confirmed', executionEmpty=True)

    launch = None
    dispatch_attempted = False
    completed = False
    keep = False
    try:
        for path in (work, report.parent, home, runtime):
            path.mkdir(mode=0o700)
        executable.symlink_to(args.codex)
        task.write_text(
            'Run only this shell command once in the assigned work directory. If it fails, stop.\n'
            'timeout 15s /bin/sh -c \'until test -f .dispatch-accepted; do sleep 0.05; done\' && '
            f'printf "%s\\n" "{marker}" "$PWD" "$WFC_PARENT_SAMPLE" "$WFC_SHARED_SAMPLE" '
            '"$WFC_SAMPLE" "$WFC_RUNTIME_SAMPLE" "$CODEX_HOME" "$WFC_BIN" "$WFC_DISPATCH" '
            '"$WFC_ATTEMPT" > "$WFC_REPORT" && '
            'sha256sum -- "$WFC_BIN" | cut -d " " -f 1 >> "$WFC_REPORT" && '
            'receipt="${WFC_REPORT%.txt}.done.json" && '
            '"$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" > "$receipt.tmp" && '
            'mv -- "$receipt.tmp" "$receipt"\n',
            encoding='utf-8',
        )
        env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'CODEX_HOME': str(args.codex_home),
               'XDG_RUNTIME_DIR': str(runtime), 'TMPDIR': str(runtime), 'LC_ALL': 'C.UTF-8',
               'TERM': 'xterm-256color', 'WFC_STATE': str(state), 'WFC_SAMPLE': 'inherited',
               'WFC_PARENT_SAMPLE': 'inherited', 'WFC_SHARED_SAMPLE': 'inherited',
               'WFC_LAUNCH_CONFIG': str(root / 'unselected.json')}
        if delegated is not None:
            env[DELEGATION] = str(delegated)
        fixed = ['-c', f'projects.{json.dumps(str(work))}.trust_level="trusted"',
                 '-c', 'tui.screen_reader_detection_done=true', '-c', 'check_for_update_on_startup=false']
        config.write_text(json.dumps({
            'env': {'WFC_SAMPLE': 'shared', 'WFC_SHARED_SAMPLE': 'shared'},
            'hosts': {'codex': {'runtime': ['/usr/bin/env', '--', 'WFC_RUNTIME_SAMPLE=runtime argument'],
                                'command': [str(executable), *fixed],
                                'env': {'WFC_SAMPLE': 'host override', 'WFC_ATTEMPT': 'overridden'}}},
        }))
        _, version = cmd([executable, '--version'], interruptible=True)
        check(re.fullmatch(r'codex-cli [a-zA-Z0-9.+-]+', version), 'expected a Codex CLI version')
        _, wfc_version = cmd([args.wfc, '--version'], interruptible=True)
        check(re.fullmatch(r'wfc [^\n]+ \(Rust, [^\n]+\)', wfc_version), 'expected a Rust wfc executable')
        record(codex=version, binarySha256=args.elf_sha256,
               delegation='explicit' if delegated is not None else 'user-service')
        check(not cancelled(), 'smoke interrupted')
        dispatch_attempted = True
        launch = wfc('dispatch', '--host', 'codex', '--cwd', work, '--task', task, '--report', report,
                     '--scope', scope, '--launch-config', config, '--model', args.model, '--effort', 'low')
        lifecycle(launch)
        completed = True
    finally:
        if dispatch_attempted:
            try:
                cleanup(launch, completed)
            except Exception:
                keep = True
                record(cleanup='unconfirmed', privateFixture=str(root))
                raise
        if not keep:
            shutil.rmtree(root)


def ticket_digest(ticket):
    return hashlib.sha256(json.dumps(ticket, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        'Run as an ordinary user in a dedicated test environment, with the current systemd user manager '
        'or an authorized WFC_BUNDLED_CGROUP_ROOT. Supply an already authorized, dedicated Codex home.'))
    parser.add_argument('wfc', type=Path, help='ELF extracted from the verified Linux package')
    parser.add_argument('--elf-sha256', required=True, help='ELF SHA-256 from the package build')
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--codex-home', type=Path, required=True, help='existing dedicated profile directory')
    parser.add_argument('--model', required=True)
    parser.add_argument('--timeout', type=int, default=180)
    args = parser.parse_args()
    requested_signal = None

    def cancel(signum, _):
        nonlocal requested_signal
        requested_signal = signum

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, cancel)
    try:
        args.wfc = args.wfc.resolve(strict=True)
        args.codex = args.codex.resolve(strict=True)
        args.codex_home = args.codex_home.resolve(strict=True)
        check(args.timeout > 0 and args.codex_home.is_dir(), 'requires a positive timeout and a dedicated Codex home')
        check(args.wfc.is_file() and os.access(args.wfc, os.X_OK)
              and file_digest(args.wfc) == args.elf_sha256, 'WFC ELF SHA-256 does not match the package')
        check(args.codex.is_file() and os.access(args.codex, os.X_OK), 'expected an executable Codex CLI')
        delegated = delegation()
        os.umask(0o077)
        root = Path(tempfile.mkdtemp(prefix='wfc codex ')).resolve()
        exercise(args, root, delegated, lambda: requested_signal is not None)
        check(file_digest(args.wfc) == args.elf_sha256, 'WFC ELF changed during the check')
        if requested_signal is None:
            record(result='BUNDLED_CODEX_SMOKE_PASS', binarySha256=args.elf_sha256)
            return 0
    except RuntimeError as error:
        print(error, file=sys.stderr)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        print('invalid or unavailable smoke evidence', file=sys.stderr)
    if requested_signal is not None:
        return 128 + requested_signal
    return 1


if __name__ == '__main__':
    sys.exit(main())
