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
        self.stop()
        self.assertFalse(self.driver('inspect')['alive'])

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
        self.driver('inspect','uncertain')
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
    def fault_proxy(self, change, *, read_frame=None, respond_before_forward=None):
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
                            frame = read_frame(stream) if read_frame else stream.readline()
                            if frame is None:
                                continue
                            req = json.loads(frame)
                            reply = respond_before_forward(req) if respond_before_forward else None
                            if reply is None:
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

    def test_disconnect_during_input_request_keeps_claim(self):
        prefixes = []

        def cut_input(stream):
            prefix = stream.readline(512)
            if b'x' * 64 in prefix and not prefix.endswith(b'\n'):
                prefixes.append(prefix)
                return None
            return prefix if prefix.endswith(b'\n') else prefix + stream.readline()

        with self.fault_proxy(lambda req, reply: reply, read_frame=cut_input) as endpoint:
            self.create(endpoint)
            text = 'x' * (1024 * 1024)
            self.driver('submit','uncertain',text=text,host='codex')
            self.assertTrue(self.state()['pendingInput'])
            self.driver('submit','pending',text=text,host='codex')
            self.assertEqual(len(prefixes),1)
            self.assertEqual(len(prefixes[0]),512)
            self.assertIn(b'pane.send_input',prefixes[0])
            self.assertFalse((self.root/'events').exists())
            self.driver('close','completed')

    def test_compact_empty_composer_releases_confirmed_input(self):
        writes = []
        def trace(req, reply):
            if req['method'] in ('pane.send_input','pane.send_keys'):
                writes.append((req['method'],req['params'].get('text'),reply['result']['type']))
            return reply

        with self.fault_proxy(trace) as endpoint:
            host = Path(__file__).with_name('composer.py').resolve()
            self.create(endpoint,[sys.executable,'-u',str(host),'two','--submit-first'])
            (self.root/'show-history').touch()
            frame = '›\n  model · dir\n  keys · help'
            def empty_box():
                return self.driver('read')['screen']['text'].rstrip().endswith(frame)
            wait_for(empty_box)
            events = self.root/'events'
            received = []
            for text in ['hello from review','next message']:
                self.driver('submit',text=text,host='codex')
                received.append(text)
                self.assertEqual(writes,[('pane.send_input',line,'ok') for line in received])
                wait_for(lambda: events.exists() and events.read_text().splitlines() == received)
                self.assertFalse(self.state()['pendingInput'])
                wait_for(empty_box)

            # With the same frame, one plausible draft now equals the input.
            text = 'model · dir'
            self.driver('submit','uncertain',text=text,host='codex')
            self.assertTrue(self.state()['pendingInput'])
            self.driver('submit','pending',text=text,host='codex')
            self.assertEqual(writes,[('pane.send_input',line,'ok') for line in [*received,text]])
            wait_for(lambda: events.exists() and events.read_text().splitlines() == [*received,text])
            self.driver('close','completed')

    def test_long_history_with_two_footer_lines(self):
        self.long_history_submit(lose_key=False, footer='two')

    def test_long_history_with_one_footer_line(self):
        self.long_history_submit(lose_key=False, footer='one')

    def test_followup_enter_lost_after_worker_submits(self):
        self.long_history_submit(lose_key=True, footer='two')

    def long_history_submit(self, lose_key, footer):
        # The raw terminal drops the first Enter. The second is a separate
        # key, and only that key commits the draft as a worker event.
        host = Path(__file__).with_name('composer.py').resolve()
        calls = []
        def trace(req, reply):
            method = req['method']
            if method in ('pane.read','pane.send_input','pane.send_keys'):
                calls.append(method)
            if lose_key and method == 'pane.send_keys':
                return None
            return reply
        with self.fault_proxy(trace) as endpoint:
            self.create(endpoint, [sys.executable,'-u',str(host),footer])
            (self.root/'show-history').touch()
            def complete():
                screen = self.driver('read')['screen']
                return screen['truncated'] and 'model · dir' in screen['text']
            wait_for(complete)
            calls.clear()
            text = 'one draft tail · value'
            self.driver('submit','uncertain' if lose_key else 'accepted',text=text,host='codex')
            expected = ['pane.read','pane.send_input','pane.read','pane.read','pane.send_keys']
            if lose_key:
                self.driver('submit','pending',text=text,host='codex')
            else:
                expected.extend(['pane.read','pane.read'])
            self.assertEqual(calls,expected)
            events = self.root/'events'
            wait_for(lambda: events.exists() and events.read_text().splitlines() == [text])
            self.driver('close','completed')

    def test_paired_samples_and_followup_limit(self):
        # Script only the pane.read frames; count all writes to the real pane.
        frames = []
        calls = []
        def render(req, reply):
            if req['method'] in ('pane.read','pane.send_input','pane.send_keys'):
                calls.append(req['method'])
            if req['method'] == 'pane.read' and frames:
                read = reply['result']['read']
                read.update(text=frames.pop(0),truncated=True)
            return reply
        exact = '› draft\n  fixture · model'
        with self.fault_proxy(render) as endpoint:
            self.create(endpoint)
            empty = '›\n\n  fixture · model'
            ambiguous = '› draft\n  tail · value\n  model · dir'
            for pair in [(empty,ambiguous), (ambiguous,empty), (exact,'› changed\n  fixture · model'), (exact, '› draft ↓\n  fixture · model'),
                         ('› draft ↑\n  fixture · model',exact),
                         ('› draft\n  extra · text\n  model · dir\n  keys · info',)*2]:
                calls.clear()
                frames[:] = ['›\n\n  model · dir\n  keys · info',*pair]
                self.driver('submit',text='draft',host='codex')
                self.assertEqual(calls,['pane.read','pane.send_input','pane.read','pane.read'])
                self.assertFalse(self.state()['pendingInput'])
            calls.clear()
            frames[:] = ['›\n\n  fixture · model'] + ['› draft\n  tail · value\n  fixture · model']*2
            self.driver('submit',text='draft',host='codex')
            self.assertNotIn('pane.send_keys',calls)
            calls.clear()
            frames[:] = ['',*([exact]*6)]
            self.driver('submit','uncertain',text='draft',host='codex')
            self.assertEqual(calls,['pane.read','pane.send_input'] +
                             ['pane.read','pane.read','pane.send_keys']*2 + ['pane.read','pane.read'])
            self.driver('submit','pending',text='draft',host='codex')
            self.driver('close','completed')

    def test_close_tracks_writer_seen_by_inspect(self):
        self.track_background_writer('inspect')

    def test_close_tracks_writer_seen_by_submit(self):
        self.track_background_writer('submit')

    def track_background_writer(self, observe):
        h = self.create(command=[sys.executable,str(Path(__file__).with_name('job_control.py').resolve())])
        self.api('pane.split', target_pane_id=h['pane'], direction='right', focus=False)
        siblings = [p for p in self.api('session.snapshot')['snapshot']['panes']
                    if p['workspace_id'] == h['workspace'] and p['pane_id'] != h['pane']]
        sibling, = siblings
        sibling_pid = self.api('pane.process_info',pane_id=sibling['pane_id'])['process_info']['shell_pid']

        (self.root/'start-job').touch()
        job_file = self.root/'job-pid'
        pid = int(wait_for(lambda: job_file.exists() and job_file.read_text()))
        def foreground():
            info = self.api('pane.process_info',pane_id=h['pane'])['process_info']
            return {p['pid'] for p in info.get('foreground_processes', [])}
        wait_for(lambda: pid in foreground())
        self.assertNotIn(pid,[p['pid'] for p in h['writerProcesses']])
        if observe == 'inspect':
            self.assertTrue(self.driver('inspect')['alive'])
        else:
            self.driver('submit',text='fixture input')
        self.assertIn(pid,[p['pid'] for p in self.state()['handle']['writerProcesses']])

        (self.root/'background-job').touch()
        wait_for(lambda: (self.root/'backgrounded').exists() and pid not in foreground())
        if observe == 'submit':
            self.driver('herdr-reconcile',**self.mapping(h))
            self.assertIn(pid,[p['pid'] for p in self.state()['handle']['writerProcesses']])
        self.driver('close','uncertain')
        self.driver('inspect','uncertain')
        os.kill(pid,0)
        self.assertEqual(self.api('pane.process_info',pane_id=sibling['pane_id'])['process_info']['shell_pid'],sibling_pid)
        (self.root/'stop-job').touch()
        self.driver('close','completed')
        self.assertFalse(self.driver('inspect')['alive'])
        os.kill(sibling_pid,0)

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

    def test_partial_input_observation_retains_identified_writer(self):
        self.partial_observation('submit')

    def test_partial_reconcile_retains_identified_writer(self):
        self.partial_observation('herdr-reconcile')

    def partial_observation(self, operation):
        mode = 'pass'
        with subprocess.Popen(['/bin/sleep','30']) as writer:
            def partial(req, reply):
                if mode == 'partial' and req['method'] == 'pane.process_info':
                    reply['result']['process_info']['foreground_processes'] = [
                        {'pid':4294967295}, {'pid':writer.pid}]
                return reply
            try:
                with self.fault_proxy(partial) as endpoint:
                    h = self.create(endpoint)
                    mode = 'partial'
                    if operation == 'submit':
                        self.driver('submit','uncertain',text='held')
                        self.assertTrue(self.state()['pendingInput'])
                    else:
                        self.driver('herdr-reconcile','uncertain',**self.mapping(h))
                        self.assertEqual(self.state()['audit'],[])
                        self.assertEqual(self.state()['handle']['service'],h['service'])
                    self.assertFalse((self.root/'events').exists())
                    self.assertIn(writer.pid,[p['pid'] for p in self.state()['handle']['writerProcesses']])
                    mode = 'pass'
                    self.driver('close','uncertain')
                    writer.terminate()
                    writer.wait(timeout=3)
                    self.driver('close','completed')
            finally:
                writer.kill()

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
