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
TERMINAL_STATES = ('closed', 'exited', 'faulted', 'startup-aborted')
REFUSED_TICKET = 'product refused the original bundled ticket'


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
    except (FileNotFoundError, ProcessLookupError):
        return False
    return int(fields[19]) == identity['start_ticks']


def worker_arguments_match(proc, executables, flags, brief_refs):
    try:
        with (proc / 'cmdline').open('rb') as stream:
            raw = stream.read(32769)
        if (proc / 'exe').samefile('/usr/bin/env'):
            return False
    except (FileNotFoundError, ProcessLookupError):
        return False
    if len(raw) > 32768 or not raw.endswith(b'\0'):
        return False
    fields = raw[:-1].split(b'\0')
    split = len(fields) - len(flags) - 1
    return (split > 0 and fields[split - 1] in [os.fsencode(path) for path in executables]
            and fields[split:-1] == [os.fsencode(arg) for arg in flags]
            and all(os.fsencode(ref) in fields[-1] for ref in brief_refs))


def check_assignment(row, scope, work, task, report):
    check(isinstance(row, dict) and row.get('scope') == scope and row.get('cwd') == str(work)
          and row.get('task') == str(task) and row.get('report') == str(report)
          and row.get('host') == 'codex' and row.get('backend') == 'bundled' and row.get('round') == 1
          and row.get('publicationDiagnostic') is None and row.get('controllerOwner') is None
          and isinstance(row.get('dispatch'), str) and re.fullmatch(r'd-[0-9a-f]+', row['dispatch'])
          and isinstance(row.get('attempt'), str) and re.fullmatch(r'a-[0-9a-f]+', row['attempt']),
          'dispatch does not match the private fixture')


def check_ticket(row, ticket):
    check(isinstance(ticket, dict) and ticket.get('version') == 2
          and ticket.get('unit') == row['dispatch'] and ticket.get('attempt') == row['attempt']
          and row.get('backendHandle') == ticket, 'product did not bind the original bundled ticket')
    check(isinstance(ticket.get('generation'), str)
          and re.fullmatch(r'[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}', ticket['generation']),
          'original bundled ticket lacks a valid generation')


def owned_resources(ticket, identity, delegated):
    if delegated is None:
        generation = str(uuid.UUID(ticket['generation']))
        unit = f'wfc-bundled-{generation}.service'
        uid = os.geteuid()
        unit_group = CGROUP / f'user.slice/user-{uid}.slice/user@{uid}.service/app.slice' / unit
        user_service = identity['userService']
        check(user_service['uid'] == uid and user_service['unit'] == unit
              and re.fullmatch(r'[0-9a-f]{32}', user_service['invocation']),
              'user service does not match the original ticket')
        parent = unit_group
    else:
        check(identity.get('userService') is None, 'expected the explicit delegation path')
        unit, unit_group = None, None
        parent = delegated
    containment = identity['containment']
    group = Path(containment['path'])
    check(containment['kind'] == 'cgroup-v2' and group.parent == parent
          and re.fullmatch(r'wfc-[0-9a-f-]{36}', group.name)
          and type(containment['device']) is int and containment['device'] > 0
          and type(containment['inode']) is int and containment['inode'] > 0,
          'unexpected containment path')
    boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    check(all(identity[key]['boot'] == boot
              and type(identity[key]['pid']) is int and identity[key]['pid'] > 0
              and type(identity[key]['start_ticks']) is int and identity[key]['start_ticks'] > 0
              for key in PROCESSES),
          'missing original process identities')
    return group, unit, unit_group


def recovered_receipt(row, partial):
    observation = row.get('backendObservation')
    check(isinstance(observation, dict) and observation.get('scope') != 'execution',
          'product did not confirm a binding for the private assignment')
    result = observation.get('result')
    create = row.get('create') or {}
    partial_create = (partial.get('create') or {}) if isinstance(partial, dict) else {}
    check('refused' not in (result, row.get('backendResult'), create.get('result'),
                            partial_create.get('result')),
          REFUSED_TICKET)
    ticket = observation.get('ticket')
    check_ticket(row, ticket)
    check(create.get('ticket') in (None, ticket), 'status has conflicting bundled tickets')
    if isinstance(partial, dict):
        check(all(partial.get(key) in (None, row[key]) for key in ('dispatch', 'attempt', 'round', 'scope'))
              and partial.get('backendHandle') in (None, ticket)
              and partial_create.get('ticket') in (None, ticket),
              'status does not match the partial launch receipt')

    # The product echoes the binding candidate even after a failed inspect.
    # Require evidence from the operation, in addition to matching the ticket.
    identity = create.get('identity') if create.get('result') == 'accepted' else None
    if (result == 'accepted' and observation.get('owned') is True
            and observation.get('alive') is True and row.get('backendAlive') is True):
        return ticket, identity
    if (result == 'uncertain' and observation.get('startup') == 'prepared'
            and observation.get('cleanup') == 'unknown' and ticket.get('startupProtocol') == 1
            and row.get('backendAlive') is None):
        prepared = observation.get('startupIdentity')
        check(isinstance(prepared, dict) and identity in (None, prepared),
              'prepared startup lacks matching execution identity')
        return ticket, prepared
    if (result == 'completed' and observation.get('cleanup') == 'confirmed'
            and observation.get('alive') is False and row.get('backendAlive') is False):
        proof = observation.get('proof')
        check(isinstance(proof, dict) and proof.get('proves') == 'execution-empty'
              and proof.get('state') in TERMINAL_STATES and isinstance(proof.get('identity'), dict)
              and identity in (None, proof['identity']),
              'completed observation lacks matching execution proof')
        return ticket, proof['identity']
    raise RuntimeError('recovered ticket has no verified live, prepared or terminal evidence')


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


def run_command(argv, *, cwd, env, timeout, cancel=None, version_probe=False):
    deadline = time.monotonic() + timeout
    with subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                          start_new_session=True) as proc:
        while True:
            interrupted = cancel is not None and cancel()
            remaining = deadline - time.monotonic()
            if interrupted or remaining <= 0:
                if version_probe:
                    # This probe owns the session created by Popen, including a wrapper's children.
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                else:
                    proc.kill()
                proc.wait()
                raise RuntimeError('smoke interrupted' if interrupted else 'smoke command timed out')
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
    refusal_seen = False

    def cmd(argv, *, timeout=15, codes=(0,), interruptible=False):
        rc, output = run_command(argv, cwd=work, env=env, timeout=timeout,
                                 cancel=cancelled if interruptible else None,
                                 version_probe=argv[1:] == ['--version'])
        check(rc in codes, f'{Path(argv[0]).name} {argv[1]} exited {rc}')
        return rc, output

    def wfc(*argv, **kwargs):
        return json.loads(cmd([args.wfc, *argv], **kwargs)[1])

    def status(dispatch):
        nonlocal refusal_seen
        row, = wfc('status', '--dispatch', dispatch)['dispatches']
        observation = row.get('backendObservation') or {}
        if row.get('backendResult') == 'refused' or observation.get('result') == 'refused':
            refusal_seen = True
            raise RuntimeError(REFUSED_TICKET)
        return row

    def lifecycle(launch, ticket, identity, resources):
        group, unit, unit_group = resources
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
        flags = [*fixed, '--no-alt-screen', '-s', 'workspace-write', '-a', 'never',
                 '-m', args.model, '-c', 'model_reasoning_effort=low',
                 '--add-dir', str(state), '--add-dir', str(report.parent)]
        brief_refs = (f"(dispatch {launch['dispatch']})", str(task), str(report))
        until(lambda: same_process(identity['worker'])
              and worker_arguments_match(proc, (executable, args.codex), flags, brief_refs)
              and same_process(identity['worker']), 3, 'live Codex arguments did not match the authorized launch')
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

    def cleanup(launch, binding, completed):
        check(not refusal_seen, REFUSED_TICKET)
        recovered = binding is None
        prior_proof = None
        if recovered:
            rows = wfc('status')['dispatches']
            check(isinstance(rows, list) and len(rows) == 1,
                  'could not identify one original assignment in the private status')
            check_assignment(rows[0], scope, work, task, report)
            binding = recovered_receipt(rows[0], launch)
            launch = rows[0]
            observation = launch['backendObservation']
            if observation['result'] == 'completed':
                prior_proof = observation['proof']
        ticket, identity = binding
        if recovered and identity is not None:
            owned_resources(ticket, identity, delegated)
        closed = wfc('close', launch['dispatch'])
        proof = closed['proof']
        check(closed['operation'] == 'close' and closed['result'] == 'completed' and closed['cleanup'] == 'confirmed'
              and (closed['dispatch'], closed['round'], closed['attempt'], closed['ticket']) ==
              (launch['dispatch'], 1, launch['attempt'], ticket)
              and proof['proves'] == 'execution-empty'
              and proof['state'] in TERMINAL_STATES
              and (prior_proof is None or proof == prior_proof)
              and (identity is None or proof['identity'] == identity)
              and (not completed or proof['state'] == 'closed'),
              'close did not confirm the original ticket and execution')
        identity = proof['identity']
        group, unit, unit_group = owned_resources(ticket, identity, delegated)
        after = status(launch['dispatch'])
        check_assignment(after, scope, work, task, report)
        observation = after['backendObservation']
        check((after['dispatch'], after['attempt']) == (launch['dispatch'], launch['attempt']),
              'cleanup status does not match the original dispatch and attempt')
        check(after['backendHandle'] == ticket and after['backendAlive'] is False
              and observation['ticket'] == ticket and observation['proof'] == proof
              and observation['cleanup'] == 'confirmed', 'public cleanup status disagreed')
        until(lambda: retired(identity, group, unit, unit_group), 8,
              'original processes, group or user service survived close')
        record(closed=launch['dispatch'], attempt=launch['attempt'], ticketSha256=ticket_digest(closed['ticket']),
               unit=unit, cleanup='confirmed', executionEmpty=True, recoveredLaunch=recovered)

    launch = None
    binding = None
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
        check_assignment(launch, scope, work, task, report)
        create = launch.get('create')
        check(isinstance(create, dict) and create.get('result') == 'accepted',
              'dispatch did not publish the original accepted receipt')
        ticket = create.get('ticket')
        check_ticket(launch, ticket)
        identity = create['identity']
        resources = owned_resources(ticket, identity, delegated)
        binding = ticket, identity
        lifecycle(launch, ticket, identity, resources)
        completed = True
    finally:
        if dispatch_attempted:
            try:
                cleanup(launch, binding, completed)
            except Exception as error:
                keep = True
                reason = str(error) if isinstance(error, RuntimeError) else 'invalid or unavailable cleanup evidence'
                record(cleanup='unconfirmed', privateFixture=str(root), reason=reason)
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
