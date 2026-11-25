"""Use the downloaded WFC and a controlled Codex stand-in with a fictional profile."""
from contextlib import redirect_stdout
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import signal
import shutil
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

CHECKER = Path(__file__).resolve().parents[1] / 'tools/check_codex.py'
WFC = Path(sys.argv.pop(1)).resolve(strict=True)
ELF_SHA256 = hashlib.sha256(WFC.read_bytes()).hexdigest()
DELEGATION = 'WFC_BUNDLED_CGROUP_ROOT'
FIXTURE = r"""#!/usr/bin/python3
import json, os, re, subprocess, sys, time
from pathlib import Path

host = Path(__file__).resolve()
root, mode = host.parent, host.name
if sys.argv[1:] == ['--version']:
    if mode == 'cancel-version':
        child = subprocess.Popen(['/usr/bin/sleep', '60'])
        stat = Path(f'/proc/{child.pid}/stat').read_text().rsplit(') ', 1)[1].split()
        pending = root / 'version.pending'
        pending.write_text(f'{child.pid} {stat[19]}')
        pending.rename(root / 'version')
        time.sleep(60)
    print('codex-cli controlled')
    sys.exit()

e = os.environ
profile = root / 'profile with spaces'
assert Path(e['CODEX_HOME']) == profile
assert (profile / 'fixture-marker').read_text() == 'fictional profile'
assert 'OPENAI_API_KEY' not in e and 'CODEX_API_KEY' not in e
assert e['WFC_PARENT_SAMPLE'] == 'inherited'
assert e['WFC_SHARED_SAMPLE'] == 'shared'
assert e['WFC_SAMPLE'] == 'host override'
assert e['WFC_RUNTIME_SAMPLE'] == 'runtime argument'
assert re.fullmatch(r'a-[0-9a-f]+', e['WFC_ATTEMPT'])
state, report, task = (Path(e[key]) for key in ('WFC_STATE', 'WFC_REPORT', 'WFC_TASK'))
work = state.parent / 'worker cwd'
assert Path.cwd() == work and state.parent == report.parent.parent == task.parent
assert sys.argv[1:-1] == [
    '-c', f'projects.{json.dumps(str(work))}.trust_level="trusted"',
    '-c', 'tui.screen_reader_detection_done=true', '-c', 'check_for_update_on_startup=false',
    '--no-alt-screen', '-s', 'workspace-write', '-a', 'never',
    '-m', 'fixture', '-c', 'model_reasoning_effort=low', '--add-dir', str(state),
    '--add-dir', str(report.parent),
]
assert all(ref in sys.argv[-1] for ref in (e['WFC_DISPATCH'], str(task), str(report)))
instruction, command = task.read_text().splitlines()
assert instruction.startswith('Run only this shell command')
if mode in ('cancel-running', 'file-only'):
    report.write_text('fixture without a done receipt\n')
else:
    if mode == 'wrong':
        command = re.sub(r'"[0-9a-f]{32}"', '"invalid"', command, count=1)
    subprocess.run(['/bin/sh', '-eu', '-c', command], check=True)
(root / 'running').touch()
time.sleep(60)
"""


def probe_child_running(marker):
    pid, start = marker.read_text().split()
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
    except (FileNotFoundError, ProcessLookupError):
        return False
    return fields[19] == start and fields[0] not in ('Z', 'X')


def wait_for_probe_exit(marker):
    deadline = time.monotonic() + 2
    while probe_child_running(marker):
        if time.monotonic() >= deadline:
            return False
        time.sleep(.02)
    return True


class Checker(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='c9-')).resolve()
        self.addCleanup(self.cleanup_root)
        self.profile = self.root / 'profile with spaces'
        self.profile.mkdir()
        (self.profile / 'fixture-marker').write_text('fictional profile')
        self.env = dict(PATH='/usr/bin:/bin', TMPDIR=str(self.root), HOME=str(self.root),
                        LC_ALL='C.UTF-8', OPENAI_API_KEY='fictional-parent-marker',
                        CODEX_API_KEY='fictional-parent-marker')
        if DELEGATION in os.environ:
            self.env[DELEGATION] = os.environ[DELEGATION]

    def cleanup_root(self):
        # A failed cleanup proof retains its own state for read-only diagnosis.
        if not list(self.root.glob('wfc codex *')):
            shutil.rmtree(self.root)

    def run_case(self, mode, *, cancel_at=None, elf_sha=ELF_SHA256):
        host = self.root / mode
        host.write_text(FIXTURE)
        host.chmod(0o700)
        args = [sys.executable, CHECKER, WFC, '--codex', host, '--codex-home', self.profile,
                '--model', 'fixture', '--elf-sha256', elf_sha,
                '--timeout', '2' if mode == 'file-only' else '12']
        proc = subprocess.Popen(args, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            if cancel_at:
                deadline = time.monotonic() + 15
                while not (self.root / cancel_at).exists():
                    self.assertIsNone(proc.poll(), cancel_at)
                    self.assertLess(time.monotonic(), deadline, cancel_at)
                    time.sleep(.02)
                proc.terminate()
                for _ in range(4):
                    if proc.poll() is not None:
                        break
                    time.sleep(.01)
                    proc.terminate()
            out, err = proc.communicate(timeout=45)
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.communicate(timeout=25)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.communicate()
        self.assertNotIn('fictional-parent-marker', out + err)
        self.assertEqual(list(self.root.glob('wfc codex *')), [], out + err)
        self.assertEqual([p.name for p in self.profile.iterdir()], ['fixture-marker'])
        self.assertEqual((self.profile / 'fixture-marker').read_text(), 'fictional profile')
        return proc.returncode, [json.loads(line) for line in out.splitlines()], err

    def check_close(self, rows):
        dispatch, = [r for r in rows if 'workerPid' in r]
        closed, = [r for r in rows if r.get('cleanup') == 'confirmed']
        self.assertEqual((closed['closed'], closed['attempt'], closed['ticketSha256'], closed['unit']),
                         (dispatch['dispatch'], dispatch['attempt'], dispatch['ticketSha256'], dispatch['unit']))
        self.assertTrue(closed['executionEmpty'])
        if DELEGATION in self.env:
            self.assertIsNone(closed['unit'])
        else:
            self.assertRegex(closed['unit'], r'^wfc-bundled-[0-9a-f-]+\.service$')
        return dispatch, closed

    def test_original_done_ack_and_close(self):
        rc, rows, err = self.run_case('good')
        self.assertEqual(rc, 0, err)
        message, = [r for r in rows if 'messageId' in r]
        self.assertEqual((message['state'], message['statusVersion']), ('processed', 3))
        self.assertIs(type(message['message']), int)
        self.assertTrue(0 < message['message'] < 2**64)
        dispatch, closed = self.check_close(rows)
        self.assertEqual(rows[-1], {'result': 'BUNDLED_CODEX_SMOKE_PASS', 'binarySha256': ELF_SHA256})
        self.assertRegex(dispatch['dispatch'], r'^d-[0-9a-f]{32}$')
        self.assertRegex(dispatch['attempt'], r'^a-[0-9a-f]{32}$')
        self.assertRegex(dispatch['scope'], r'^codex-smoke-[0-9a-f]{32}$')
        self.assertRegex(dispatch['ticketSha256'], r'^[0-9a-f]{64}$')
        self.assertRegex(message['messageId'], r'^m-[0-9a-f]{32}$')
        self.assertEqual(message['attempt'], dispatch['attempt'])
        self.assertEqual(message['receivedState'], 'received')
        receipt = {'mode': 'explicit' if DELEGATION in self.env else 'user-service', 'binarySha256': ELF_SHA256,
                   'dispatch': dispatch['dispatch'], 'scope': dispatch['scope'], 'round': 1,
                   'attempt': dispatch['attempt'], 'number': message['message'], 'messageId': message['messageId'],
                   'received': message['receivedState'], 'ack': message['state'],
                   'ackVersion': message['statusVersion'], 'launchTicketSha256': dispatch['ticketSha256'],
                   'closeTicketSha256': closed['ticketSha256'], 'cleanup': closed['cleanup']}
        print('BUNDLED_CODEX_RECEIPT ' + json.dumps(receipt, separators=(',', ':')), flush=True)

    def test_wrong_body_and_missing_receipt(self):
        rc, rows, err = self.run_case('wrong')
        self.assertEqual(rc, 1)
        self.assertIn('report does not match', err)
        self.check_close(rows)
        rc, rows, _ = self.run_case('file-only')
        self.assertEqual(rc, 1)
        self.assertTrue(any(r.get('reportPresent') and r['waitExit'] == 3 and not r['messages'] for r in rows))
        self.check_close(rows)

    def test_mismatched_payload_stops_before_launch(self):
        rc, rows, err = self.run_case('wrong-elf', elf_sha='0' * 64)
        self.assertEqual((rc, rows), (1, []))
        self.assertIn('ELF SHA-256', err)
        self.assertFalse((self.root / 'running').exists())

    def test_terminate_before_dispatch(self):
        rc, rows, _ = self.run_case('cancel-version', cancel_at='version')
        self.assertEqual((rc, rows), (128 + signal.SIGTERM, []))
        self.assertTrue(wait_for_probe_exit(self.root / 'version'))

    def test_terminate_with_live_worker(self):
        rc, rows, _ = self.run_case('cancel-running', cancel_at='running')
        self.assertEqual(rc, 128 + signal.SIGTERM)
        self.check_close(rows)

    def test_crossed_scope_and_dispatch_leave_both_receipts_available(self):
        env = dict(self.env, WFC_STATE=str(self.root / 'state'))
        task = self.root / 'task'
        task.touch()

        def run(*args, rc=0, **context):
            result = subprocess.run([WFC, *args], cwd=self.root, env=dict(env, **context),
                                    capture_output=True, timeout=10)
            self.assertEqual(result.returncode, rc)
            return json.loads(result.stdout)

        launches = []
        for scope in ('left', 'right'):
            report = self.root / f'{scope} report'
            report.write_text(scope)
            launch = run('dispatch', '--role', 'test', '--scope', scope, '--cwd', self.root,
                         '--report', report, '--task', task, '--', '/usr/bin/true')
            run('done', '--attempt', launch['attempt'], '--report', report, WFC_DISPATCH=launch['dispatch'])
            launches.append(launch)
        for launch, other in zip(launches, reversed(launches)):
            crossed = run('wait', '--scope', other['scope'], '--dispatch', launch['dispatch'], '--timeout', '0', rc=3)
            self.assertEqual(crossed, {'timedOut': True, 'messages': []})
        for launch in launches:
            msg, = run('wait', '--scope', launch['scope'], '--dispatch', launch['dispatch'], '--timeout', '0')['messages']
            self.assertEqual((msg['attempt'], Path(msg['report']).read_text()), (launch['attempt'], launch['scope']))
            run('close', launch['dispatch'])


class LostLaunchReceipt(unittest.TestCase):
    def exercise_breakpoint(self, outcome):
        exercise = runpy.run_path(str(CHECKER))['exercise']
        fixture = tempfile.TemporaryDirectory(prefix='receipt-fixture-')
        self.addCleanup(fixture.cleanup)
        base = Path(fixture.name)
        root = base / 'private'
        root.mkdir()
        prepared_case = outcome.startswith('prepared')
        delegate = None if prepared_case else base / 'delegation'
        args = SimpleNamespace(wfc=base / 'wfc', codex=base / 'codex', codex_home=base / 'profile',
                               model='fixture', elf_sha256='b' * 64, timeout=3)
        dispatch, attempt = 'd-a1', 'a-b1'
        ticket = dict(version=2, unit=dispatch, attempt=attempt,
                      generation='00000000-0000-4000-8000-000000000001',
                      nonce='00000000-0000-4000-8000-000000000002', capabilityHash='a' * 64)
        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        identity = {key: dict(boot=boot, pid=2**30 + index, start_ticks=10)
                    for index, key in enumerate(('guardian', 'service', 'kernel', 'worker'))}
        parent = delegate
        if prepared_case:
            ticket['startupProtocol'] = 1
            uid = os.geteuid()
            unit = f"wfc-bundled-{ticket['generation']}.service"
            parent = Path(f'/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice') / unit
            identity['userService'] = dict(uid=uid, unit=unit, invocation='a' * 32)
        identity['containment'] = dict(kind='cgroup-v2', device=1, inode=2,
                                       path=str(parent / 'wfc-00000000-0000-4000-8000-000000000003'))
        proof = dict(proves='execution-empty', state='startup-aborted' if prepared_case else 'closed',
                     identity=identity)
        calls = []
        row = {}
        refused_after_ack = outcome == 'refused-after-ack'
        message_id = 'm-' + 'a' * 32
        if refused_after_ack:
            group = Path(identity['containment']['path'])
            group.mkdir(parents=True)
            group_stat = group.stat()
            identity['containment'].update(device=group_stat.st_dev, inode=group_stat.st_ino)
            args.wfc.touch()
            pids = {str(identity[key]['pid']) for key in ('guardian', 'service', 'kernel', 'worker')}
            for pid in pids:
                proc = base / 'proc' / pid
                proc.mkdir(parents=True)
                (proc / 'exe').symlink_to(args.wfc)
            worker = base / 'proc' / str(identity['worker']['pid'])
            (worker / 'cwd').symlink_to(root / 'worker cwd')
            (worker / 'cgroup').write_text(f'0::/{group.relative_to(base)}\n')

            def fixture_path(path):
                path = Path(path)
                parts = path.parts
                if len(parts) >= 3 and parts[:2] == ('/', 'proc') and parts[2] in pids:
                    return (base / 'proc').joinpath(*parts[2:])
                return path

        def command(argv, **_):
            op, *values = argv[1:]
            if op == '--version':
                return 0, 'codex-cli controlled' if argv[0] != args.wfc else 'wfc 0 (Rust, fixture)'
            calls.append((op, *values) if op != 'dispatch' else (op,))
            if op == 'dispatch':
                options = dict(zip(values[::2], map(str, values[1::2])))
                row.update(dispatch=dispatch, attempt=attempt, scope=options['--scope'],
                           cwd=str(root / 'worker cwd'), task=options['--task'], report=options['--report'],
                           host='codex', backend='bundled', round=1, createPending=True, backendHandle=ticket,
                           backendResult='accepted', backendAlive=True,
                           backendObservation={'result': 'accepted', 'ticket': ticket, 'alive': True,
                                               'owned': True})
                if outcome == 'empty-output':
                    return 0, ''
                if outcome == 'conflicting-launch':
                    return 0, json.dumps(dict(dispatch=dispatch, attempt='a-b2'))
                if refused_after_ack:
                    row.update(createPending=False, role='reports', launchConfig=options['--launch-config'],
                               create={'result': 'accepted', 'ticket': ticket, 'identity': identity})
                    return 0, json.dumps(row)
                raise RuntimeError('smoke command timed out')
            if op == 'wait' and refused_after_ack:
                report = Path(row['report'])
                lines = [row['scope'].removeprefix('codex-smoke-'), row['cwd'], 'inherited', 'shared',
                         'host override', 'runtime argument', str(args.codex_home), str(args.wfc),
                         dispatch, attempt, args.elf_sha256]
                report.write_text('\n'.join(lines) + '\n')
                report.with_suffix('.done.json').write_text(
                    json.dumps(dict(message=1, messageId=message_id, duplicate=False)))
                delivered = {key: row[key] for key in ('dispatch', 'attempt', 'round', 'scope', 'role', 'report')}
                delivered.update(id=1, messageId=message_id, type='done', state='received')
                return 0, json.dumps({'messages': [delivered]})
            if op == 'ack' and refused_after_ack:
                return 0, json.dumps(dict(message=1, messageId=message_id, state='processed',
                                          statusVersion=3, duplicate=False))
            if op == 'status':
                if values:
                    self.assertEqual(values, ['--dispatch', dispatch])
                    if refused_after_ack:
                        refused = dict(row, backendResult='refused', backendAlive=None,
                                       backendObservation={'result': 'refused', 'ticket': ticket},
                                       messages=[dict(id=1, messageId=message_id, state='processed')])
                        return 0, json.dumps({'dispatches': [refused]})
                    closed = dict(row, backendResult='completed', backendAlive=False, backendObservation={
                        'result': 'completed', 'alive': False, 'ticket': ticket,
                        'cleanup': 'confirmed', 'proof': proof})
                    return 0, json.dumps({'dispatches': [closed]})
                if outcome == 'no-assignment':
                    return 0, '{"dispatches":[]}'
                if outcome == 'ambiguous':
                    return 0, json.dumps({'dispatches': [row, dict(row, dispatch='d-a2')]})
                if outcome == 'wrong-scope':
                    row['scope'] = 'another scope'
                if outcome == 'unknown-ticket':
                    row['backendObservation'] = {'result': 'uncertain'}
                if outcome == 'conflicting-ticket':
                    row['backendHandle'] = dict(ticket, generation='00000000-0000-4000-8000-000000000004')
                if outcome == 'refused':
                    row.update(createPending=False, create={'result': 'accepted', 'ticket': ticket,
                                                            'identity': identity},
                               backendResult='refused', backendAlive=None,
                               backendObservation={'result': 'refused', 'ticket': ticket,
                                                   'diagnostic': 'stale execution identity'})
                if outcome == 'uncertain-echo':
                    row.update(backendResult='uncertain', backendAlive=None,
                               backendObservation={'result': 'uncertain', 'ticket': ticket,
                                                   'cleanup': 'unknown'})
                if outcome == 'accepted-unowned':
                    del row['backendObservation']['owned']
                    del row['backendObservation']['alive']
                    row['backendAlive'] = None
                if prepared_case:
                    row.update(backendResult='uncertain', backendAlive=None,
                               backendObservation={'result': 'uncertain', 'ticket': ticket,
                                                   'startup': 'prepared', 'cleanup': 'unknown'})
                    if outcome != 'prepared-without-identity':
                        row['backendObservation']['startupIdentity'] = identity
                if outcome.startswith('terminal-'):
                    row.update(backendResult='completed', backendAlive=False,
                               backendObservation={'result': 'completed', 'ticket': ticket, 'alive': False,
                                                   'cleanup': 'confirmed'})
                    if outcome != 'terminal-without-proof':
                        row['backendObservation']['proof'] = proof
                return 0, json.dumps({'dispatches': [row]})
            self.assertEqual((op, values), ('close', [dispatch]))
            close_ticket = dict(ticket, attempt='a-b2') if outcome == 'wrong-close-proof' else ticket
            close_proof = dict(proof, state='exited') if outcome == 'terminal-changed-on-close' else proof
            return 0, json.dumps(dict(operation='close', result='completed', cleanup='confirmed',
                                      dispatch=dispatch, attempt=attempt, round=1, ticket=close_ticket,
                                      proof=close_proof))

        output = io.StringIO()
        overrides = {'run_command': command}
        if refused_after_ack:
            overrides.update(Path=fixture_path, CGROUP=base, same_process=lambda _: True,
                             worker_arguments_match=lambda *_: True)
        if prepared_case:
            # Cover the user-service proof without querying any real user manager.
            def retired_prepared(observed, group, service, service_group):
                self.assertEqual((observed, group, service, service_group),
                                 (identity, Path(identity['containment']['path']), unit, parent))
                return True
            overrides['retired'] = retired_prepared
        with patch.dict(exercise.__globals__, overrides), redirect_stdout(output):
            with self.assertRaises((RuntimeError, json.JSONDecodeError)):
                exercise(args, root, delegate, lambda: False)
        return calls, root, [json.loads(line) for line in output.getvalue().splitlines()]

    def test_bound_reservation_closes_once_without_a_launch_receipt(self):
        for outcome in ('timeout', 'empty-output', 'prepared', 'terminal-proof'):
            with self.subTest(outcome=outcome):
                calls, root, records = self.exercise_breakpoint(outcome)
                self.assertEqual(calls, [('dispatch',), ('status',), ('close', 'd-a1'),
                                         ('status', '--dispatch', 'd-a1')])
                self.assertFalse(root.exists())
                self.assertEqual(records[-1]['cleanup'], 'confirmed')

    def test_unknown_or_mismatched_reservation_is_retained_without_close(self):
        for outcome in ('no-assignment', 'ambiguous', 'wrong-scope', 'unknown-ticket',
                        'conflicting-ticket', 'conflicting-launch', 'uncertain-echo',
                        'accepted-unowned', 'prepared-without-identity', 'terminal-without-proof'):
            with self.subTest(outcome=outcome):
                calls, root, records = self.exercise_breakpoint(outcome)
                self.assertEqual(calls, [('dispatch',), ('status',)])
                self.assertTrue(root.is_dir())
                self.assertEqual(records[-1]['cleanup'], 'unconfirmed')
                self.assertTrue(records[-1]['reason'])

    def test_refused_ticket_echo_is_retained_without_close(self):
        calls, root, records = self.exercise_breakpoint('refused')
        self.assertEqual(calls, [('dispatch',), ('status',)])
        self.assertTrue(root.is_dir())
        self.assertEqual(records[-1]['cleanup'], 'unconfirmed')
        self.assertIn('refused', records[-1]['reason'])

    def test_refusal_observed_after_normal_ack_is_retained_without_close(self):
        calls, root, records = self.exercise_breakpoint('refused-after-ack')
        self.assertEqual([call[0] for call in calls], ['dispatch', 'wait', 'ack', 'status'])
        self.assertTrue(root.is_dir())
        self.assertEqual(records[-1]['cleanup'], 'unconfirmed')
        self.assertIn('refused', records[-1]['reason'])

    def test_mismatched_close_proof_is_retained_without_retry(self):
        for outcome in ('wrong-close-proof', 'terminal-changed-on-close'):
            with self.subTest(outcome=outcome):
                calls, root, records = self.exercise_breakpoint(outcome)
                self.assertEqual(calls, [('dispatch',), ('status',), ('close', 'd-a1')])
                self.assertTrue(root.is_dir())
                self.assertEqual(records[-1]['cleanup'], 'unconfirmed')


class VersionProbe(unittest.TestCase):
    def test_cancel_stops_its_version_query_group(self):
        run_command = runpy.run_path(str(CHECKER))['run_command']
        with tempfile.TemporaryDirectory(prefix='probe-fixture-') as tmp:
            root = Path(tmp)
            host = root / 'cancel-version'
            host.write_text(FIXTURE)
            host.chmod(0o700)
            marker = root / 'version'
            with self.assertRaisesRegex(RuntimeError, 'smoke interrupted'):
                run_command([host, '--version'], cwd=root, env={'PATH': '/usr/bin:/bin'},
                            timeout=3, cancel=marker.is_file, version_probe=True)
            self.assertTrue(wait_for_probe_exit(marker))


class WorkerArguments(unittest.TestCase):
    def test_compares_only_the_expected_startup_fields(self):
        match = runpy.run_path(str(CHECKER))['worker_arguments_match']
        with tempfile.TemporaryDirectory(prefix='argv-fixture-') as tmp:
            proc = Path(tmp)
            (proc / 'exe').symlink_to('/usr/bin/true')
            (proc / 'cmdline').write_bytes(
                b'/usr/bin/python3\0/private/Codex CLI\0-m\0fixture\0- a\0never\0plain (dispatch d-01)\0')
            allowed = ('/private/Codex CLI',)
            refs = ('(dispatch d-01)',)
            self.assertFalse(match(proc, allowed, ['-m', 'fixture', '-a', 'never'], refs))
            (proc / 'cmdline').write_bytes(
                b'/usr/bin/python3\0/private/Codex CLI\0-m\0fixture\0-a\0never\0plain (dispatch d-01)\0')
            self.assertTrue(match(proc, allowed, ['-m', 'fixture', '-a', 'never'], refs))
            self.assertFalse(match(proc, allowed, ['-m', 'fixture', '-a', 'never'], ('(dispatch d-02)',)))


if __name__ == '__main__':
    unittest.main()
