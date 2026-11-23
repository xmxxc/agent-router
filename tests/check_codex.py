"""Exercise the Linux Codex acceptance checker with a built wfc and fictional credentials."""
import ctypes
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

CHECKER = Path(__file__).resolve().parents[1] / 'tools/check_codex.py'
WFC = Path(sys.argv.pop(1)).resolve()
TMUX = shutil.which('tmux')
FIXTURE = '''#!/usr/bin/python3
import os, re, subprocess, sys, time
from pathlib import Path
root = Path(__file__).resolve().parent
key = (root / 'fictional-key').read_text()
mode = Path(__file__).resolve().name
auth = Path(os.environ['CODEX_HOME']) / 'auth.json'
assert key not in repr(sys.argv) and 'OPENAI_API_KEY' not in os.environ
if sys.argv[-2:] == ['login', '--with-api-key']:
    assert sys.stdin.read().strip() == key
    auth.write_text(key)
    (root / 'login').touch()
    print(key, file=sys.stderr)  # A failing login must not echo this to the checker.
    if mode == 'cancel-login':
        time.sleep(60)
    sys.exit(1 if mode == 'reject-login' else 0)
if sys.argv[1:] == ['--version']:
    if mode == 'cancel-copy':
        assert auth.read_text() == 'fictional profile'
        (root / 'copied').touch()
        time.sleep(60)
    print('codex-cli controlled')
    sys.exit()
assert auth.read_text() == (key if (root / 'login').exists() else 'fictional profile')
e = os.environ
task = Path(e['WFC_TASK']).read_text()
marker = re.search(r'"([0-9a-f]{24})"', task)[1]
Path(e['WFC_REPORT']).write_text('\\n'.join([marker if mode != 'wrong' else 'invalid', os.getcwd(),
    e['WFC_SAMPLE'], e['WFC_RUNTIME_SAMPLE'], e['WFC_DISPATCH'], e['WFC_ATTEMPT']]) + '\\n')
(root / 'running').touch()
if mode not in ('cancel-running', 'file-only'):
    subprocess.run([e['WFC_BIN'], 'done', '--report', e['WFC_REPORT'], '--attempt', e['WFC_ATTEMPT']], check=True)
time.sleep(60)
'''


class Checker(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix='c8-')
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.key = 'fictional-' + secrets.token_hex(12)
        (self.root / 'fictional-key').write_text(self.key)
        self.profile = self.root / 'profile'
        self.profile.write_text('fictional profile')
        self.env = dict(PATH=os.environ['PATH'], TMPDIR=tmp.name, HOME=tmp.name, LANG='C.UTF-8',
                        OPENAI_API_KEY=self.key)
        self.assertEqual(ctypes.CDLL(None).prctl(36, 1, 0, 0, 0), 0)  # observe any leaks
        self.addCleanup(self.reap_test_children)

    def reap_test_children(self):
        # On a regressed checker, reap only the test's own orphaned descendants.
        deadline = time.monotonic() + 8
        while True:
            self.assertLess(time.monotonic(), deadline, 'test descendant cleanup timed out')
            try:
                if os.waitpid(-1, os.WNOHANG)[0]:
                    continue
            except ChildProcessError:
                break
            for pid in Path('/proc/thread-self/children').read_text().split():
                os.kill(int(pid), signal.SIGKILL)
            time.sleep(.02)

    def run_case(self, mode, *, profile=False, cancel_at=None):
        host = self.root / mode
        host.write_text(FIXTURE)
        host.chmod(0o700)
        args = [sys.executable, CHECKER, WFC, '--codex', host, '--model', 'fixture',
                '--timeout', '2' if mode == 'file-only' else '12']
        if profile:
            args.extend(['--auth-file', self.profile])
        proc = subprocess.Popen(args, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            if cancel_at:
                deadline = time.monotonic() + 12
                while not (self.root / cancel_at).exists():
                    self.assertIsNone(proc.poll(), cancel_at)
                    self.assertLess(time.monotonic(), deadline, cancel_at)
                    time.sleep(.02)
                proc.terminate()
                # Repeat TERM while the parent is cleaning up.
                for _ in range(4):
                    if proc.poll() is not None:
                        break
                    time.sleep(.01)
                    proc.terminate()
            out, err = proc.communicate(timeout=22)
            self.assertNotIn(self.key, out + err)
            self.assertEqual(list(self.root.glob('wfc codex *')), [])
            self.assertEqual(self.profile.read_text(), 'fictional profile')
            with self.assertRaises(ChildProcessError):
                os.waitpid(-1, os.WNOHANG)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
        return proc.returncode, [json.loads(line) for line in out.splitlines()], err

    def test_api_login_and_received_report(self):
        rc, rows, _ = self.run_case('good')
        self.assertEqual(rc, 0)
        self.assertTrue((self.root / 'login').exists())
        message, = [r for r in rows if 'messageId' in r]
        self.assertEqual((message['state'], len(message['reportLines'])), ('received', 6))

    def test_login_failure_is_private(self):
        rc, _, err = self.run_case('reject-login')
        self.assertEqual(rc, 1)
        self.assertTrue((self.root / 'login').exists())
        self.assertIn('login', err)

    def test_wrong_body_and_missing_receipt(self):
        rc, _, err = self.run_case('wrong', profile=True)
        self.assertEqual(rc, 1)
        self.assertIn('report does not match', err)
        rc, rows, _ = self.run_case('file-only', profile=True)
        self.assertEqual(rc, 1)
        self.assertTrue(any(r.get('reportPresent') and r['waitExit'] == 3 and not r['messages'] for r in rows))
        self.assertFalse((self.root / 'login').exists())

    def test_terminate_after_copy(self):
        self.assertEqual(self.run_case('cancel-copy', profile=True, cancel_at='copied')[0], -signal.SIGTERM)

    def test_terminate_during_login(self):
        self.assertEqual(self.run_case('cancel-login', cancel_at='login')[0], -signal.SIGTERM)

    def test_terminate_with_live_worker(self):
        self.assertEqual(self.run_case('cancel-running', profile=True, cancel_at='running')[0], -signal.SIGTERM)

    def test_terminate_during_tmux_startup_and_shutdown(self):
        self.env['PATH'] = str(self.root) + os.pathsep + self.env['PATH']
        for operation in ('new-session', 'kill-server'):
            with self.subTest(operation=operation):
                shim = self.root / 'tmux'
                shim.write_text(f"""#!/usr/bin/python3
import os, subprocess, sys, time
from pathlib import Path
if {operation!r} in sys.argv:
    if 'new-session' in sys.argv:
        subprocess.run([{TMUX!r}, *sys.argv[1:]], check=True)
    Path({str(self.root / operation)!r}).touch()
    time.sleep(60)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
                shim.chmod(0o700)
                rc, _, _ = self.run_case('cancel-' + operation, profile=True, cancel_at=operation)
                self.assertEqual(rc, -signal.SIGTERM)

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
