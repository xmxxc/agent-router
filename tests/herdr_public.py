"""Linux integration: python3 tests/herdr_public.py HERDR WFC [-v]."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

# Both suites use the same isolated Herdr server, fault proxy and child reaper.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'crates/herdr/tests'))
import live


class PublicLifecycle(unittest.TestCase):
    def setUp(self):
        self.service = live.HerdrContract()
        self.addCleanup(lambda: self.assertTrue(self.service.doCleanups(), "isolated Herdr cleanup failed"))
        self.service.setUp()
        self.root = self.service.root
        self.env = dict(PATH='/usr/bin:/bin', HOME=str(self.root), WFC_STATE=str(self.root/'router'),
                        WFC_HERDR_ENDPOINT=str(self.service.endpoint), WFC_CONTROLLER_TOKEN='test-secret',
                        HERDR_PANE_ID='stale-pane', TMUX_PANE='stale-tmux-pane')
        self.config = self.root/'config.json'
        self.task = self.root/'task.md'
        self.task.write_text('record this assignment')
        self.worker = self.root/'worker script.py'
        self.worker.write_text('''import os, sys, json, subprocess
from pathlib import Path
keys = ("WFC_DISPATCH","WFC_ROUND","WFC_ATTEMPT","WFC_TASK","WFC_STATE","WFC_REPORT","WFC_BIN","WFC_CONTROLLER_TOKEN","HERDR_PANE_ID","HERDR_WORKSPACE_ID","TMUX_PANE")
Path("assignment").write_text(json.dumps({k:os.getenv(k) for k in keys}))
for line in sys.stdin:
    with Path("events").open("a") as f:
        print(line.rstrip(), file=f)
    if line.rstrip() == "finish":
        Path(os.environ["WFC_REPORT"]).write_text("finished")
        subprocess.run([os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                        "--attempt", os.environ["WFC_ATTEMPT"]], check=True)
''')

    def cli(self, *args, error=None, payload=None):
        p = subprocess.run([live.DRIVER, *map(str,args)], env=self.env, text=True,
                           input=json.dumps(payload) if payload else None, capture_output=True, timeout=25)
        if error is not None:
            self.assertNotEqual(p.returncode, 0, p.stdout)
            self.assertIn(error, p.stderr + p.stdout)
            return p.stderr + p.stdout
        self.assertEqual(p.returncode, 0, p.stderr)
        return json.loads(p.stdout)

    def dispatch(self, endpoint=None, script=None, host="codex", command=None, error=None):
        self.config.write_text(json.dumps({'hosts':{host:{'command':command or [sys.executable, '-u', str(script or self.worker)]}}}))
        opts = ['--herdr-endpoint', endpoint] if endpoint else []
        self.row = self.cli('dispatch', '--host', host, '--backend', 'herdr', '--task', self.task,
                            '--cwd', self.root, '--report', self.root/'report.md', '--launch-config', self.config, *opts, error=error)
        if error:
            self.row, = self.cli('status')['dispatches']
        self.dispatch_id = self.row['dispatch']
        self.h = self.row['backendHandle']
        self.assertEqual(self.row['attempt'], self.h['attempt'])

    def status(self):
        return self.cli('status', '--dispatch', self.dispatch_id)['dispatches'][0]

    def submit(self, text, error=None, **changes):
        payload = {k:self.row[k] for k in ('dispatch', 'round', 'attempt')}
        self.cli('--herdr-backend', payload=dict(payload, op='submit', text=text) | changes, error=error)

    def reconcile(self, error=None, **opts):
        flags = dict(pane=self.h['pane'], terminal=self.h['terminal'], actor='operator', evidence='inspected unit') | opts
        args = [s for k,v in flags.items() for s in ('--'+k.replace('_','-'), v)]
        return self.cli('herdr-reconcile', self.dispatch_id, *args, error=error)

    def unconfirmed_close(self, diagnostic):
        reply = self.cli('close', self.dispatch_id)
        self.assertEqual(reply['result'], 'uncertain')
        self.assertEqual([reply[k] for k in ('dispatch','round','attempt')],
                         [self.row[k] for k in ('dispatch','round','attempt')])
        self.assertIn(diagnostic, reply['diagnostic'])

    def sibling(self):
        self.service.api('pane.split', target_pane_id=self.h['pane'], direction='right', focus=False)
        panes = self.service.api('session.snapshot')['snapshot']['panes']
        other, = [p['pane_id'] for p in panes if p['workspace_id']==self.h['workspace'] and p['pane_id']!=self.h['pane']]
        pid = self.service.api('pane.process_info', pane_id=other)['process_info']['shell_pid']
        return other, pid

    def tmux_handoff_fixture(self, *, hold=False, scope=None, backend='tmux',
                             endpoint=None, detached=False):
        socket = self.root / 'handoff-tmux.sock'
        self.env['WFC_TMUX_SOCKET'] = str(socket)
        tmux = ['tmux', '-f', '/dev/null', '-S', str(socket)]
        subprocess.run(tmux + ['new-session', '-d', '-s', 'sibling', '/bin/cat'],
                       env=self.env, check=True, capture_output=True, timeout=5)
        self.addCleanup(subprocess.run, tmux + ['kill-server'], env=self.env,
                        capture_output=True, timeout=5)
        traces = self.root / 'handoff-workers'
        traces.mkdir()
        self.worker.write_text('''import json, os, signal, subprocess, sys, time
from pathlib import Path
if os.getenv("HANDOFF_TEST_HOLD"):
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
attempt = os.environ["WFC_ATTEMPT"]
keys = ("WFC_DISPATCH", "WFC_ROUND", "WFC_ATTEMPT", "WFC_SCOPE",
        "WFC_REPORT", "WFC_TASK", "WFC_WORKER_CALLBACK")
trace = {"callback": {key: os.environ[key] for key in keys},
         "tmuxPane": os.getenv("TMUX_PANE"), "herdrPane": os.getenv("HERDR_PANE_ID"),
         "pid": os.getpid(),
         "startTicks": int(Path("/proc/self/stat").read_bytes().rsplit(b")", 1)[1].split()[19])}
directory = Path("handoff-workers")
if os.getenv("HANDOFF_TEST_DETACHED") and trace["herdrPane"]:
    script = """import time
from pathlib import Path
deadline = time.monotonic() + 20
while not Path("handoff-workers/release").exists() and time.monotonic() < deadline:
    time.sleep(.025)
"""
    descendant = subprocess.Popen([sys.executable, "-c", script],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True)
    trace["detached"] = {"pid": descendant.pid, "startTicks": int(
        Path(f"/proc/{descendant.pid}/stat").read_bytes().rsplit(b")", 1)[1].split()[19])}
check = directory / "reverse-check"
if trace["tmuxPane"] and check.exists():
    source = json.loads(check.read_bytes())
    record = json.loads(Path(source["record"]).read_bytes())
    marker = json.loads(Path(source["stop"]).read_bytes())
    close, = [event for event in record["audit"] if event["operation"] == "close"]
    try:
        fields = Path(f'/proc/{source["pid"]}/stat').read_bytes().rsplit(b")", 1)[1].split()
        old_alive = fields[0] not in (b"Z", b"X", b"x") and int(fields[19]) == source["startTicks"]
    except FileNotFoundError:
        old_alive = False
    trace["sourceStoppedBeforeTarget"] = (not old_alive and marker["writerTreeStopped"]
        and close["result"] == "completed" and close["attempt"] == marker["attempt"])
tmp = directory / (attempt + ".tmp")
tmp.write_text(json.dumps(trace))
tmp.rename(directory / (attempt + ".json"))
deadline = time.monotonic() + 45
while not (directory / (attempt + ".quit")).exists() and time.monotonic() < deadline:
    time.sleep(.025)
''')
        env = {}
        if hold:
            env['HANDOFF_TEST_HOLD'] = '1'
        if detached:
            env['HANDOFF_TEST_DETACHED'] = '1'
        self.config.write_text(json.dumps({'hosts': {
            'codex': {'command': [sys.executable, '-u', str(self.worker)], 'env': env},
        }}))
        flags = ('--scope', scope) if scope is not None else ()
        if endpoint is not None:
            flags += ('--herdr-endpoint', endpoint)
        row = self.cli('dispatch', '--host', 'codex', '--backend', backend, '--task', self.task,
                       '--cwd', self.root, '--report', self.root/'report.md',
                       '--launch-config', self.config, *flags)
        self.dispatch_id = row['dispatch']
        self.handoff_traces = traces
        original = self.handoff_trace(row['attempt'])

        def stop_workers():
            (traces / 'release').touch()
            for trace in traces.glob('*.json'):
                (traces / (trace.stem + '.quit')).touch()
            workers = [json.loads(trace.read_bytes()) for trace in traces.glob('*.json')]
            workers += [worker['detached'] for worker in workers if 'detached' in worker]
            live.wait_for(lambda: all(not self.handoff_worker_alive(worker) for worker in workers))

        self.addCleanup(stop_workers)
        return row, original

    def handoff_trace(self, attempt):
        trace = self.handoff_traces / (attempt + '.json')
        return live.wait_for(lambda: json.loads(trace.read_bytes()) if trace.is_file() else None)

    def handoff_worker_alive(self, trace):
        try:
            stat = Path(f'/proc/{trace["pid"]}/stat').read_bytes()
        except (FileNotFoundError, ProcessLookupError):
            return False
        fields = stat.rsplit(b')', 1)[1].split()
        return fields[0] not in (b'Z', b'X', b'x') and int(fields[19]) == trace['startTicks']

    def target_handoff(self, endpoint):
        return ('handoff', self.dispatch_id, '--host', 'codex', '--backend', 'herdr',
                '--herdr-endpoint', endpoint)

    def handoff_source_evidence(self, source):
        state = Path(self.env['WFC_STATE'])
        archive = state / 'dispatches' / f'{source["dispatch"]}.{source["attempt"]}.closed'
        stopped = Path(source['create']['receipt']).with_suffix('.stopped')
        return archive, stopped

    def test_tmux_to_herdr_handoff_preserves_original_receipts_and_launches_once(self):
        source, old = self.tmux_handoff_fixture()
        archive, stopped = self.handoff_source_evidence(source)
        worker_env = {k:v for k,v in self.env.items() if k != 'WFC_CONTROLLER_TOKEN'} | old['callback']
        question = subprocess.check_output([live.DRIVER, 'ask', 'original', '--attempt', source['attempt']],
                                           env=worker_env, text=True, timeout=6)
        question = json.loads(question)
        assignment = Path(self.env['WFC_STATE']) / 'dispatches' / f'{self.dispatch_id}.json'
        original = assignment.read_bytes()
        for bad in (('handoff', self.dispatch_id, '--host', 'codex', '--backend', 'herdr'),
                    self.target_handoff(self.root/'offline.sock')):
            self.cli(*bad, error='Herdr')
            self.assertEqual(assignment.read_bytes(), original)
            self.assertFalse(archive.exists())
            self.assertTrue(self.handoff_worker_alive(old))
        lock = assignment.with_suffix('.herdr-lock')
        lock.symlink_to(self.root/'missing-lock-target')
        try:
            self.cli(*self.target_handoff(self.service.endpoint), error='assignment lock')
            self.assertEqual(assignment.read_bytes(), original)
            self.assertFalse(archive.exists())
            self.assertTrue(self.handoff_worker_alive(old))
        finally:
            lock.unlink()

        effects = []
        corrupt_snapshot = True

        def snapshot(request, response):
            if corrupt_snapshot and request['method'] == 'session.snapshot':
                response['result']['snapshot'] = {}
            return response

        def verify_before_forwarding(stream):
            frame = stream.readline()
            request = json.loads(frame)
            if request['method'] in ('workspace.create', 'layout.apply'):
                close = json.loads(archive.read_bytes())
                stop = json.loads(stopped.read_bytes())
                self.assertEqual(close['closeOperationId'], f'close:{source["attempt"]}')
                self.assertEqual((stop['launchId'], stop['attempt']),
                                 (stopped.stem, source['attempt']))
                self.assertFalse(self.handoff_worker_alive(old))
                effects.append(request['method'])
            return frame

        with self.service.fault_proxy(snapshot, read_frame=verify_before_forwarding) as proxy:
            args = self.target_handoff(proxy)
            self.cli(*args, error='incomplete Herdr target snapshot')
            self.assertEqual(assignment.read_bytes(), original)
            self.assertFalse(archive.exists())
            self.assertTrue(self.handoff_worker_alive(old))
            self.assertEqual(effects, [])
            corrupt_snapshot = False
            result = self.cli(*args)
            self.assertEqual((result['result'], result['backend'], result['endpoint'], result['round'],
                              result['fromAttempt']), ('accepted', 'herdr', str(proxy), 1, source['attempt']))
            self.assertNotEqual(result['attempt'], source['attempt'])
            new = self.handoff_trace(result['attempt'])
            self.assertEqual(new['callback']['WFC_TASK'], old['callback']['WFC_TASK'])
            self.assertNotEqual(new['callback']['WFC_WORKER_CALLBACK'],
                                old['callback']['WFC_WORKER_CALLBACK'])
            self.assertIsNone(new['tmuxPane'])
            row = self.status()
            self.assertEqual(new['herdrPane'], row['backendHandle']['pane'])
            original_close = archive.read_bytes()
            event, = row['audit']
            self.assertEqual(event['target']['service'], row['backendHandle']['service'])
            self.assertEqual(event['close']['closeOperationId'], f'close:{source["attempt"]}')
            message, = row['messages']
            self.assertEqual((message['attempt'], message['messageId'], message['id']),
                             (source['attempt'], question['messageId'], question['message']))
            self.assertEqual(self.cli(*args), {**result, 'duplicate':True})
            self.cli(*self.target_handoff(self.service.endpoint), error='Herdr source handoff')
            self.assertTrue(self.handoff_worker_alive(new))
            self.assertEqual(archive.read_bytes(), original_close)
            self.assertEqual(effects, ['workspace.create', 'layout.apply'])
            print(json.dumps({'target':event['target'], 'oldClose':event['close']['closeOperationId'],
                              'oldMessageId':message['messageId'], 'createCalls':effects,
                              'differentHerdrTargetRefused':True, 'duplicate':True}))

    def test_tmux_to_herdr_handoff_does_not_retry_after_layout_reply_loss(self):
        source, _ = self.tmux_handoff_fixture()
        archive, stopped = self.handoff_source_evidence(source)
        effects = []
        def lose(request, response):
            if request['method'] in ('workspace.create', 'layout.apply'):
                self.assertTrue(archive.is_file() and stopped.is_file())
                effects.append(request['method'])
            return None if request['method'] == 'layout.apply' else response

        with self.service.fault_proxy(lose) as proxy:
            args = self.target_handoff(proxy)
            result = self.cli(*args)
            self.assertEqual(result['result'], 'uncertain', result)
            new = self.handoff_trace(result['attempt'])
            self.assertTrue(self.handoff_worker_alive(new))
            for replay in (args, self.target_handoff(self.service.endpoint)):
                pending = self.cli(*replay)
                for key in ('attempt','audit','fromAttempt','result','endpoint'):
                    self.assertEqual(pending[key], result[key])
            row = self.status()
            self.assertEqual(row['audit'][0]['result'], 'uncertain')
            self.assertEqual(row['audit'][0]['phase'], 'starting')
            self.assertEqual(effects, ['workspace.create', 'layout.apply'])
            self.assertEqual(len(list(self.handoff_traces.glob('*.json'))), 2)
            print(json.dumps({'lostLayoutReply':True, 'audit':result['audit'],
                              'attempt':result['attempt'], 'effects':effects,
                              'changedEndpointStillPending':True}))

    def test_tmux_to_herdr_handoff_waits_for_source_writer(self):
        source, old = self.tmux_handoff_fixture(hold=True)
        archive, stopped = self.handoff_source_evidence(source)
        effects = []
        def observe(request, response):
            if request['method'] in ('workspace.create', 'layout.apply'):
                effects.append(request['method'])
            return response
        with self.service.fault_proxy(observe) as proxy:
            args = self.target_handoff(proxy)
            result = self.cli(*args)
            self.assertEqual(result['result'], 'uncertain')
            self.assertIn('original writer has not stopped', result['diagnostic'])
            self.assertTrue(archive.is_file())
            self.assertFalse(stopped.exists())
            self.assertTrue(self.handoff_worker_alive(old))
            replay = self.cli(*args)
            self.assertEqual((replay['result'], replay['audit']), ('uncertain', result['audit']))
            self.assertEqual(effects, [])
            print(json.dumps({'closeRecorded':True, 'writerSurvived':True,
                              'targetEffects':effects, 'audit':result['audit']}))

    def test_tmux_to_herdr_handoff_refuses_a_replaced_service(self):
        source, old = self.tmux_handoff_fixture(hold=True)
        archive, stopped = self.handoff_source_evidence(source)
        endpoint = self.service.endpoint
        def workspace_ids():
            return sorted(row['workspace_id']
                          for row in self.service.api('session.snapshot')['snapshot']['workspaces'])

        workspaces = workspace_ids()
        quit_file = self.handoff_traces / (source['attempt'] + '.quit')
        with self.service.fault_proxy(lambda req, reply: reply) as proxy:
            args = self.target_handoff(proxy)
            swapped = False
            with subprocess.Popen([live.DRIVER, *map(str, args)], env=self.env, text=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE) as handoff:
                try:
                    live.wait_for(archive.is_file)
                    self.assertTrue(self.handoff_worker_alive(old))
                    # The original proxy and the already-running Herdr service
                    # have different peers. Swap only the private socket name.
                    endpoint.replace(proxy)
                    swapped = True
                    quit_file.touch()
                    out, err = handoff.communicate(timeout=15)
                finally:
                    quit_file.touch()
                    try:
                        if handoff.poll() is None:
                            handoff.kill()
                            handoff.communicate(timeout=5)
                    finally:
                        if swapped:
                            proxy.replace(endpoint)
            self.assertEqual(handoff.returncode, 0, err)
            result = json.loads(out)
            self.assertEqual(result['result'], 'uncertain')
            self.assertIn('Herdr service identity changed', result['diagnostic'])
            self.assertTrue(stopped.is_file())
            self.assertEqual(workspace_ids(), workspaces)
            self.assertEqual(len(list(self.handoff_traces.glob('*.json'))), 1)
            replay = self.cli(*args)
            self.assertEqual((replay['result'], replay['audit'], replay['attempt']),
                             ('uncertain', result['audit'], result['attempt']))
            print(json.dumps({'replacedTargetPeer':True, 'originalStopped':True,
                              'targetWorkspaceUnchanged':True, 'audit':result['audit']}))

    def reverse_handoff(self, dispatch):
        return ('handoff', dispatch, '--host', 'codex', '--backend', 'tmux')

    def handoff_record(self, source):
        state = Path(self.env['WFC_STATE'])
        return state / 'dispatches' / f'{source["dispatch"]}.json'

    def assert_no_tmux_handoff(self):
        socket = self.env['WFC_TMUX_SOCKET']
        sessions = subprocess.check_output(['tmux', '-S', socket, 'list-sessions', '-F', '#{session_name}'],
                                          env=self.env, text=True, timeout=5)
        self.assertEqual(sessions.splitlines(), ['sibling'])
        self.assertEqual(len(list(self.handoff_traces.glob('*.json'))), 1)
        self.assertFalse(list((Path(self.env['WFC_STATE'])/'launches').glob('*.submission')))

    def assert_reverse_pending(self, args, first, source):
        self.assertEqual(first['result'], 'uncertain', first)
        replay = self.cli(*args)
        for key in ('audit', 'attempt', 'fromAttempt', 'endpoint', 'result'):
            self.assertEqual(replay[key], first[key], key)
        row = self.status()
        self.assertEqual(row['attempt'], source['attempt'])
        self.assertEqual(sum(event['operation'] == 'handoff' for event in row['audit']), 1)
        self.assert_no_tmux_handoff()
        return row

    def test_herdr_to_tmux_handoff_waits_for_confirmed_original_stop(self):
        source, old = self.tmux_handoff_fixture(backend='herdr')
        args = self.reverse_handoff(source['dispatch'])
        path = self.handoff_record(source)
        receipt = Path(source['create']['receipt'])
        stopped = receipt.with_suffix('.stopped')
        old_row = path.read_bytes()
        socket = self.env['WFC_TMUX_SOCKET']
        self.env['WFC_TMUX_SOCKET'] = str(self.root/'offline-tmux.sock')
        self.cli(*args, error='target tmux service')
        self.env['WFC_TMUX_SOCKET'] = socket
        self.assertEqual(path.read_bytes(), old_row)
        self.assertTrue(self.handoff_worker_alive(old))
        self.assert_no_tmux_handoff()
        (self.handoff_traces/'reverse-check').write_text(json.dumps(dict(old, record=str(path), stop=str(stopped))))
        worker_env = {k:v for k,v in self.env.items() if k != 'WFC_CONTROLLER_TOKEN'} | old['callback']
        question = json.loads(subprocess.check_output(
            [live.DRIVER, 'ask', 'original Herdr question', '--attempt', source['attempt']],
            env=worker_env, text=True, timeout=6))
        report = Path(source['report'])
        report.write_text('original contents')
        result = self.cli(*args)
        self.assertEqual((result['result'], result['backend'], result['endpoint'],
                          result['fromAttempt'], result['round']),
                         ('accepted', 'tmux', socket, source['attempt'], source['round']))
        self.assertNotEqual(result['attempt'], source['attempt'])
        trace = self.handoff_trace(result['attempt'])
        self.assertTrue(trace['sourceStoppedBeforeTarget'])
        self.assertIsNotNone(trace['tmuxPane'])
        self.assertIsNone(trace['herdrPane'])
        self.assertEqual(report.read_text(), 'original contents')
        self.assertEqual(trace['callback']['WFC_REPORT'], str(report))
        row = self.status()
        event, closed = row['audit']
        self.assertEqual((event['id'], event['phase'], event['fromBackend']),
                         (result['audit'], 'complete', 'herdr'))
        self.assertEqual(event['close']['id'], closed['id'])
        self.assertEqual(closed['closeOperationId'], f'close:{source["attempt"]}')
        self.assertEqual(closed['executionId'], source['attempt'])
        self.assertEqual(event['writerStopEvidence'], json.loads(stopped.read_bytes()))
        self.assertEqual((row['rounds'][0]['attempt'], row['rounds'][1]['attempt']),
                         (source['attempt'], result['attempt']))
        message, = row['messages']
        self.assertEqual((message['attempt'], message['id'], message['messageId']),
                         (source['attempt'], question['message'], question['messageId']))
        closed_bytes = stopped.read_bytes()
        self.assertEqual(self.cli(*args), {**result, 'duplicate':True})
        self.assertEqual(stopped.read_bytes(), closed_bytes)
        self.assertEqual(len(list(self.handoff_traces.glob('*.json'))), 2)
        self.assertEqual(len(list((stopped.parent).glob('*.submission'))), 1)
        print(json.dumps({'audit':result['audit'], 'sourceStoppedBeforeTarget':True,
                          'close':closed['closeOperationId'], 'oldMessageId':message['messageId'],
                          'tmuxSubmissions':1, 'duplicate':True}))

    def test_herdr_to_tmux_handoff_legacy_receipt_stays_pending(self):
        source, old = self.tmux_handoff_fixture(backend='herdr')
        args = self.reverse_handoff(source['dispatch'])
        path = self.handoff_record(source)
        row = json.loads(path.read_bytes())
        row.pop('executionId')
        for key in ('herdrBinding', 'dispatch', 'attempt', 'round'):
            row['create'].pop(key)
        path.write_text(json.dumps(row))
        first = self.cli(*args)
        self.assertIn('supervision', first['diagnostic'])
        view = self.assert_reverse_pending(args, first, source)
        self.assertEqual([event['operation'] for event in view['audit']], ['handoff'])
        self.assertEqual(view['status'], 'online')
        preflight, = view['audit']
        self.assertEqual(preflight['phase'], 'preflight')
        self.assertFalse(any(field in preflight for field in ('toAttempt', 'close', 'launch')))
        self.assertFalse(list(Path(self.env['WFC_STATE']).glob('handoff-order-*.json')))
        socket = Path(self.env['WFC_TMUX_SOCKET'])
        hidden = socket.with_suffix('.hidden')
        socket.rename(hidden)
        try:
            self.assertEqual(self.cli(*args), first)
        finally:
            hidden.rename(socket)
        self.assertTrue(self.handoff_worker_alive(old))
        worker_env = {k:v for k,v in self.env.items() if k != 'WFC_CONTROLLER_TOKEN'} | old['callback']
        question = json.loads(subprocess.check_output(
            [live.DRIVER, 'ask', 'still running', '--attempt', source['attempt']],
            env=worker_env, text=True, timeout=6))
        report = Path(source['report'])
        report.write_text('completed by the original worker')
        done = json.loads(subprocess.check_output(
            [live.DRIVER, 'done', '--report', report, '--attempt', source['attempt']],
            env=worker_env, text=True, timeout=6))
        view = self.assert_reverse_pending(args, first, source)
        self.assertEqual(view['status'], 'completed')
        self.assertEqual([message['messageId'] for message in view['messages']],
                         [question['messageId'], done['messageId']])
        self.assertEqual(view['audit'], [preflight])
        self.cli(*args, '--task', self.task, '--report', self.root/'next-report.md',
                 error='first-round target')
        self.assertEqual(self.status()['audit'], [preflight])
        self.assert_no_tmux_handoff()
        print(json.dumps({'legacyReceipt':True, 'audit':first['audit'], 'tmuxSubmissions':0}))

    def test_tmux_herdr_tmux_handoff_restores_public_session(self):
        original, _ = self.tmux_handoff_fixture()
        _, old_stop = self.handoff_source_evidence(original)
        first = self.cli(*self.target_handoff(self.service.endpoint))
        self.assertEqual(first['result'], 'accepted', first)
        herdr = self.status()
        self.handoff_trace(first['attempt'])
        args = self.reverse_handoff(original['dispatch'])
        final = self.cli(*args)
        self.assertEqual(final['result'], 'accepted', final)
        self.assertEqual(final['fromAttempt'], herdr['attempt'])
        trace = self.handoff_trace(final['attempt'])
        view = self.status()
        self.assertEqual(view['tmux'], 'wfc-' + original['dispatch'])
        self.assertIsNotNone(trace['tmuxPane'])
        self.assertEqual(self.cli(*args), {**final, 'duplicate':True})
        self.assertTrue(old_stop.is_file())
        self.assertEqual(len(list(self.handoff_traces.glob('*.json'))), 3)
        self.assertEqual([event['id'] for event in view['audit']
                          if event['operation'] == 'handoff'], [first['audit'], final['audit']])
        print(json.dumps({'roundTrip':True, 'session':view['tmux'],
                          'audits':[first['audit'], final['audit']]}))

    def test_herdr_to_tmux_handoff_surviving_descendant_stays_pending(self):
        source, old = self.tmux_handoff_fixture(backend='herdr', detached=True)
        stopped = Path(source['create']['receipt']).with_suffix('.stopped')
        args = self.reverse_handoff(source['dispatch'])
        first = self.cli(*args)
        self.assert_reverse_pending(args, first, source)
        self.assertFalse(self.handoff_worker_alive(old))
        self.assertTrue(self.handoff_worker_alive(old['detached']))
        self.assertFalse(stopped.exists())
        print(json.dumps({'survivingDetachedDescendant':True, 'audit':first['audit'],
                          'tmuxSubmissions':0}))

    def test_herdr_to_tmux_handoff_lost_close_keeps_original_audit(self):
        effects = []
        refused = []
        def read_frame(stream):
            frame = stream.readline()
            if json.loads(frame)['method'] != 'pane.close':
                return frame
            self.assertTrue(self.handoff_worker_alive(old))
            for callback in (('ask', 'must stay fenced'), ('done', '--report', source['report'])):
                result = subprocess.run(
                    [live.DRIVER, *callback, '--attempt', source['attempt']],
                    env=worker_env, text=True, capture_output=True, timeout=2)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn('handoff is closing the old execution', result.stderr)
                refused.append(callback[0])
            return frame

        def lose(request, response):
            if request['method'] == 'pane.close':
                effects.append(request['params']['pane_id'])
                return None
            return response
        with self.service.fault_proxy(lose, read_frame=read_frame) as proxy:
            source, old = self.tmux_handoff_fixture(backend='herdr', endpoint=proxy)
            worker_env = {k:v for k,v in self.env.items() if k != 'WFC_CONTROLLER_TOKEN'} | old['callback']
            Path(source['report']).write_text('completion must stay fenced')
            args = self.reverse_handoff(source['dispatch'])
            first = self.cli(*args)
            view = self.assert_reverse_pending(args, first, source)
            event, close = view['audit']
            self.assertEqual((event['id'], event['phase'], close['result']),
                             (first['audit'], 'closing', 'uncertain'))
            self.assertEqual(effects, [source['backendHandle']['pane']])
            self.assertEqual(refused, ['ask', 'done'])
            self.assertFalse(any(message['type'] in ('question', 'done') for message in view['messages']))
            print(json.dumps({'lostCloseReply':True, 'audit':first['audit'],
                              'closeCalls':len(effects), 'refusedCallbacks':refused,
                              'tmuxSubmissions':0}))

    def test_herdr_to_tmux_handoff_mismatched_stop_stays_pending(self):
        stopped = None
        def change(request, response):
            if request['method'] == 'pane.close':
                live.wait_for(stopped.is_file)
                marker = json.loads(stopped.read_bytes())
                marker['attempt'] = 'a-' + '0'*32
                substitute = stopped.with_suffix('.fixture')
                substitute.write_text(json.dumps(marker))
                substitute.replace(stopped)
            return response
        with self.service.fault_proxy(change) as proxy:
            source, old = self.tmux_handoff_fixture(backend='herdr', endpoint=proxy)
            stopped = Path(source['create']['receipt']).with_suffix('.stopped')
            args = self.reverse_handoff(source['dispatch'])
            first = self.cli(*args)
            view = self.assert_reverse_pending(args, first, source)
            event, close = view['audit']
            self.assertEqual((event['id'], close['result']), (first['audit'], 'completed'))
            self.assertFalse(self.handoff_worker_alive(old))
            print(json.dumps({'mismatchedStop':True, 'closeCompleted':True,
                              'audit':first['audit'], 'tmuxSubmissions':0}))

    def test_dispatch_current_execution_done_and_close(self):
        self.dispatch()
        self.env['WFC_HERDR_ENDPOINT'] = str(self.root/'different.sock')
        view = self.status()
        self.assertTrue(view['backendAlive'])
        self.assertNotIn('workerCallbackHash', self.row)
        self.assertNotIn('workerCallbackHash', view)
        self.assertEqual(view['backendHandle']['service']['pid'], self.service.server.pid)
        self.assertTrue(view['backendHandle']['writerProcesses'][0]['creation'].startswith('linux:'))
        assignment = json.loads(live.wait_for(lambda: (self.root/'assignment').exists() and (self.root/'assignment').read_text()))
        self.assertEqual([assignment['WFC_'+k.upper()] for k in ('dispatch','round','attempt','task','report')],
                         [str(view[k]) for k in ('dispatch','round','attempt','task','report')])
        self.assertEqual(assignment['WFC_BIN'], live.DRIVER)
        self.assertIsNone(assignment['WFC_CONTROLLER_TOKEN'])
        self.assertIsNone(assignment['TMUX_PANE'])
        self.assertEqual(assignment['HERDR_PANE_ID'], self.h['pane'])
        self.assertEqual(assignment['HERDR_WORKSPACE_ID'], self.h['workspace'])
        other, pid = self.sibling()
        self.submit('wrong attempt', 'stale', attempt='a-123456789abc')
        self.submit('wrong round', 'stale', round=2)
        self.submit('finish')
        live.wait_for(lambda: self.status()['state']=='done')
        self.assertFalse(self.status()['pendingInput'])
        self.assertEqual(self.status()['rounds'][0]['result']['attempt'], self.row['attempt'])
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        self.assertFalse(self.status()['backendAlive'])
        self.assertEqual(self.status()['state'],'done')
        self.assertEqual(self.service.api('pane.process_info',pane_id=other)['process_info']['shell_pid'],pid)
        os.kill(pid, 0)
        self.assertEqual((self.root/'events').read_text().splitlines(), ['finish'])

    def test_reclaim_uses_verified_herdr_close_and_keeps_sibling(self):
        self.dispatch()
        sibling, pid = self.sibling()
        self.submit('finish')
        live.wait_for(lambda: self.status()['state'] == 'done')
        assignment = self.root / 'router' / 'dispatches' / (self.dispatch_id + '.json')
        before = assignment.read_bytes()
        item, = self.cli('reclaim')['sessions']
        self.assertEqual((item['dispatch'], item['result']), (self.dispatch_id, 'listed'))
        self.assertEqual(assignment.read_bytes(), before)
        item, = self.cli('reclaim', '--yes')['sessions']
        self.assertEqual(item['result'], 'reclaimed')
        view = self.status()
        self.assertEqual((view['state'], view['status'], view['backendAlive']), ('done', 'reclaimed', False))
        self.assertEqual(view['rounds'][0]['result']['attempt'], self.row['attempt'])
        self.assertEqual(self.service.api('pane.process_info', pane_id=sibling)['process_info']['shell_pid'], pid)
        self.assertEqual((self.root / 'report.md').read_text(), 'finished')
        print(json.dumps(dict(herdrReclaim=item, siblingPid=pid)), flush=True)

    def test_close_after_scope_takeover_fences_stale_herdr_controls(self):
        self.dispatch()
        scope = self.row['scope']
        first = self.cli('acquire', '--scope', scope, '--owner', 'A')
        current = self.cli('takeover', '--scope', scope, '--owner', 'B')
        self.env['WFC_CONTROLLER_TOKEN'] = first['token']
        self.submit('old text', error='stale_controller_owner')
        self.reconcile(error='stale_controller_owner')
        self.cli('close', self.dispatch_id, error='stale_controller_owner')
        view = self.status()
        self.assertTrue(view['backendAlive'])
        self.assertEqual(view['controllerOwner']['generation'], current['generation'])
        self.assertNotIn('workerCallbackHash', view)
        self.assertFalse((self.root/'events').exists())
        closed = self.cli('close', self.dispatch_id, '--owner-token', current['token'])
        self.assertEqual((closed['result'], closed['attempt']), ('completed', self.row['attempt']))
        print(json.dumps({'herdrOwnerGeneration': current['generation'],
                          'herdrClosedAttempt': closed['attempt']}), flush=True)

    def test_claim_survives_lost_ack_and_reconcile(self):
        sends = []
        def lose(req, reply):
            if req['method'] in ('pane.send_input','pane.send_keys'):
                sends.append(req['method'])
                return None
            return reply
        with self.service.fault_proxy(lose) as proxy:
            self.dispatch(endpoint=proxy)
            self.submit('unique', 'uncertain')
            self.assertTrue(self.status()['pendingInput'])
            self.reconcile(error='authorization', owner_token='unverified')
            self.reconcile(error='mismatch', terminal='wrong', old_writer_evidence='checked old writer')
            self.assertEqual(self.status()['audit'], [])
            receipt = self.reconcile()
            self.assertFalse(receipt['inputReplayed'])
            view = self.status()
            event, = view['audit']
            self.assertEqual([event[k] for k in ('dispatch','attempt','round','id')],
                             [receipt[k] for k in ('dispatch','attempt','round','audit')])
            self.assertEqual(event['resolution']['actor'], 'operator')
            self.assertEqual(json.loads(event['resolution']['evidence'])['mapping'], 'inspected unit')
            self.assertTrue(view['pendingInput'])
            self.submit('one more', 'pending')
            self.assertEqual(sends, ['pane.send_input'])
            self.assertEqual((self.root/'events').read_text().splitlines(), ['unique'])
            self.cli('close', self.dispatch_id)

    def setup_reply_worker(self, endpoint=None, mode='empty', host='codex'):
        repo = Path(__file__).resolve().parents[1]
        draft_screens = {
            'ambiguous-footer': '› original draft\n  tail · value\n  model · dir',
            'ambiguous-continuation': '›\n  tail · value\n  model · dir',
            'compact-placeholder': '› AskCodextodoanything\n  model · dir',
            'existing-scroll': '› original draft ↑',
            'folded-placeholder-draft': '› Ask Codex to do\n  anything',
            'shell': '! echo untouched',
            'unknown': '❯ Ask Codex to do anything',
            'ultra-empty': '» Ask Codex to do anything',
            'ultra-draft': '» original Ultra draft',
            'codex-footer': '100% context left',
            'codex-image': '100% context left',
            'codex-image-unstyled': '100% context left',
            'opencode-draft': '┃  keep OpenCode draft',
            'opencode-shell': 'esc exit shell mode',
            'claude-draft': '❯ keep Claude draft',
            'claude-disabled': '⏵⏵ auto mode',
        }
        compact = '›\n  model · dir\n  keys · help'
        draft_screens.update({mode: compact for mode in (
            'styled-compact-empty', 'styled-compact-draft', 'unstyled-compact-empty', 'unstyled-compact-draft',
        )})
        args = [repo/'crates/herdr/tests/composer.py', 'one', '--submit-first', '--reply-worker', mode]
        self.dispatch(endpoint=endpoint, host=host, command=[sys.executable, *map(str, args)])
        question = self.root/'question.json'
        receipt = json.loads(live.wait_for(lambda: question.exists() and question.read_bytes()))
        prompt = draft_screens.get(mode, '› Ask Codex to do anything')
        live.wait_for(lambda: prompt in '\n'.join(line.rstrip() for line in self.reply_screen().splitlines()))
        return receipt

    def reply_screen(self, format='text'):
        return self.service.api('pane.read', pane_id=self.h['pane'], source='recent', format=format,
                                strip_ansi=format == 'text', lines=50)['read']['text']

    def reply_attempts(self):
        return [m for m in self.status()['messages'] if m['type'] == 'reply']

    def assert_reply_worker_finished(self, question, outcome, earlier=()):
        replies = self.reply_attempts()
        self.assertEqual([m['inputResult'] for m in replies], [*earlier, outcome])
        reply = replies[-1]
        self.assertEqual(reply['questionMessageId'], question['messageId'])
        self.assertEqual(reply['to'], question['message'])
        for key in ('dispatch', 'round', 'attempt'):
            self.assertEqual(reply[key], self.row[key])
        self.cli('reply', question['message'], 'again', error='already answered')
        accepted = self.root/'accepted'
        received = live.wait_for(lambda: accepted.exists() and accepted.read_text())
        self.assertEqual(received.splitlines(), [f"[router] Answer to question {question['message']}: once"])
        first, = self.cli('wait', '--scope', self.row['scope'], '--timeout', 0)['messages']
        self.assertEqual(first['type'], 'question')
        (self.root/'finish').touch()
        done, = self.cli('wait', '--scope', self.row['scope'], '--timeout', 8)['messages']
        self.assertEqual((done['type'], done['attempt']), ('done', self.row['attempt']))
        self.assertEqual(Path(done['report']).read_text(), 'original worker received the answer\n')
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def test_public_reply_on_bound_herdr_pane(self):
        question = self.setup_reply_worker()
        self.assertEqual(self.cli('reply', question['message'], 'once'),
                         {'replied': question['message'], 'dispatch': self.dispatch_id})
        self.assertFalse(self.status()['pendingInput'])
        self.assert_reply_worker_finished(question, 'accepted')

    def test_public_reply_retains_claim_after_herdr_delivers_without_receipt(self):
        sends = []

        def lose(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
                return None
            return reply

        with self.service.fault_proxy(lose) as proxy:
            question = self.setup_reply_worker(proxy)
            self.cli('reply', question['message'], 'once', error='uncertain')
            self.assertTrue(self.status()['pendingInput'])
            self.assert_reply_worker_finished(question, 'uncertain')
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_releases_claim_after_herdr_preflight_read_fails(self):
        sends = []
        dropped = []
        fail_at = None
        saw_ansi = False

        def lose(req, reply):
            nonlocal fail_at, saw_ansi
            method = req['method']
            if method in ('pane.send_input', 'pane.send_keys'):
                sends.append(method)
            at_ansi = method == 'pane.read' and req['params']['format'] == 'ansi'
            if (fail_at == 'ansi' and at_ansi
                    or fail_at == 'prepare' and saw_ansi and method == 'session.snapshot'):
                dropped.append(fail_at)
                fail_at = None
                return None
            saw_ansi |= at_ansi
            return reply

        with self.service.fault_proxy(lose) as proxy:
            question = self.setup_reply_worker(proxy)
            for phase in ('ansi', 'prepare'):
                fail_at = phase
                saw_ansi = False
                self.cli('reply', question['message'], 'unsent', error='definitely-not-sent')
                self.assertEqual(dropped[-1], phase)
                self.assertEqual(sends, [])
                self.assertFalse((self.root/'accepted').exists())
                self.assertFalse(self.status()['pendingInput'])
                attempt = self.reply_attempts()[-1]
                self.assertEqual(attempt['inputResult'], 'definitely-not-sent')
                self.assertIn('Herdr exchange incomplete or invalid', attempt['diagnostic'])
            self.cli('reply', question['message'], 'once')
            self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',) * 2)
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_preserves_existing_scrolled_herdr_draft(self):
        sends = []

        def count(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
            return reply

        with self.service.fault_proxy(count) as proxy:
            question = self.setup_reply_worker(proxy, mode='existing-scroll')
            screen = self.reply_screen()
            self.cli('reply', question['message'], 'unsent', error='definitely-not-sent')
            self.assertEqual(sends, [])
            self.assertEqual(self.reply_screen(), screen)
            self.assertFalse(self.status()['pendingInput'])
            attempt, = self.reply_attempts()
            self.assertEqual(attempt['inputResult'], 'definitely-not-sent')
            self.assertIn('composer already contains a draft', attempt['diagnostic'])
            (self.root/'clear-draft').touch()
            live.wait_for(lambda: 'Ask Codex to do anything' in self.reply_screen())
            self.cli('reply', question['message'], 'once')
            self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',))
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_keeps_claim_when_herdr_draft_reappears(self):
        sends = []
        samples = []

        def transitional_frame(req, reply):
            method = req['method']
            if method in ('pane.send_input', 'pane.send_keys'):
                sends.append(method)
            elif method == 'pane.read' and sends:
                read = reply['result']['read']
                if len(samples) < 2:
                    read['text'] = ('\x1b[1m›\x1b[0m' if req['params']['format'] == 'ansi' else '›')
                    read['text'] += '\n\n  model · dir'
                samples.append(read['text'])
            return reply

        with self.service.fault_proxy(transitional_frame) as proxy:
            question = self.setup_reply_worker(proxy, mode='always-drop')
            expected = f"[router] Answer to question {question['message']}: once"
            self.cli('reply', question['message'], 'once', error='draft after an empty composer')
            self.assertEqual(len(samples), 4)
            self.assertEqual(samples[0], '›\n\n  model · dir')
            self.assertIn(expected, samples[2])
            self.assertEqual(sends, ['pane.send_input'])
            self.assertFalse((self.root/'accepted').exists())
            self.assertTrue(self.status()['pendingInput'])
            attempt, = self.reply_attempts()
            self.assertEqual(attempt['inputResult'], 'uncertain')
            self.cli('reply', question['message'], 'again', error='already answered')
            self.assertEqual(sends, ['pane.send_input'])
            self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def test_public_reply_preserves_herdr_draft_with_ambiguous_footer(self):
        sends = []

        def count(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
            return reply

        with self.service.fault_proxy(count) as proxy:
            question = self.setup_reply_worker(proxy, mode='ambiguous-footer')
            screen = self.reply_screen()
            self.cli('reply', question['message'], 'unsent', error='definitely-not-sent')
            self.assertEqual(sends, [])
            self.assertEqual(self.reply_screen(), screen)
            self.assertFalse((self.root/'accepted').exists())
            self.assertFalse(self.status()['pendingInput'])
            attempt, = self.reply_attempts()
            self.assertEqual(attempt['inputResult'], 'definitely-not-sent')
            self.assertIn('composer already contains a draft', attempt['diagnostic'])
            (self.root/'clear-draft').touch()
            live.wait_for(lambda: 'Ask Codex to do anything' in self.reply_screen())
            self.cli('reply', question['message'], 'once')
            self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',))
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_distinguishes_zero_write_deadline_and_lost_receipt(self):
        preparation = []
        empty_requests = []
        sends = []
        expiring = False
        lose_receipt = False

        def read_frame(stream):
            frame = stream.readline()
            if expiring and not frame:
                empty_requests.append(frame)
            return frame or None

        def observe(req, reply):
            method = req['method']
            if expiring:
                preparation.append(method)
            if method in ('pane.send_input', 'pane.send_keys'):
                sends.append(method)
                if lose_receipt:
                    return None
            return reply

        with self.service.fault_proxy(observe, read_frame=read_frame) as proxy:
            question = self.setup_reply_worker(proxy)
            number = str(question['message'])
            expiring = True
            # Connections 1–6 read text and ANSI, then 7–10 prepare the send.
            # Delay the exit of the input connection syscall past its session
            # deadline, so the real proxy sees that connection but no bytes.
            trace = self.root/'reply-connect.log'
            result = subprocess.run([
                'strace', '-e', 'trace=connect', '-e', 'inject=connect:delay_exit=3s:when=11',
                '-o', trace, '--', live.DRIVER, 'reply', number, 'expired',
            ], env={**self.env, 'WFC_HERDR_TIMEOUT': '2'}, text=True,
               capture_output=True, timeout=12)
            self.assertEqual((result.returncode, result.stdout), (1, ''), result.stderr)
            self.assertIn('definitely-not-sent: Herdr reply input was not sent', result.stderr,
                          trace.read_text())
            live.wait_for(lambda: empty_requests)
            expiring = False
            self.assertEqual(preparation, ['ping', 'session.snapshot', 'pane.read',
                                           'ping', 'session.snapshot', 'pane.read',
                                           'ping', 'session.snapshot', 'pane.process_info',
                                           'session.snapshot'])
            self.assertEqual(empty_requests, [b''])
            self.assertEqual(sends, [])
            self.assertFalse((self.root/'accepted').exists())
            self.assertFalse(self.status()['pendingInput'])
            attempt, = self.reply_attempts()
            self.assertEqual(attempt['inputResult'], 'definitely-not-sent')
            self.assertEqual(attempt['diagnostic'], 'Herdr reply input was not sent')
            lose_receipt = True
            self.cli('reply', number, 'once', error='uncertain')
            self.assertTrue(self.status()['pendingInput'])
            self.assert_reply_worker_finished(question, 'uncertain', earlier=('definitely-not-sent',))
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_protects_herdr_placeholder_like_and_ambiguous_drafts(self):
        sends = []

        def count(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
            return reply

        with self.service.fault_proxy(count) as proxy:
            for mode in ('compact-placeholder', 'ambiguous-continuation'):
                sends.clear()
                for filename in ('accepted', 'question.json'):
                    (self.root/filename).unlink(missing_ok=True)
                question = self.setup_reply_worker(proxy, mode=mode)
                screen = self.reply_screen()
                self.cli('reply', question['message'], 'unsent', error='definitely-not-sent')
                self.assertEqual(sends, [], mode)
                self.assertEqual(self.reply_screen(), screen, mode)
                self.assertFalse((self.root/'accepted').exists(), mode)
                self.assertFalse(self.status()['pendingInput'], mode)
                attempt, = self.reply_attempts()
                self.assertEqual(attempt['inputResult'], 'definitely-not-sent', mode)
                self.assertIn('composer already contains a draft', attempt['diagnostic'], mode)
                (self.root/'clear-draft').touch()
                prompt = ('Ask Codex to do\n  anything' if mode == 'compact-placeholder'
                          else 'Ask Codex to do anything')
                live.wait_for(lambda: prompt in self.reply_screen())
                self.cli('reply', question['message'], 'once')
                self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',))
                self.assertEqual(sends, ['pane.send_input'], mode)

    def test_public_reply_retries_after_herdr_presend_checkpoint_failure(self):
        self.assertNotEqual(os.geteuid(), 0, 'the dispatch permission fault requires a non-root runner')
        sends = []
        lose_receipt = False

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
                if lose_receipt:
                    return None
            return reply

        with self.service.fault_proxy(observe) as proxy:
            question = self.setup_reply_worker(proxy)
            number = str(question['message'])
            self.assertFalse(self.status()['pendingInput'])
            state = Path(self.env['WFC_STATE'])
            dispatches = state/'dispatches'
            assignment = dispatches/f'{self.dispatch_id}.json'
            initial = assignment.read_bytes()
            permissions = dispatches.stat().st_mode & 0o777
            try:
                dispatches.chmod(permissions & ~0o222)
                self.assertFalse(os.access(dispatches, os.W_OK))
                self.assertTrue(os.access(state, os.W_OK))
                diagnostic = self.cli('reply', number, 'permission fault', error='definitely-not-sent')
                self.assertIn('could not persist Herdr pending input before submission', diagnostic)
                self.assertEqual(assignment.read_bytes(), initial)
                self.assertEqual(sends, [])
            finally:
                dispatches.chmod(permissions)
            attempt, = self.reply_attempts()
            self.assertEqual(attempt['inputResult'], 'definitely-not-sent')
            self.assertFalse(self.status()['pendingInput'])

            # The reply ledger reserve and start each sync file + directory
            # (1–4). Fail the dispatch directory sync (6) after publishing.
            trace = self.root/'reply-fsync.log'
            result = subprocess.run([
                'strace', '-e', 'trace=fsync', '-e', 'inject=fsync:error=EIO:when=6',
                '-o', trace, '--', live.DRIVER, 'reply', number, 'sync fault',
            ], env=self.env, text=True, capture_output=True, timeout=12)
            self.assertEqual((result.returncode, result.stdout), (1, ''), result.stderr)
            self.assertIn('definitely-not-sent: could not persist Herdr pending input before submission',
                          result.stderr, trace.read_text())
            pending = json.loads(assignment.read_bytes())
            self.assertTrue(pending['pendingInput'])
            attempts = self.reply_attempts()
            self.assertEqual([row['inputResult'] for row in attempts], ['definitely-not-sent'] * 2)
            self.assertEqual(pending['pendingReplyMessageId'], attempts[-1]['messageId'])
            self.assertEqual(sends, [])
            self.assertFalse((self.root/'accepted').exists())

            lose_receipt = True
            self.cli('reply', number, 'once', error='uncertain')
            self.assertTrue(self.status()['pendingInput'])
            self.assert_reply_worker_finished(question, 'uncertain', earlier=('definitely-not-sent',) * 2)
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_accepts_styled_compact_herdr_empty_composer(self):
        reads, sends = [], []

        def observe(req, reply):
            if req['method'] == 'pane.read':
                reads.append(req['params']['format'])
            elif req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
            return reply

        with self.service.fault_proxy(observe) as proxy:
            question = self.setup_reply_worker(proxy, mode='styled-compact-empty')
            plain = '\n'.join(line.rstrip() for line in self.reply_screen().strip().splitlines())
            self.assertEqual(plain, '›\n  model · dir\n  keys · help')
            self.assertIn('\x1b[48;', self.reply_screen('ansi'))
            reads.clear()
            self.cli('reply', question['message'], 'once')
            self.assertEqual(reads, ['text', 'ansi'] * 3)
            self.assertFalse(self.status()['pendingInput'])
            self.assert_reply_worker_finished(question, 'accepted')
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_protects_compact_herdr_drafts_without_footer_proof(self):
        sends = []

        def count(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
            return reply

        with self.service.fault_proxy(count) as proxy:
            indistinguishable = None
            for mode in ('styled-compact-draft', 'unstyled-compact-empty', 'unstyled-compact-draft'):
                sends.clear()
                for filename in ('accepted', 'question.json'):
                    (self.root/filename).unlink(missing_ok=True)
                question = self.setup_reply_worker(proxy, mode=mode)
                plain, styled = self.reply_screen(), self.reply_screen('ansi')
                self.assertEqual('\n'.join(line.rstrip() for line in plain.strip().splitlines()),
                                 '›\n  model · dir\n  keys · help')
                if mode == 'styled-compact-draft':
                    self.assertIn('\x1b[48;', styled)
                elif indistinguishable is None:
                    indistinguishable = styled
                else:
                    self.assertEqual(styled, indistinguishable)
                self.cli('reply', question['message'], 'unsent', error='unverified footer boundary')
                self.assertEqual(sends, [], mode)
                self.assertEqual((self.reply_screen(), self.reply_screen('ansi')), (plain, styled), mode)
                self.assertFalse((self.root/'accepted').exists(), mode)
                self.assertFalse(self.status()['pendingInput'], mode)
                attempt, = self.reply_attempts()
                self.assertEqual(attempt['inputResult'], 'definitely-not-sent', mode)
                (self.root/'clear-draft').touch()
                live.wait_for(lambda: 'Ask Codex to do anything' in self.reply_screen())
                self.cli('reply', question['message'], 'once')
                self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',))
                self.assertEqual(sends, ['pane.send_input'], mode)

    def test_managed_herdr_reply_rechecks_owner_while_waiting(self):
        sends = []

        def count(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                sends.append(req['method'])
            return reply

        with self.service.fault_proxy(count) as proxy:
            question = self.setup_reply_worker(proxy)
            number, scope = str(question['message']), self.row['scope']
            first = self.cli('acquire', '--scope', scope, '--owner', 'A')
            self.env['WFC_CONTROLLER_TOKEN'] = first['token']
            state = Path(self.env['WFC_STATE'])
            lock_path = state/'dispatches'/f"{self.dispatch_id}.herdr-lock"

            def pending():
                return any((row.get('reply_payload') or {}).get('inputResult') == 'pending'
                           for row in json.loads((state/'messages.json').read_bytes()))

            with lock_path.open('rb') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                with subprocess.Popen([live.DRIVER, 'reply', number, 'stale input'], env=self.env,
                                      text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as old_reply:
                    try:
                        live.wait_for(pending)
                        current = self.cli('takeover', '--scope', scope, '--owner', 'B')
                        out, err = old_reply.communicate(timeout=5)
                    finally:
                        fcntl.flock(lock, fcntl.LOCK_UN)
            self.assertEqual((old_reply.returncode, out), (1, ''))
            stale = json.loads(err)
            self.assertEqual((stale['error'], stale['generation']),
                             ('stale_controller_owner', current['generation']))
            self.assertFalse(self.status()['pendingInput'])
            self.assertEqual(sends, [])
            self.cli('--herdr-backend', payload={
                'op': 'submit', 'dispatch': self.dispatch_id, 'round': self.row['round'],
                'attempt': self.row['attempt'], 'text': 'private input',
            }, error='stale_controller_owner')
            self.cli('reply', number, 'once', '--owner-token', current['token'])
            self.env['WFC_CONTROLLER_TOKEN'] = current['token']
            self.assert_reply_worker_finished(question, 'accepted', earlier=('not-submitted',))
            self.assertEqual(sends, ['pane.send_input'])

    def test_public_reply_distinguishes_authored_herdr_text_from_dim_placeholder(self):
        writes = []

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                writes.append(req['method'])
            return reply

        with self.service.fault_proxy(observe) as proxy:
            for mode in ('plain-placeholder-draft', 'folded-placeholder-draft'):
                with self.subTest(mode=mode):
                    writes.clear()
                    for name in ('accepted', 'question.json'):
                        (self.root/name).unlink(missing_ok=True)
                    question = self.setup_reply_worker(proxy, mode=mode)
                    plain, styled = self.reply_screen(), self.reply_screen('ansi')
                    self.assertNotIn('\x1b[2m', styled)
                    self.cli('reply', question['message'], 'unsent', error='composer already contains a draft')
                    self.assertEqual(writes, [])
                    self.assertEqual((self.reply_screen(), self.reply_screen('ansi')), (plain, styled))
                    self.assertFalse((self.root/'accepted').exists())
                    self.assertFalse(self.status()['pendingInput'])
                    self.assertEqual(self.reply_attempts()[-1]['inputResult'], 'definitely-not-sent')

                    (self.root/'clear-draft').touch()
                    live.wait_for(lambda: '\x1b[2m' in self.reply_screen('ansi'))
                    self.assertEqual(self.reply_screen(), plain)
                    self.cli('reply', question['message'], 'once')
                    self.assertFalse(self.status()['pendingInput'])
                    self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',))
                    self.assertEqual(writes, ['pane.send_input'])

    def test_public_reply_requires_a_writable_herdr_chat_prompt(self):
        writes = []

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                writes.append(req['method'])
            return reply

        with self.service.fault_proxy(observe) as proxy:
            for mode, host, complaint in (
                ('ultra-draft', 'codex', 'composer already contains a draft'),
                ('shell', 'codex', 'writable chat composer could not be verified'),
                ('unknown', 'codex', 'writable chat composer could not be verified'),
                ('disabled', 'codex', 'writable chat composer could not be verified'),
                ('empty', 'claude', 'writable chat composer could not be verified'),
                ('ultra-empty', 'codex', None),
            ):
                with self.subTest(mode=mode, host=host):
                    writes.clear()
                    for name in ('accepted', 'question.json'):
                        (self.root/name).unlink(missing_ok=True)
                    question = self.setup_reply_worker(proxy, mode=mode, host=host)
                    if complaint is not None:
                        plain, styled = self.reply_screen(), self.reply_screen('ansi')
                        self.cli('reply', question['message'], 'unsent', error=complaint)
                        self.assertEqual(writes, [])
                        self.assertEqual((self.reply_screen(), self.reply_screen('ansi')), (plain, styled))
                        self.assertFalse((self.root/'accepted').exists())
                        self.assertFalse(self.status()['pendingInput'])
                        self.assertEqual(self.reply_attempts()[-1]['inputResult'], 'definitely-not-sent')
                        first, = self.cli('wait', '--scope', self.row['scope'], '--timeout', 0)['messages']
                        self.assertEqual((first['type'], first['messageId']), ('question', question['messageId']))
                        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
                        continue
                    self.cli('reply', question['message'], 'once')
                    self.assertFalse(self.status()['pendingInput'])
                    self.assert_reply_worker_finished(question, 'accepted')
                    self.assertEqual(writes, ['pane.send_input'])

    def test_public_reply_retains_claim_when_herdr_empty_composer_cannot_be_confirmed(self):
        writes, dropped = [], []
        drop_ansi = False

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                writes.append(req['method'])
            if drop_ansi and writes and req['method'] == 'pane.read' and req['params']['format'] == 'ansi':
                dropped.append(req['params']['format'])
                return None
            return reply

        with self.service.fault_proxy(observe) as proxy:
            for mode in ('plain-after-send', 'empty'):
                with self.subTest(mode=mode):
                    writes.clear()
                    dropped.clear()
                    for name in ('accepted', 'question.json'):
                        (self.root/name).unlink(missing_ok=True)
                    question = self.setup_reply_worker(proxy, mode=mode)
                    drop_ansi = mode == 'empty'
                    diagnostic = self.cli('reply', question['message'], 'once', error='uncertain')
                    drop_ansi = False
                    self.assertEqual(dropped, ['ansi'] if mode == 'empty' else [])
                    if mode == 'plain-after-send':
                        self.assertIn('chat composer did not confirm the reply', diagnostic)
                        self.assertIn('› Ask Codex to do anything', self.reply_screen())
                        self.assertNotIn('\x1b[2m', self.reply_screen('ansi'))
                    self.assertTrue(self.status()['pendingInput'])
                    self.assert_reply_worker_finished(question, 'uncertain')
                    self.assertEqual(writes, ['pane.send_input'])

    def test_public_reply_uses_upstream_codex_footer_and_protects_attachments(self):
        writes = []

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                writes.append(req['method'])
            return reply

        with self.service.fault_proxy(observe) as proxy:
            for mode in ('codex-footer', 'codex-image', 'codex-image-unstyled'):
                with self.subTest(mode=mode):
                    writes.clear()
                    for name in ('accepted', 'question.json'):
                        (self.root/name).unlink(missing_ok=True)
                    question = self.setup_reply_worker(proxy, mode=mode)
                    plain, styled = self.reply_screen(), self.reply_screen('ansi')
                    self.assertRegex(plain, r'\? for shortcuts +100% context left')
                    earlier = ()
                    if mode != 'codex-footer':
                        self.assertIn('[Image #1]', plain)
                        self.cli('reply', question['message'], 'unsent',
                                 error='composer already contains a draft')
                        self.assertEqual(writes, [])
                        self.assertEqual((self.reply_screen(), self.reply_screen('ansi')), (plain, styled))
                        self.assertFalse(self.status()['pendingInput'])
                        self.assertFalse((self.root/'accepted').exists())
                        (self.root/'clear-draft').touch()
                        live.wait_for(lambda: self.reply_screen('ansi') != styled)
                        if mode == 'codex-image':
                            self.assertEqual(self.reply_screen(), plain)
                        else:
                            self.assertIn('assistant response', self.reply_screen())
                        earlier = ('definitely-not-sent',)
                    self.cli('reply', question['message'], 'once')
                    self.assertFalse(self.status()['pendingInput'])
                    self.assert_reply_worker_finished(question, 'accepted', earlier=earlier)
                    self.assertEqual(writes, ['pane.send_input'])

    def test_public_reply_only_retries_enter_for_the_exact_visible_codex_line(self):
        writes = []

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                writes.append(req['method'])
            return reply

        with self.service.fault_proxy(observe) as proxy:
            for mode in ('changed-reply', 'folded-reply', 'exact-reply'):
                with self.subTest(mode=mode):
                    writes.clear()
                    for name in ('accepted', 'question.json'):
                        (self.root/name).unlink(missing_ok=True)
                    question = self.setup_reply_worker(proxy, mode=mode)
                    if mode == 'exact-reply':
                        self.cli('reply', question['message'], 'once')
                        self.assert_reply_worker_finished(question, 'accepted')
                        self.assertEqual(writes, ['pane.send_input', 'pane.send_keys'])
                        continue
                    self.cli('reply', question['message'], 'a b', error='uncertain')
                    self.assertIn(': ab' if mode == 'changed-reply' else ': a\n  b', self.reply_screen())
                    self.assertEqual(writes, ['pane.send_input'])
                    self.assertFalse((self.root/'accepted').exists())
                    self.assertTrue(self.status()['pendingInput'])
                    self.assertEqual(self.reply_attempts()[-1]['inputResult'], 'uncertain')
                    self.cli('reply', question['message'], 'again', error='already answered')
                    self.assertEqual(writes, ['pane.send_input'])
                    self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def test_public_reply_uses_native_opencode_and_claude_composer_shapes(self):
        writes = []

        def observe(req, reply):
            if req['method'] in ('pane.send_input', 'pane.send_keys'):
                writes.append(req['method'])
            return reply

        with self.service.fault_proxy(observe) as proxy:
            for mode in ('opencode-shell', 'claude-disabled', 'opencode-draft', 'claude-draft'):
                with self.subTest(mode=mode):
                    writes.clear()
                    for name in ('accepted', 'question.json'):
                        (self.root/name).unlink(missing_ok=True)
                    host, variant = mode.split('-', 1)
                    question = self.setup_reply_worker(proxy, mode=mode, host=host)
                    plain, styled = self.reply_screen(), self.reply_screen('ansi')
                    diagnostic = ('composer already contains a draft' if variant == 'draft'
                                  else 'writable chat composer could not be verified')
                    self.cli('reply', question['message'], 'unsent', error=diagnostic)
                    self.assertEqual(writes, [])
                    self.assertEqual((self.reply_screen(), self.reply_screen('ansi')), (plain, styled))
                    self.assertFalse(self.status()['pendingInput'])
                    self.assertFalse((self.root/'accepted').exists())
                    self.assertEqual(self.reply_attempts()[-1]['inputResult'], 'definitely-not-sent')
                    if variant != 'draft':
                        first, = self.cli('wait', '--scope', self.row['scope'], '--timeout', 0)['messages']
                        self.assertEqual(first['messageId'], question['messageId'])
                        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
                        continue
                    (self.root/'clear-draft').touch()
                    live.wait_for(lambda: 'keep ' not in self.reply_screen())
                    self.cli('reply', question['message'], 'once')
                    self.assertFalse(self.status()['pendingInput'])
                    self.assert_reply_worker_finished(question, 'accepted', earlier=('definitely-not-sent',))
                    self.assertEqual(writes, ['pane.send_input'])

    def drop_close(self, calls):
        def read(stream):
            frame = stream.readline()
            request = json.loads(frame)
            if request['method'] == 'pane.close':
                calls.append(request['id'])
                return None
            return frame
        return self.enterContext(self.service.fault_proxy(lambda req, reply: reply, read_frame=read))

    def callback(self, *flags):
        env = self.env | {'WFC_DISPATCH':self.dispatch_id, 'WFC_ROUND':str(self.row['round']),
                          'WFC_ATTEMPT':self.row['attempt']}
        return subprocess.Popen([live.DRIVER, 'done', '--report', self.root/'report.md',
                                 '--attempt', self.row['attempt'], *flags], env=env, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def assert_pending_views(self, status):
        named = self.status()
        active = self.cli('status', '--active')
        self.assertEqual([r['dispatch'] for r in active['dispatches']], [self.dispatch_id])
        listed, = active['dispatches']
        self.assertEqual(active['reclaimable'], [])
        self.assertTrue(named['closePending'])
        self.assertEqual(named['status'], status)
        for field in ('dispatch', 'round', 'attempt', 'rounds', 'status', 'state', 'closePending'):
            self.assertEqual(listed[field], named[field])
        return named

    def test_close_during_first_done_git_sample(self):
        calls = []
        self.dispatch(endpoint=self.drop_close(calls))
        (self.root/'report.md').write_text('finished')
        git = self.root/'git'
        git.write_text('#!/bin/sh\ntouch sampling\nwhile [ ! -f resume ]; do sleep .02; done\nexit 1\n')
        git.chmod(0o700)
        self.env['PATH'] = str(self.root) + ':' + self.env['PATH']
        with self.callback() as callback:
            try:
                live.wait_for(lambda: (self.root/'sampling').exists())
                self.unconfirmed_close('incomplete')
            finally:
                (self.root/'resume').touch()
            _, diagnostic = callback.communicate(timeout=10)
            self.assertEqual(callback.returncode, 1, diagnostic)
            self.assertIn('closing or closed', diagnostic)
        self.assertEqual(self.assert_pending_views('unknown')['messages'], [])
        self.unconfirmed_close('pending close')
        self.service.api('pane.close', pane_id=self.h['pane'])
        live.wait_for(lambda: self.cli('close', self.dispatch_id)['result'] == 'completed')
        with self.callback() as callback:
            _, diagnostic = callback.communicate(timeout=10)
            self.assertEqual(callback.returncode, 1, diagnostic)
        self.assertEqual(self.status()['messages'], [])
        self.assertEqual(len(calls), 1)

    def test_question_replays_after_herdr_close_intent(self):
        calls = []
        self.dispatch(endpoint=self.drop_close(calls))
        self.assertNotIn('workerCallbackHash', self.row)
        self.assertNotIn('workerCallbackHash', self.status())
        self.env['WFC_DISPATCH'] = self.dispatch_id
        first = ('ask', 'First question?', '--attempt', self.row['attempt'], '--message-id', 'first')
        receipt = self.cli(*first)
        self.unconfirmed_close('incomplete')
        self.cli('ask', 'Late question?', '--attempt', self.row['attempt'],
                 error='closing or closed')
        message, = self.cli('wait', '--scope', self.row['scope'], '--timeout', '0')['messages']
        self.assertEqual(message['messageId'], receipt['messageId'])
        confirmed = self.cli('ack', receipt['messageId'])
        self.assertEqual(self.cli(*first), {**confirmed, 'duplicate': True})
        self.assertEqual(len(calls), 1)

    def test_done_receipt_and_active_list_through_close(self):
        calls = []
        self.dispatch(endpoint=self.drop_close(calls))
        self.submit('finish')
        message, = live.wait_for(lambda: self.status()['messages'])
        self.assertEqual(self.cli('status', '--active')['reclaimable'][0]['dispatch'], self.dispatch_id)
        self.unconfirmed_close('incomplete')
        for confirmed in (False, True):
            if confirmed:
                self.service.api('pane.close', pane_id=self.h['pane'])
                live.wait_for(lambda: self.status()['backendAlive'] is False)
                self.assert_pending_views('completed')
                self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
            else:
                self.assert_pending_views('completed')
                self.unconfirmed_close('pending close')
            with self.callback() as callback:
                out, error = callback.communicate(timeout=10)
                self.assertEqual(callback.returncode, 0, error)
                original = json.loads(out)
                self.assertTrue(original['duplicate'])
                self.assertEqual((original['message'], original['messageId']), (message['id'], message['messageId']))
            with self.callback('--message-id', 'different') as callback:
                _, error = callback.communicate(timeout=10)
                self.assertEqual(callback.returncode, 1, error)
            self.assertEqual(self.status()['messages'], [message])
        Path(self.h['receipt']).write_text('{')
        archived = self.status()
        self.assertEqual((archived['status'], archived['worker']['state'], archived['backendAlive']),
                         ('completed', 'cancelled', False))
        self.assertEqual(archived['messages'], [message])
        self.assertTrue(archived['launchDiagnostic'])
        active = self.cli('status', '--active')
        self.assertEqual((active['dispatches'], active['reclaimable']), ([], []))
        self.assertEqual(len(calls), 1)

    def test_close_request_not_forwarded(self):
        self.check_uncertain_close(forward=False)

    def test_close_executed_without_reply(self):
        self.check_uncertain_close(forward=True)

    def check_uncertain_close(self, forward):
        requested, executed = [], []
        def read(stream):
            frame = stream.readline()
            req = json.loads(frame)
            if req['method'] == 'pane.close':
                # See the durable intent while the client is still inside the RPC.
                assignment = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
                intent = json.loads(assignment.read_bytes())['audit'][-1]
                self.assertEqual(intent['result'], 'pending')
                for key in ('dispatch','round','attempt'):
                    self.assertEqual(intent[key], self.row[key])
                for key in ('workspace','terminal','pane','service','nonce'):
                    self.assertEqual(intent['target'][key], self.h[key])
                requested.append(req['id'])
                if not forward:
                    return None
            return frame
        def reply(req, response):
            if req['method'] == 'pane.close':
                self.assertEqual(response['result']['type'], 'ok')
                executed.append(req['id'])
                return None
            return response

        with self.service.fault_proxy(reply, read_frame=read) as proxy:
            self.dispatch(endpoint=proxy)
            other, sibling_pid = self.sibling()
            first = self.cli('close', self.dispatch_id)
            self.assertEqual(first['result'], 'uncertain')
            if forward:
                receipt = Path(self.h['receipt'])
                live.wait_for(lambda: json.loads(receipt.read_bytes())['category'] == 'process_exited')
            view = self.status()
            self.assertTrue(view['closePending'])
            self.assertEqual(view['status'], 'unknown')
            self.assertEqual(view['audit'][0]['result'], 'uncertain')
            self.submit('must not arrive', error='close still pending')
            self.reconcile(error='close still pending')
            if not forward:
                self.assertTrue(view['backendAlive'])
                Path(self.h['receipt']).unlink()
                missing = self.status()
                self.assertIsNone(missing['launch'])
                self.assertTrue(missing['closePending'])
                self.assertEqual(missing['audit'][0]['result'], 'uncertain')
                self.unconfirmed_close('pending close')
                self.assertFalse((self.root/'events').exists())
                # An independent operator removes this pane on the real service.
                self.service.api('pane.close', pane_id=self.h['pane'])
            live.wait_for(lambda: self.cli('close', self.dispatch_id)['result'] == 'completed')
            self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
            end = self.status()
            self.assertFalse(end['closePending'])
            self.assertEqual(end['worker']['state'], 'cancelled')
            event, = end['audit']
            self.assertEqual(event['result'], 'completed')
            self.assertEqual(len(requested), 1)
            self.assertEqual(executed, requested if forward else [])
            self.assertEqual(self.service.api('pane.process_info', pane_id=other)['process_info']['shell_pid'], sibling_pid)
            print(json.dumps(dict(dispatch=self.dispatch_id, forward=forward, requests=requested,
                                  executed=executed, first=first['result'], confirmed=event['result'])))

    def test_killed_close_client_leaves_queryable_intent(self):
        from threading import Event
        arrived, release = Event(), Event()
        requests = []
        def hold(stream):
            frame = stream.readline()
            req = json.loads(frame)
            if req['method'] == 'pane.close':
                requests.append(req['id'])
                arrived.set()
                self.assertTrue(release.wait(15))
                return None
            return frame
        with self.service.fault_proxy(lambda req, reply: reply, read_frame=hold) as proxy:
            self.dispatch(endpoint=proxy)
            with subprocess.Popen([live.DRIVER, 'close', self.dispatch_id], env=self.env) as client:
                try:
                    self.assertTrue(arrived.wait(10))
                finally:
                    client.kill()
                    client.wait(timeout=5)
                    release.set()
            view = self.status()
            self.assertTrue(view['closePending'])
            self.assertEqual(view['audit'][0]['result'], 'pending')
            self.submit('blocked', error='close still pending')
            self.unconfirmed_close('pending close')
            self.assertEqual(len(requests), 1)
            self.service.api('pane.close', pane_id=self.h['pane'])
            live.wait_for(lambda: self.status()['backendAlive'] is False)
            self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
            self.assertEqual(len(requests), 1)

    def test_close_definitely_not_sent_allows_new_attempt(self):
        hidden = self.root/'temporarily-offline.sock'
        calls = []
        armed = False
        def intercept(req, reply):
            nonlocal armed
            if req['method'] == 'pane.close':
                calls.append(req['id'])
            if armed and req['method'] == 'session.snapshot':
                path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
                if json.loads(path.read_bytes())['audit']:
                    # The final ownership guard has a real reply. The mutating
                    # connection cannot be opened while the socket is absent.
                    armed = False
                    proxy.rename(hidden)
            return reply
        with self.service.fault_proxy(intercept) as proxy:
            self.dispatch(endpoint=proxy)
            armed = True
            try:
                self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'definitely-not-sent')
            finally:
                if hidden.exists():
                    hidden.rename(proxy)
            self.assertEqual(calls, [])
            self.assertFalse(self.status()['closePending'])
            self.submit('still active')
            self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
            self.assertEqual(len(calls), 1)
            self.assertEqual([e['result'] for e in self.status()['audit']], ['definitely-not-sent','completed'])
            self.assertEqual((self.root/'events').read_text().splitlines(), ['still active'])
            print(json.dumps(dict(dispatch=self.dispatch_id, initialRequests=0, retryRequests=calls)))

    def test_named_status_during_done_then_exit(self):
        self.worker.write_text("""import os, time, subprocess
from pathlib import Path
while not Path('finish-now').exists():
    time.sleep(.02)
Path(os.environ['WFC_REPORT']).write_text('finished')
reply = subprocess.check_output([os.environ['WFC_BIN'], 'done', '--attempt', os.environ['WFC_ATTEMPT'],
                                 '--report', os.environ['WFC_REPORT']])
Path('done-received').write_bytes(reply)
""")
        armed = False
        def gate(req, reply):
            nonlocal armed
            if armed and req['method'] == 'ping':
                armed = False
                (self.root/'finish-now').touch()
                live.wait_for(lambda: (self.root/'done-received').exists())
                live.wait_for(lambda: json.loads(Path(self.h['receipt']).read_bytes())['category']=='process_exited')
            return reply
        with self.service.fault_proxy(gate) as proxy:
            self.dispatch(endpoint=proxy)
            armed = True
            v = self.status()
            self.assertEqual(v['status'], 'completed')
            done, = v['messages']
            self.assertEqual(done['messageId'], json.loads((self.root/'done-received').read_bytes())['messageId'])
            self.assertEqual(v['launch']['category'], 'process_exited')
            self.assertEqual(v['launch']['exitCode'], 0)
            self.assertEqual(v['worker']['exit_code'], v['launch']['exitCode'])
            self.cli('close', self.dispatch_id)

    def test_invalid_audit_fields_and_endpoint(self):
        self.dispatch()
        base = [live.DRIVER, 'herdr-reconcile', self.dispatch_id, '--pane', self.h['pane'],
                '--terminal', self.h['terminal']]
        for fields, expected in [
            (['--actor','  ','--evidence','checked'], 1),
            (['--actor','tester','--evidence',''], 1),
            (['--actor','--evidence','checked'], 2),
            (['--actor','tester','--evidence'], 2),
            (['--evidence','checked'], 2),
        ]:
            p = subprocess.run(base+fields, env=self.env, capture_output=True)
            self.assertEqual(p.returncode, expected, p.stderr)
        self.assertEqual(self.status()['audit'], [])
        self.cli('close', self.dispatch_id)
        state = self.root/'invalid-endpoint-state'
        env = self.env | {'WFC_HERDR_ENDPOINT':b'/tmp/\xff','WFC_STATE':str(state)}
        p = subprocess.run([live.DRIVER,'dispatch','--backend','herdr','--host','codex','--cwd',self.root,
                            '--task',self.task,'--report',self.root/'report.md'],env=env,capture_output=True)
        self.assertEqual(p.returncode, 1)
        self.assertIn(b'UTF-8', p.stderr)
        self.assertFalse(any(p.is_file() for p in state.rglob('*')))

    def test_unreadable_launch_after_confirmed_close(self):
        self.worker.write_text("from pathlib import Path\nimport time\nwhile not Path('exit-now').exists(): time.sleep(.02)\nraise SystemExit(17)")
        self.dispatch()
        (self.root/'exit-now').touch()
        live.wait_for(lambda: self.status()['status'] == 'failed')
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        receipt = Path(self.h['receipt'])
        # Invalid JSON exercises the read error even for a privileged test runner.
        receipt.write_text('{')
        view = self.status()
        self.assertIsNone(view['launch'])
        self.assertTrue(view['launchDiagnostic'])
        self.assertEqual((view['status'], view['worker']['exit_code'], view['rounds'][0]['result']['exit_code']),
                         ('failed', 17, 17))
        self.assertEqual((view['backendResult'], view['backendAlive'], view['closePending']),
                         ('completed', False, False))
        # Neither a different close target nor absent close evidence certifies this execution.
        path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
        row = json.loads(path.read_bytes())
        row['audit'][-1]['target']['nonce'] = 'other-execution'
        for events in (row['audit'], []):
            path.write_text(json.dumps(row | {'audit':events}))
            view = self.status()
            self.assertTrue(view['launchDiagnostic'])
            self.assertEqual((view['status'], view['worker']['state'], view['backendAlive']),
                             ('unknown', 'unknown', None))

    def test_conflicting_stop_evidence_cannot_close_live_writers(self):
        calls = []
        self.dispatch(endpoint=self.drop_close(calls))
        sibling, sibling_pid = self.sibling()
        binding = self.status()['backendHandle']
        root_pid = binding['writerProcesses'][0]['pid']
        path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
        original = json.loads(path.read_bytes())
        for evidence, writers, diagnostic in [
            ('   ', binding['writerProcesses'], 'blank Herdr writer stop evidence'),
            ('claimed stopped', binding['writerProcesses'], 'conflicts with recorded writers'),
            ('   ', [], 'blank Herdr writer stop evidence'),
        ]:
            handle = binding | {'writerStoppedEvidence':evidence, 'writerProcesses':writers}
            path.write_text(json.dumps(original | {'backendHandle':handle}))
            view = self.assert_pending_views('unknown')
            self.assertIsNone(view['backendAlive'])
            self.assertIn(diagnostic, view['backendDiagnostic'])
            self.unconfirmed_close(diagnostic)
            self.unconfirmed_close(diagnostic)
            self.assertEqual(self.status()['audit'], [])
            self.assertEqual(self.service.api('pane.process_info', pane_id=self.h['pane'])['process_info']['shell_pid'],
                             root_pid)
            self.assertEqual(self.service.api('pane.process_info', pane_id=sibling)['process_info']['shell_pid'],
                             sibling_pid)
        self.assertEqual(calls, [])

    def test_old_worker_data_does_not_override_verified_resource_close(self):
        self.dispatch()
        sibling, sibling_pid = self.sibling()
        self.submit('finish')
        first, = live.wait_for(lambda: self.status()['messages'])
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
        row = json.loads(path.read_bytes())
        row['audit'][-1]['workerOnClose'] = {'state':'exited', 'exit_code':'legacy'}
        for archived_result in ('completed', 'uncertain'):
            row['audit'][-1]['result'] = archived_result
            path.write_text(json.dumps(row))
            if archived_result == 'uncertain':
                self.assert_pending_views('completed')
            for _ in range(2):
                close = self.cli('close', self.dispatch_id)
                self.assertEqual(close['result'], 'completed')
                self.assertIn('invalid closed worker outcome', close['closeWorkerDiagnostic'])
                view = self.status()
                self.assertEqual((view['backendResult'], view['backendAlive'], view['closePending']),
                                 ('completed', False, False))
                self.assertEqual(view['status'], 'completed')
                self.assertIn('invalid closed worker outcome', view['closeWorkerDiagnostic'])
                self.assertEqual(view['messages'], [first])
            with self.callback() as callback:
                out, error = callback.communicate(timeout=10)
                self.assertEqual(callback.returncode, 0, error)
                self.assertEqual(json.loads(out)['messageId'], first['messageId'])
        self.assertEqual(self.service.api('pane.process_info', pane_id=sibling)['process_info']['shell_pid'],
                         sibling_pid)

    def test_close_archive_requires_the_original_root_writer(self):
        requests = []
        def count_close(req, reply):
            if req['method'] == 'pane.close':
                requests.append(req['id'])
            return reply

        endpoint = self.enterContext(self.service.fault_proxy(count_close))
        self.worker.write_text("from pathlib import Path\nimport time\nwhile not Path('exit-now').exists(): time.sleep(.02)\nraise SystemExit(17)")
        self.dispatch(endpoint=endpoint)
        sibling, sibling_pid = self.sibling()
        (self.root/'exit-now').touch()
        live.wait_for(lambda: self.status()['status'] == 'failed')
        receipt = Path(self.h['receipt'])
        exit_receipt = receipt.read_bytes()
        receipt.write_text(json.dumps({'category':'process_exited', 'exitCode':'invalid'}))
        self.unconfirmed_close('invalid host launch exit code')
        view = self.status()
        self.assertEqual(view['audit'], [])
        self.assertIn('invalid host launch exit code', view['launchDiagnostic'])
        self.assertEqual(requests, [])
        receipt.write_bytes(exit_receipt)
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        initial_requests = requests.copy()
        path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
        original = json.loads(path.read_bytes())
        event, = original['audit']
        target = event['target']
        root, other, *_ = target['writerProcesses']

        def archive_with(**changes):
            path.write_text(json.dumps(original | {'audit':[event | {'target':target | changes}]}))

        # A matching root can be verified even if the current binding learned more writers.
        archive_with(writerProcesses=[root])
        receipt.write_text('{')
        view = self.status()
        self.assertEqual((view['status'], view['worker']['exit_code'], view['backendAlive']), ('failed', 17, False))
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

        for changes, diagnostic in [
            ({'writerProcesses':[other, root]}, 'root writer'),
            ({'writerProcesses':[other]}, 'root writer'),
            ({'writerProcesses':[]}, 'root writer'),
            ({'writerProcesses':[root | {'creation':'other-birth'}]}, 'root writer'),
            ({'writerStoppedEvidence':'claimed stopped'}, 'conflicts with recorded writers'),
            ({'nonce':'other-execution'}, 'close intent target changed'),
        ]:
            with self.subTest(changes=changes):
                archive_with(**changes)
                for content in (exit_receipt, b'{'):
                    receipt.write_bytes(content)
                    view = self.assert_pending_views('unknown')
                    listed, = self.cli('status', '--active')['dispatches']
                    self.assertIsNone(view['backendAlive'])
                    self.assertEqual(view['backendResult'], 'uncertain')
                    self.assertIn(diagnostic, view['backendDiagnostic'])
                    for field in ('worker', 'backendDiagnostic', 'backendAlive', 'launchDiagnostic'):
                        self.assertEqual(view.get(field), listed.get(field))
                    if content == exit_receipt:
                        self.assertEqual((view['worker']['state'], view['worker']['exit_code']), ('exited', 17))
                        self.assertIsNone(view.get('launchDiagnostic'))
                    else:
                        self.assertEqual(view['worker']['state'], 'unknown')
                        self.assertTrue(view['launchDiagnostic'])
                self.cli('close', self.dispatch_id, error=diagnostic)

        # The legacy archive carries no target. A rooted binding still requires root evidence.
        path.write_text(json.dumps(original | {'audit':[]}))
        legacy = path.with_suffix(f".{self.row['attempt']}.closed")
        archive = dict(dispatch=self.dispatch_id, round=self.row['round'], attempt=self.row['attempt'],
                       worker=event['workerOnClose'])
        legacy.write_text(json.dumps(archive))
        self.assertIn('legacy archive', self.assert_pending_views('unknown')['backendDiagnostic'])
        self.cli('close', self.dispatch_id, error='legacy archive')

        stopped = original['backendHandle'] | {'writerProcesses':[], 'writerStoppedEvidence':'   '}
        path.write_text(json.dumps(original | {'audit':[], 'backendHandle':stopped}))
        self.cli('close', self.dispatch_id, error='legacy archive')
        stopped['writerStoppedEvidence'] = 'inspected and stopped'
        path.write_text(json.dumps(original | {'audit':[], 'backendHandle':stopped}))
        legacy.write_text(json.dumps(archive | {'worker':{'state':'running'}}))
        close = self.cli('close', self.dispatch_id)
        self.assertEqual(close['result'], 'completed')
        self.assertIn('invalid closed worker outcome', close['closeWorkerDiagnostic'])
        view = self.status()
        self.assertIs(view['backendAlive'], False)
        self.assertFalse(view['closePending'])
        self.assertIn('invalid closed worker outcome', view['closeWorkerDiagnostic'])

        self.assertEqual(self.service.api('pane.process_info', pane_id=sibling)['process_info']['shell_pid'],
                         sibling_pid)
        os.kill(sibling_pid, 0)
        self.assertEqual(requests, initial_requests)

    def test_new_done_selects_only_its_execution_from_close_history(self):
        self.dispatch()
        self.submit('finish')
        first, = live.wait_for(lambda: self.status()['messages'])
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        # Install an offline next-round assignment after a real first round.
        path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
        row = json.loads(path.read_bytes())
        old, = row['audit']
        report = self.root/'second.md'
        report.write_text('second round')
        row.update(round=2, attempt='a-0123456789ab', report=str(report), rounds=self.status()['rounds'])
        row['backendHandle'].update(round=2, attempt=row['attempt'])
        self.env.update(WFC_DISPATCH=self.dispatch_id, WFC_ROUND='2', WFC_ATTEMPT=row['attempt'])
        args = ('done', '--report', report, '--attempt', row['attempt'], '--message-id', 'second-key')
        current = old | {'dispatch':self.dispatch_id, 'round':2, 'attempt':row['attempt'], 'result':'uncertain'}
        # Each unrelated event differs from the current identity in at least one field.
        history = [old, current | {'dispatch':'d-other'}, current | {'round':1}, current | {'attempt':old['attempt']}]
        path.write_text(json.dumps(row | {'audit':[current, *history]}))
        self.cli(*args, error='closing or closed')
        path.write_text(json.dumps(row | {'audit':history}))
        accepted = self.cli(*args)
        self.assertFalse(accepted['duplicate'])
        repeated = self.cli(*args)
        self.assertTrue(repeated['duplicate'])
        self.assertEqual(repeated['messageId'], accepted['messageId'])
        messages = self.status()['messages']
        self.assertEqual([m['messageId'] for m in messages], [first['messageId'], accepted['messageId']])

    def test_worker_fails_before_close_with_surviving_writer(self):
        self.check_worker_exit_order(worker_first=True)

    def test_writer_stops_before_worker_exits_during_close(self):
        self.check_worker_exit_order(worker_first=False)

    def check_worker_exit_order(self, worker_first):
        calls = []
        command = [sys.executable, str(Path(live.__file__).with_name('job_control.py')), '--controlled-exit']
        self.dispatch(endpoint=self.drop_close(calls), command=command)
        (self.root/'start-job').touch()
        pid = int(live.wait_for(lambda: (self.root/'job-pid').exists() and (self.root/'job-pid').read_text()))
        live.wait_for(lambda: any(w['pid'] == pid for w in self.status()['backendHandle']['writerProcesses']))
        (self.root/'background-job').touch()
        live.wait_for(lambda: (self.root/'backgrounded').exists())
        if worker_first:
            (self.root/'exit-main').touch()
            live.wait_for(lambda: self.status()['status'] == 'failed')
            os.kill(pid, 0)
            live.wait_for(lambda: self.h['pane'] not in [p['pane_id'] for p in self.service.api('session.snapshot')['snapshot']['panes']])
        self.unconfirmed_close('writer' if worker_first else 'incomplete')
        pending_status = 'failed' if worker_first else 'unknown'
        view = self.assert_pending_views(pending_status)
        if worker_first:
            self.assertEqual(view['rounds'][0]['result']['exit_code'], 17)
            Path(self.h['receipt']).unlink()
            self.assertEqual(self.assert_pending_views('failed')['worker']['exit_code'], 17)
        (self.root/'stop-job').touch()
        def writer_stopped():
            try:
                return Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].startswith('Z')
            except FileNotFoundError:
                return True
        live.wait_for(writer_stopped)
        if not worker_first:
            self.assert_pending_views('unknown')
            (self.root/'exit-main').touch()
            live.wait_for(lambda: self.status()['launch'].get('exitCode') == 17)
        view = self.assert_pending_views(pending_status)
        if not worker_first:
            self.assertEqual(view['launch']['exitCode'], 17)
            # Ensure the overlap is exercised even if CI ran the worker
            # beyond the short startup window before releasing it.
            path = self.handoff_record(self.row)
            original = json.loads(path.read_bytes())
            event, = original['audit']
            history = view['messages']
            if not any(message.get('kind') == 'startup_failure' for message in history):
                notice = {'kind':'startup_failure', 'type':'exited', 'messageId':'fixture-old-startup',
                          'dispatch':self.dispatch_id, 'round':self.row['round'], 'attempt':self.row['attempt'],
                          'fault':{'category':'process_exited', 'exitCode':17}}
                history = [*history, notice]
                original['messages'] = [*original.get('messages', []), notice]
                path.write_text(json.dumps(original))
            pending = self.assert_pending_views('unknown')
            self.assertEqual(pending['messages'], history)
            self.assertEqual(pending['audit'], [event])
        live.wait_for(lambda: self.cli('close', self.dispatch_id)['result'] == 'completed')
        final = self.status()
        self.assertFalse(final['closePending'])
        self.assertEqual(final['status'], 'failed' if worker_first else 'cancelled')
        if worker_first:
            self.assertEqual(final['worker']['exit_code'], 17)
        else:
            self.assertEqual(final['launch']['exitCode'], 17)
            self.assertEqual(final['messages'], history)
            self.assertEqual(final['audit'][0]['id'], event['id'])
        self.assertEqual(len(calls), 0 if worker_first else 1)

    def test_writer_survives_pane_close(self):
        self.dispatch(script=Path(live.__file__).with_name('job_control.py'))
        other, pid = self.sibling()
        (self.root/'start-job').touch()
        job = int(live.wait_for(lambda: (self.root/'job-pid').exists() and (self.root/'job-pid').read_text()))
        def writers():
            writers = self.status()['backendHandle']['writerProcesses']
            return writers if any(w['pid']==job for w in writers) else None
        known = live.wait_for(writers)
        (self.root/'background-job').touch()
        live.wait_for(lambda: (self.root/'backgrounded').exists())
        self.reconcile()
        self.assertTrue(all(w in self.status()['backendHandle']['writerProcesses'] for w in known))
        self.unconfirmed_close('writer')
        self.assertIsNone(self.status()['backendAlive'])
        (self.root/'stop-job').touch()
        live.wait_for(lambda: self.status()['backendAlive'] is False)
        self.cli('close', self.dispatch_id)
        self.assertEqual(self.service.api('pane.process_info',pane_id=other)['process_info']['shell_pid'],pid)

    def test_worker_that_immediately_detaches_is_still_a_writer(self):
        self.worker.write_text("""import os, time
from pathlib import Path
os.setsid()
Path('detached').write_text(str(os.getpid()))
while not Path('release').exists():
    time.sleep(.02)
""")
        def after_detach(req, reply):
            if req['method'] == 'layout.apply':
                live.wait_for(lambda: (self.root/'detached').exists())
            return reply
        proxy_context = self.service.fault_proxy(after_detach)
        self.dispatch(endpoint=self.enterContext(proxy_context))
        pid = int(live.wait_for(lambda: (self.root/'detached').exists() and (self.root/'detached').read_text()))
        self.assertIn(pid, [w['pid'] for w in self.status()['backendHandle']['writerProcesses']])
        self.unconfirmed_close('writer')
        os.kill(pid, 0)
        (self.root/'release').touch()
        live.wait_for(lambda: self.status()['backendAlive'] is False)
        self.cli('close', self.dispatch_id)

    def test_reused_ids_on_new_service_require_the_original_nonce(self):
        self.dispatch()
        original = self.row
        self.service.stop()
        self.service.start('replacement')
        self.dispatch()
        replacement = self.row
        for key in ('workspace', 'pane'):
            self.assertEqual(original['backendHandle'][key], replacement['backendHandle'][key])
        self.row = original
        self.dispatch_id, self.h = self.row['dispatch'], self.row['backendHandle']
        self.assertNotEqual(self.h['nonce'], replacement['backendHandle']['nonce'])
        self.reconcile(error='ownership mismatch', terminal=replacement['backendHandle']['terminal'],
                       old_writer_evidence='old service stopped')
        self.unconfirmed_close('identity changed')
        self.assertFalse(self.status()['audit'])
        self.assertTrue(self.cli('status', '--dispatch', replacement['dispatch'])['dispatches'][0]['backendAlive'])
        self.cli('close', replacement['dispatch'])

    def test_rebind_restored_metadata_requires_proof_and_keeps_claim(self):
        self.dispatch()
        self.service.stop()
        self.service.start('restored')
        self.submit('lost delivery must stay claimed', error='uncertain')
        restored = self.service.api('workspace.create', cwd=str(self.root), label='restored', focus=False)
        workspace = restored['workspace']['workspace_id']
        # Restore known metadata explicitly on the new test service. Its PID
        # and terminal root really differ from the original saved identities.
        self.service.api('workspace.report_metadata', workspace_id=workspace, source='wfc',
                         tokens={'wfc_owner':self.h['nonce'],'wfc_attempt':self.row['attempt']})
        pane = restored['root_pane']
        mapping = dict(workspace=workspace, pane=pane['pane_id'], terminal=pane['terminal_id'])
        self.reconcile(error='old writer evidence', **mapping)
        result = self.reconcile(old_writer_evidence='observed old server and writers exit', **mapping)
        view = self.status()
        self.assertFalse(result['inputReplayed'])
        self.assertTrue(view['backendAlive'])
        self.assertTrue(view['pendingInput'])
        self.assertIsNone(view['launch'])
        self.assertEqual(view['backendHandle']['service']['pid'], self.service.server.pid)
        self.assertNotEqual(view['backendHandle']['writerProcesses'][0], self.h['writerProcesses'][0])
        self.submit('must stay blocked', error='pending')
        self.assertFalse((self.root/'events').exists())
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def test_legacy_authority_or_later_round_is_left_pending(self):
        self.dispatch()
        state = Path(self.env['WFC_STATE'])
        import sqlite3
        with sqlite3.connect(state/'router.db') as db:
            db.execute('create table marker(x)')
        self.reconcile(error='legacy router database')
        self.cli('close', self.dispatch_id, error='legacy router database')
        self.assertIsNone(self.status()['backendAlive'])
        (state/'router.db').unlink()
        path = state/'dispatches'/(self.dispatch_id+'.json')
        row = json.loads(path.read_bytes())
        row['audit'] = [dict(operation='close', result='pending', dispatch=self.dispatch_id, round=1,
                             attempt=row['attempt'], target=self.h, workerOnClose=dict(state='exited', exit_code=17))]
        row.update(round=2, attempt='a-abcdef123456')
        path.write_text(json.dumps(row))
        self.reconcile(error='R1')
        self.submit('stale', error='R1')
        self.assertIsNone(self.status()['backendAlive'])
        view = self.status()
        self.assertEqual(view['audit'], row['audit'])
        self.assertEqual(view['status'], 'unknown')
        self.assertEqual(view['worker']['state'], 'unknown')
        self.assertFalse((self.root/'events').exists())

    def test_reconcile_rechecks_current_assignment_at_commit(self):
        change = None
        def inject(req, reply):
            nonlocal change
            if req['method'] == 'pane.process_info' and change:
                path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
                latest = json.loads(path.read_bytes())
                latest.update(change)
                path.write_text(json.dumps(latest))
                change = None
            return reply
        with self.service.fault_proxy(inject) as proxy:
            self.dispatch(endpoint=proxy)
            self.status()
            change = {'controllerOwner':{'generation':2}}
            self.reconcile(error='assignment changed')
            self.assertFalse(self.status()['audit'])
            self.assertIsNone(self.status()['backendAlive'])
            self.reconcile(error='authorization')
            self.submit('stale', error='authorization')
            self.cli('close',self.dispatch_id,error='authorization')
            self.assertFalse((self.root/'events').exists())

    def test_opencode_prefill_only_sends_enter_for_this_attempt(self):
        self.worker.write_text("""import os, sys
from pathlib import Path
print('┃ Current attempt ' + os.environ['WFC_ATTEMPT'] + '.', flush=True)
print('┃ model · cwd', flush=True)
print('╹▀▀▀▀', flush=True)
for line in sys.stdin:
    with Path('enters').open('a') as f: print(repr(line), file=f)
""")
        self.dispatch(host='opencode')
        self.assertFalse(self.status()['pendingInput'])
        contents = live.wait_for(lambda: (self.root/'enters').exists() and (self.root/'enters').read_text())
        self.assertEqual(contents.splitlines(), [repr('\n')])
        self.cli('close', self.dispatch_id)

    def test_uncertain_first_enter_preserves_round_and_claim(self):
        self.worker.write_text("""import os, sys
from pathlib import Path
print('┃ Current attempt ' + os.environ['WFC_ATTEMPT'] + '.', flush=True)
print('┃ model · cwd', flush=True)
print('╹▀▀▀▀', flush=True)
for line in sys.stdin:
    with Path('enters').open('a') as f: print(repr(line), file=f)
""")
        sends = []
        def lose(req, reply):
            if req['method'] == 'pane.send_keys':
                sends.append(req['params']['keys'])
                return None
            return reply
        with self.service.fault_proxy(lose) as proxy:
            self.dispatch(host='opencode', endpoint=proxy, error='uncertain')
            self.reconcile()
            view = self.status()
            self.assertTrue(view['backendAlive'])
            self.assertEqual(view['status'], 'unknown')
            self.assertEqual(view['rounds'][0]['state'], 'unknown')
            self.assertTrue(view['pendingInput'])
            self.assertEqual(sends, [['Enter']])
            self.assertEqual((self.root/'enters').read_text().splitlines(), [repr('\n')])
            self.unconfirmed_close('unresolved')

    @unittest.skipUnless(os.environ.get('WFC_HERDR_TEST_CODEX'), 'optional isolated Codex login screen')
    def test_codex_login_screen(self):
        import tempfile
        home = Path(self.enterContext(tempfile.TemporaryDirectory(prefix='.herdr-codex-', dir=Path.cwd())))
        (home/'codex').mkdir(mode=0o700)
        self.env.update(HOME=str(home), CODEX_HOME=str(home/'codex'))
        self.dispatch(command=[os.environ['WFC_HERDR_TEST_CODEX'], '--no-daemon'])
        def login():
            screen = self.service.api('pane.read', pane_id=self.h['pane'], source='recent', format='text',
                                      strip_ansi=True, lines=100)['read']['text'].lower()
            return 'sign in' in screen or 'choose how you want to use codex' in screen
        live.wait_for(login, 25)
        self.cli('close', self.dispatch_id)

    def test_restarted_service_and_absent_writers(self):
        self.dispatch()
        self.service.stop()
        self.assertIsNone(self.status()['backendAlive'])
        self.service.start('replacement')
        self.assertEqual(self.status()['backendResult'], 'uncertain')
        Path(self.h['receipt']).unlink()  # simulate loss of the original launch receipt
        self.reconcile(error='old writer evidence')
        self.assertFalse(self.status()['audit'])
        self.reconcile(old_writer_evidence='old service and worker reaped')
        self.service.stop()
        view = self.status()
        self.assertFalse(view['backendAlive'])
        self.assertEqual(view['backendHandle']['state'], 'not-started')
        self.assertIsNone(view['backendHandle']['receipt'])
        self.assertEqual(json.loads(view['audit'][0]['resolution']['evidence'])['writers'], [])
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        path = Path(self.env['WFC_STATE'])/'dispatches'/(self.dispatch_id+'.json')
        row = json.loads(path.read_bytes())
        event = row['audit'][-1]
        original = event['target']['writerStoppedEvidence']
        for proof in ('', '  ', 'different stop evidence'):
            event['target']['writerStoppedEvidence'] = proof
            path.write_text(json.dumps(row))
            self.assertIn('close intent target changed', self.assert_pending_views('unknown')['backendDiagnostic'])
            self.cli('close', self.dispatch_id, error='close intent target changed')
        event['target']['writerStoppedEvidence'] = original
        path.write_text(json.dumps(row))
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        self.assertFalse(self.status()['backendAlive'])


if __name__ == '__main__':
    unittest.main()
