"""Use the downloaded WFC and a controlled Codex stand-in with a fictional profile."""
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

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
        (root / 'version').touch()
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

    def test_original_done_ack_and_close(self):
        rc, rows, err = self.run_case('good')
        self.assertEqual(rc, 0, err)
        message, = [r for r in rows if 'messageId' in r]
        self.assertEqual((message['state'], message['statusVersion']), ('processed', 3))
        self.assertGreater(message['message'], 0)
        self.check_close(rows)
        self.assertEqual(rows[-1], {'result': 'BUNDLED_CODEX_SMOKE_PASS', 'binarySha256': ELF_SHA256})

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


if __name__ == '__main__':
    unittest.main()
