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
        row.update(round=2, attempt='a-abcdef123456')
        path.write_text(json.dumps(row))
        self.reconcile(error='R1')
        self.submit('stale', error='R1')
        self.assertIsNone(self.status()['backendAlive'])
        self.assertEqual(self.status()['audit'], [])
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
        self.cli('close', self.dispatch_id)


if __name__ == '__main__':
    unittest.main()
