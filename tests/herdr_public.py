"""Linux integration: python3 tests/herdr_public.py HERDR WFC [-v]."""
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

    def test_dispatch_current_execution_done_and_close(self):
        self.dispatch()
        self.env['WFC_HERDR_ENDPOINT'] = '/missing/different.sock'
        view = self.status()
        self.assertTrue(view['backendAlive'])
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

    def setup_reply_worker(self, endpoint=None):
        fixture = Path(__file__).resolve().parents[1] / 'tools/check_tmux.py'
        self.dispatch(endpoint=endpoint, command=[
            sys.executable, str(fixture), '--reply-worker', 'empty', str(self.root/'reply.log'),
        ])
        question = self.root/'question.json'
        receipt = json.loads(live.wait_for(lambda: question.exists() and question.read_bytes()))
        live.wait_for(lambda: 'Ask Codex to do anything' in self.service.api(
            'pane.read', pane_id=self.h['pane'], source='recent', format='text',
            strip_ansi=True, lines=50)['read']['text'])
        return receipt

    def assert_reply_worker_finished(self, question, outcome):
        reply, = [m for m in self.status()['messages'] if m['type'] == 'reply']
        self.assertEqual(reply['inputResult'], outcome)
        self.assertEqual(reply['questionMessageId'], question['messageId'])
        self.assertEqual(reply['to'], question['message'])
        for key in ('dispatch', 'round', 'attempt'):
            self.assertEqual(reply[key], self.row[key])
        self.cli('reply', question['message'], 'again', error='already answered')
        accepted = self.root/'accepted'
        received = live.wait_for(lambda: accepted.exists() and accepted.read_text())
        self.assertEqual(received.splitlines(), [f"[router] Answer to question {question['message']}: once"])
        first, = self.cli('wait', '--timeout', 0)['messages']
        self.assertEqual(first['type'], 'question')
        (self.root/'finish').touch()
        done, = self.cli('wait', '--timeout', 8)['messages']
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
        self.assert_pending_views(pending_status)
        live.wait_for(lambda: self.cli('close', self.dispatch_id)['result'] == 'completed')
        final = self.status()
        self.assertFalse(final['closePending'])
        self.assertEqual(final['status'], 'failed' if worker_first else 'cancelled')
        if worker_first:
            self.assertEqual(final['worker']['exit_code'], 17)
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
