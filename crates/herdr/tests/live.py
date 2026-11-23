"""Linux live contract. Pass the pinned Herdr binary and built herdr_unit example."""
import contextlib
import ctypes
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid


HERDR, DRIVER = map(os.path.abspath, sys.argv[1:3])
del sys.argv[1:3]


def wait_for(check, seconds=6):
    deadline = time.monotonic() + seconds
    while True:
        if result := check():
            return result
        if time.monotonic() >= deadline:
            raise AssertionError("timed out waiting for " + repr(check))
        time.sleep(.04)


def exchange(endpoint, request):
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(5)
        conn.connect(str(endpoint))
        conn.sendall(json.dumps(request).encode() + b'\n')
        with conn.makefile('rb') as stream:
            return stream.readline(2 * 1024 * 1024 + 1)


class HerdrContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='rust-herdr-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.dir = self.root / 'attempt'
        self.dir.mkdir(mode=0o700)
        self.endpoint = self.root / 'herdr.sock'
        self.server = None
        # Reap only children of this private harness, including orphaned PTYs.
        self.assertEqual(ctypes.CDLL(None).prctl(36, 1, 0, 0, 0), 0)  # PR_SET_CHILD_SUBREAPER
        self.addCleanup(self.cleanup_children)
        self.addCleanup(self.stop)
        self.start('first')

    def start(self, name):
        base = self.root / name
        base.mkdir(exist_ok=True)
        config = base / 'herdr.toml'
        config.write_text('[terminal]\ndefault_shell = "/bin/sh"\n')
        env = dict(PATH='/usr/bin:/bin', HOME=str(base), SHELL='/bin/sh', TERM='xterm-256color',
                   HERDR_SOCKET_PATH=str(self.endpoint), HERDR_CONFIG_PATH=str(config),
                   XDG_CONFIG_HOME=str(base / 'config'), XDG_STATE_HOME=str(base / 'state'),
                   XDG_DATA_HOME=str(base / 'data'), XDG_CACHE_HOME=str(base / 'cache'),
                   XDG_RUNTIME_DIR=str(base))
        with (base / 'log').open('wb') as log:
            self.server = subprocess.Popen([HERDR, 'server'], env=env, cwd=base, start_new_session=True,
                                           stdin=subprocess.DEVNULL, stdout=log, stderr=log)

        def ready():
            self.assertIsNone(self.server.poll(), (base / 'log').read_text()[-1800:])
            try:
                return self.api('ping')['protocol'] == 22
            except (FileNotFoundError, ConnectionRefusedError):
                return False
        wait_for(ready)

    def stop(self):
        if self.server is not None:
            self.server.terminate()
            try:
                self.server.wait(timeout=6)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.wait()
            self.server = None

    def cleanup_children(self):
        children = Path(f'/proc/self/task/{os.getpid()}/children')
        def reaped():
            for pid in map(int, children.read_text().split()):
                if os.waitpid(pid, os.WNOHANG)[0] == 0:
                    os.kill(pid, signal.SIGKILL)
            return not children.read_text().strip()
        wait_for(reaped)

    def api(self, method, **params):
        req = {'id': str(uuid.uuid4()), 'method': method, 'params': params}
        response = json.loads(exchange(self.endpoint, req))
        self.assertEqual(response['id'], req['id'])
        self.assertNotIn('error', response)
        return response['result']

    def driver(self, op, expected='accepted', **params):
        p = subprocess.run([DRIVER, self.dir, op], input=json.dumps(params), capture_output=True, text=True,
                           env={'PATH':'/usr/bin:/bin'}, timeout=25)
        if expected == 'pending':
            self.assertNotEqual(p.returncode, 0)
            self.assertIn('input still pending', p.stderr)
            return
        self.assertTrue(p.stdout, p.stderr)
        result = json.loads(p.stdout)
        self.assertEqual(result['result'], expected, result)
        self.assertEqual(p.returncode, 0 if expected in ('accepted', 'completed') else 1)
        return result

    def state(self):
        return json.loads((self.dir / 'unit.json').read_text())

    def create(self, endpoint=None, command=None):
        worker = self.root / '受控 worker.py'
        worker.write_text('''import os, signal, sys
from pathlib import Path
def interrupted(*args):
    print("INTERRUPTED", flush=True)
signal.signal(signal.SIGINT, interrupted)
print("READY", os.getpid(), flush=True)
for line in sys.stdin:
    with Path("events").open("a") as log:
        print(line.rstrip(), file=log)
    print("RECEIVED", flush=True)
''')
        self.driver('create', endpoint=str(endpoint or self.endpoint), attempt='a-contract',
                    launch={'cwd':str(self.root),'label':'rust worker','command':command or [sys.executable, '-u', str(worker)]})
        return self.state()['handle']

    def mapping(self, handle, **updates):
        return dict(dict(workspace=handle['workspace'], pane=handle['pane'], terminal=handle['terminal'],
                         actor='tester', evidence='checked isolated snapshot'), **updates)

    def test_independent_clients_input_interrupt_sibling_close(self):
        h = self.create()
        self.assertEqual(h['service']['pid'], self.server.pid)
        self.assertTrue(h['service']['creation'].startswith('linux:'))
        pid = h['writerProcesses'][0]['pid']
        wait_for(lambda: f'READY {pid}' in self.driver('read')['screen']['text'])
        self.assertTrue(self.driver('inspect')['alive'])
        self.api('pane.split', target_pane_id=h['pane'], direction='right', focus=False)
        panes = self.api('session.snapshot')['snapshot']['panes']
        sibling, = [p for p in panes if p['workspace_id']==h['workspace'] and p['pane_id']!=h['pane']]
        sibling_pid = self.api('pane.process_info',pane_id=sibling['pane_id'])['process_info']['shell_pid']
        self.driver('submit',text='hello 世界')
        events = self.root/'events'
        wait_for(lambda: events.exists() and events.read_text().splitlines() == ['hello 世界'])
        self.api('pane.report_agent',pane_id=h['pane'],source='contract',agent='fixture',state='blocked')
        self.driver('submit','refused',text='blocked')
        self.driver('interrupt')
        wait_for(lambda: 'INTERRUPTED' in self.driver('read')['screen']['text'])
        self.driver('herdr-reconcile','refused',**self.mapping(h, actor=' '))
        self.driver('herdr-reconcile','refused',**self.mapping(h, evidence=' '))
        self.driver('herdr-reconcile','refused',**self.mapping(h, terminal='other',old_writer_evidence='checked'))
        self.driver('herdr-reconcile',**self.mapping(h))
        self.assertEqual(self.state()['audit'][-1]['inputReplayed'],False)
        self.driver('close','completed')
        self.assertFalse(self.driver('inspect')['alive'])
        self.driver('herdr-reconcile','refused',**self.mapping(h))
        stopped = self.driver('herdr-reconcile',**self.mapping(h,old_writer_evidence='observed writer exit and absent pane'))
        self.assertEqual(stopped['audit']['state'],'not-started')
        self.assertFalse(self.driver('inspect')['alive'])
        self.assertEqual(self.api('pane.process_info',pane_id=sibling['pane_id'])['process_info']['shell_pid'],sibling_pid)
        os.kill(sibling_pid,0)
        self.assertEqual(events.read_text().splitlines(), ['hello 世界'])

    def test_restart_and_explicit_binding(self):
        h = self.create()
        self.stop()
        self.start('restarted')
        self.driver('inspect','uncertain')
        self.driver('submit','uncertain',text='never')
        self.assertEqual(self.state()['handle'],h)
        # Reproduce a matching textual ID on a new service with a different nonce.
        old_dir = self.dir
        self.dir = self.root/'replacement'
        self.dir.mkdir(mode=0o700)
        replacement = self.create()
        self.dir = old_dir
        self.assertEqual((replacement['workspace'],replacement['pane']), (h['workspace'],h['pane']))
        self.assertNotEqual(replacement['nonce'], h['nonce'])
        mapping = self.mapping(replacement,old_writer_evidence='old isolated server was reaped')
        self.driver('herdr-reconcile','refused',**mapping)
        self.assertEqual(self.state()['handle'],h)
        # Controlled positive fixture: restore the original metadata exactly.
        self.api('workspace.report_metadata', workspace_id=h['workspace'],source='wfc',
                 tokens={'wfc_owner':h['nonce'],'wfc_attempt':h['attempt']})
        for missing in [None, '  ']:
            self.driver('herdr-reconcile','refused',**dict(mapping,old_writer_evidence=missing))
        self.driver('herdr-reconcile',**mapping)
        state = self.state()
        self.assertTrue(state['pendingInput'])
        self.assertEqual(len(state['audit']),1)
        self.assertEqual(state['handle']['attempt'],h['attempt'])
        self.assertEqual(state['handle']['service']['pid'],self.server.pid)
        self.assertTrue(self.driver('inspect')['alive'])
        self.assertFalse((self.root/'events').exists())
        self.driver('close','completed')

    def test_restart_same_state_preserves_unknown(self):
        h = self.create()
        self.stop()
        self.start('first')
        self.driver('inspect','uncertain')
        self.assertEqual(self.state()['handle'],h)
        self.assertFalse((self.root/'events').exists())

    def test_paused_worker_accepts_no_input(self):
        notice = 'This conversation is unavailable. Its cached transcript and draft remain here; input is paused.'
        self.create(command=[sys.executable,'-u','-c',f'print({notice!r}); input()'])
        wait_for(lambda: 'input is paused' in self.driver('read')['screen']['text'])
        self.driver('submit','refused',text='do not send')
        self.assertTrue(self.driver('inspect')['alive'])
        self.driver('close','completed')

    @unittest.skipUnless(os.environ.get('WFC_HERDR_TEST_CODEX'),'optional real Codex binary')
    def test_real_codex_first_screen(self):
        # Codex rejects helper directories under /tmp. Keep its empty home in
        # the calling workspace so the login probe is independent of that guard.
        home = tempfile.TemporaryDirectory(prefix='.herdr-codex-', dir=Path.cwd())
        self.addCleanup(home.cleanup)
        (Path(home.name)/'codex').mkdir(mode=0o700)
        # Retain the final PTY frame if the CLI exits during initialization.
        wrapper = 'import subprocess,sys,time; print("Codex exit",subprocess.call(sys.argv[1:]),flush=True); time.sleep(45)'
        self.create(command=['/usr/bin/env', f'HOME={home.name}',f'CODEX_HOME={home.name}/codex',
                             sys.executable,'-u','-c',wrapper,os.environ['WFC_HERDR_TEST_CODEX'],'--no-daemon','--no-alt-screen'])
        def login():
            text = self.driver('read')['screen']['text']
            return any(s in text.lower() for s in ('sign in', 'choose how you want to use codex'))
        try:
            wait_for(login,30)
        except AssertionError:
            self.fail(self.driver('read')['screen']['text'][-1200:])
        self.driver('close','completed')

    @contextlib.contextmanager
    def fault_proxy(self, change):
        endpoint = self.root / 'proxy.sock'
        errors = []
        stop = threading.Event()
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(endpoint))
            endpoint.chmod(0o600)
            server.listen()
            server.settimeout(.1)
            def serve():
                try:
                    while not stop.is_set():
                        try:
                            conn, _ = server.accept()
                        except socket.timeout:
                            continue
                        with conn, conn.makefile('rb') as stream:
                            req = json.loads(stream.readline())
                            response = json.loads(exchange(self.endpoint,req))
                            reply = change(req,response)
                            if reply is not None:
                                conn.sendall(json.dumps(reply).encode()+b'\n')
                except Exception as err:
                    errors.append(err)
            thread = threading.Thread(target=serve)
            thread.start()
            try:
                yield endpoint
            finally:
                stop.set()
                thread.join(7)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors,[])

    def test_lost_input_response_stays_single_after_reconcile(self):
        sends = []
        def lose(req, reply):
            if req['method'] in ('pane.send_input','pane.send_keys'):
                sends.append(req['method'])
                return None
            return reply
        with self.fault_proxy(lose) as endpoint:
            h = self.create(endpoint)
            self.driver('submit','uncertain',text='one input',host='codex')
            self.driver('herdr-reconcile',**self.mapping(h))
            self.driver('submit','pending',text='one input',host='codex')
            self.assertEqual(sends,['pane.send_input'])
            events = self.root/'events'
            wait_for(lambda: events.exists() and events.read_text().splitlines() == ['one input'])
            self.driver('close','completed')

    def test_followup_enter_lost_after_worker_submits(self):
        # A controlled raw terminal drops the initial grouped Enter and shows
        # the draft. Only the separate Enter commits one host event.
        host = self.root / 'composer.py'
        host.write_text(r'''import os, sys, tty
from pathlib import Path
tty.setraw(0)
draft = b''
def draw():
    os.write(1,b'\x1b[2J\x1b[H'+'› '.encode()+draft+b'\r\n\r\n  fixture '+ '·'.encode()+b' model\r\n')
draw()
dropped = False
while True:
    key = os.read(0,1)
    if key in (b'\r',b'\n'):
        if not dropped:
            dropped = True
        else:
            with Path('events').open('ab') as f:
                f.write(draft+b'\n')
            draft = b''
        draw()
    else:
        draft += key
        draw()
''')
        writes = []
        def lose_key(req, reply):
            if req['method'] in ('pane.send_input','pane.send_keys'):
                writes.append(req['method'])
                if req['method'] == 'pane.send_keys':
                    return None
            return reply
        with self.fault_proxy(lose_key) as endpoint:
            self.create(endpoint, [sys.executable,'-u',str(host)])
            wait_for(lambda: 'fixture' in self.driver('read')['screen']['text'])
            self.driver('submit','uncertain',text='one draft',host='codex')
            self.driver('submit','pending',text='one draft',host='codex')
            self.assertEqual(writes,['pane.send_input','pane.send_keys'])
            events = self.root/'events'
            wait_for(lambda: events.exists() and events.read_text().splitlines() == ['one draft'])
            self.driver('close','completed')

    def test_close_requires_every_checkpointed_writer_to_exit(self):
        extra = subprocess.Popen(['/bin/sleep','60'])
        try:
            def with_writer(req, reply):
                if req['method'] == 'pane.process_info':
                    info = reply['result']['process_info']
                    info.setdefault('foreground_processes', []).append({'pid':extra.pid})
                return reply
            with self.fault_proxy(with_writer) as endpoint:
                self.create(endpoint)
                self.driver('close','uncertain')
                self.driver('inspect','uncertain')
                self.assertIsNone(extra.poll())
                extra.terminate()
                extra.wait(timeout=3)
                self.driver('close','completed')
        finally:
            extra.kill()
            extra.wait()

    def test_wrong_protocol_and_partial_writer(self):
        mode = 'protocol'
        def change(req, reply):
            if mode == 'protocol' and req['method'] == 'ping':
                reply['result']['protocol'] = 21
            if mode == 'partial' and req['method'] == 'pane.process_info':
                reply['result']['process_info']['foreground_processes'] = [{'pid':4294967295}]
            if mode == 'missing members' and req['method'] == 'pane.process_info':
                info = reply['result']['process_info']
                info['foreground_process_group_id'] = info['shell_pid']
                info.pop('foreground_processes',None)
            return reply
        with self.fault_proxy(change) as endpoint:
            self.driver('create','refused',endpoint=str(endpoint),attempt='a-contract',
                        launch={'cwd':str(self.root),'label':'refused','command':['true']})
            mode = 'pass'
            h = self.create(endpoint)
            for mode in ['partial','missing members']:
                self.driver('herdr-reconcile','uncertain',**self.mapping(h))
                self.driver('close','uncertain')
            self.assertEqual(self.state()['handle'],h)
            mode = 'pass'
            self.driver('close','completed')


if __name__ == '__main__':
    unittest.main(verbosity=2)
