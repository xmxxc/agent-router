"""Public offline resume with a private Herdr server and controlled Codex worker."""
import json
from pathlib import Path
import subprocess
import sys
import unittest

import herdr_public


class OfflineResume(herdr_public.PublicLifecycle):
    effect_methods = {'workspace.create', 'workspace.report_metadata', 'layout.apply',
                      'pane.close', 'pane.send_input', 'pane.send_keys'}

    def setUp(self):
        super().setUp()
        self.root = self.root / 'resume fixture project'
        self.root.mkdir()
        subprocess.run(['git', 'init', '-q', str(self.root)], env=self.env, check=True)
        self.task = self.root / 'task.md'
        self.task.write_text('Resume this task in the saved project conversation.\n')
        self.report = self.root / 'report.md'
        self.report.write_bytes(b'Original work in progress.\n')
        self.state = Path(self.env['WFC_STATE'])
        self.env['CODEX_HOME'] = str(self.root / 'codex-home')
        self.requests = []
        self.keep_original_pane = False
        self.observed_writer = None
        self.hold_zero_layout = False

    def launch_original(self, *, keep_pane=False):
        self.keep_original_pane = keep_pane
        self.endpoint = self.enterContext(self.service.fault_proxy(self.record_request, read_frame=self.request_frame))
        self.dispatch(endpoint=self.endpoint, command=[
            sys.executable, '-u', str(Path(__file__).with_name('resume_worker.py').resolve()),
        ])
        self.original = self.row
        self.path = self.state / 'dispatches' / (self.dispatch_id + '.json')
        self.ledger = self.state / 'messages.json'
        self.launch_log = self.root / 'launches.jsonl'
        self.question = self.receipt('first-question')
        self.assert_claimed('question', self.question)

    def request_frame(self, stream):
        frame = stream.readline()
        if self.keep_original_pane and frame:
            request = json.loads(frame)
            if request['method'] == 'layout.apply':
                self.keep_original_pane = False
                pane = request['params']['root']
                wrapper = Path(__file__).with_name('resume_worker.py').resolve()
                pane['command'] = [sys.executable, str(wrapper), '--keep-original-pane', *pane['command']]
                return (json.dumps(request) + '\n').encode()
        return frame

    def record_request(self, request, response):
        method = request['method']
        self.requests.append(method)
        if self.observed_writer is not None and method == 'pane.process_info':
            info = response['result']['process_info']
            self.assertEqual(info['pane_id'], self.h['pane'])
            # Add an actual fixture-owned PID to the old pane's foreground observation.
            info['foreground_processes'] = [{'pid': self.observed_writer}]
        if self.hold_zero_layout and method == 'layout.apply':
            self.hold_zero_layout = False
            row = json.loads(self.path.read_bytes())
            self.assertEqual(row['audit'][-1]['result'], 'pending')
            self.assertNotEqual(row['attempt'], self.original['attempt'])
            self.zero_receipt = self.wait_exit(Path(row['create']['receipt']), 0)
            self.assertIn('startupZero', self.zero_receipt)
        return response

    def receipt(self, name):
        path = self.root / (name + '.json')
        return json.loads(herdr_public.live.wait_for(lambda: path.exists() and path.read_bytes()))

    def assert_claimed(self, kind, receipt):
        claimed, = self.cli('wait', '--dispatch', self.dispatch_id, '--scope', self.row['scope'],
                            '--timeout', '4')['messages']
        self.assertEqual((claimed['id'], claimed['messageId'], claimed['type']),
                         (receipt['message'], receipt['messageId'], kind))
        ack = self.cli('ack', claimed['messageId'])
        self.assertEqual((ack['message'], ack['messageId'], ack['state']),
                         (receipt['message'], receipt['messageId'], 'processed'))

    def stop_first(self):
        (self.root / 'stop-first').touch()

        def stopped():
            view = self.status()
            launch = view.get('launch') or {}
            return (view['backendAlive'] is False
                    and (launch.get('category'), launch.get('exitCode')) == ('process_exited', 17))

        herdr_public.live.wait_for(stopped, seconds=12)
        self.wait_tree_stop()
        # Keep an actual archive event from the original attempt in the row.
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def wait_tree_stop(self):
        path = Path(self.original['create']['receipt']).with_suffix('.stopped')
        herdr_public.live.wait_for(path.is_file)
        return path

    def wait_exit(self, path, code):
        def exited():
            if path.exists():
                receipt = json.loads(path.read_bytes())
                if (receipt['category'], receipt.get('exitCode')) == ('process_exited', code):
                    return receipt

        return herdr_public.live.wait_for(exited, seconds=5)

    @staticmethod
    def writer_alive(writer):
        try:
            fields = Path(f"/proc/{writer['pid']}/stat").read_bytes().rsplit(b')', 1)[1].split()
        except FileNotFoundError:
            return False
        return (fields[0] not in (b'Z', b'X', b'x')
                and fields[19].decode() == writer['creation'].rsplit(':', 1)[1])

    def snapshot(self):
        launches = sorted(self.state.joinpath('launches').glob('*'))
        return [(path.name, path.read_bytes()) for path in
                [self.path, self.ledger, self.task, self.report, self.launch_log, *launches]]

    def assert_resume_without_effect(self, error):
        before = self.snapshot()
        index = len(self.requests)
        refused = self.cli('resume', self.dispatch_id, error=error)
        self.assertFalse(self.effect_methods.intersection(self.requests[index:]), self.requests[index:])
        self.assertEqual(self.snapshot(), before)
        return refused

    def test_public_resume_reuses_saved_chat_and_task_once(self):
        self.launch_original()
        self.stop_first()
        original_view = self.status()
        ledger = self.ledger.read_bytes()
        report = self.report.read_bytes()
        task = self.task.read_bytes()
        index = len(self.requests)
        resumed = self.cli('resume', self.dispatch_id)
        view = self.status()

        def logged_resume():
            lines = self.launch_log.read_text().splitlines()
            return lines if len(lines) >= 2 else None

        first, second = [json.loads(line) for line in herdr_public.live.wait_for(logged_resume)]
        self.assertEqual((resumed['result'], resumed['launchResult'], resumed['backend']),
                         ('accepted', 'accepted', 'herdr'))
        self.assertEqual((resumed['dispatch'], resumed['round'], resumed['session']),
                         (self.dispatch_id, 1, '11111111-2222-4333-8444-555555555555'))
        self.assertEqual(view['attempt'], resumed['attempt'])
        self.assertNotEqual(view['attempt'], self.original['attempt'])
        self.assertEqual(view['executionId'], resumed['attempt'])
        self.assertEqual(view['backendHandle']['attempt'], resumed['attempt'])
        self.assertEqual(view['create']['receipt'], resumed['receipt'])
        binding = view['create']['herdrBinding']
        self.assertEqual(set(binding), {'endpoint', 'service', 'workspace', 'pane', 'terminal', 'nonce', 'receipt'})
        for field, value in binding.items():
            self.assertEqual(view['backendHandle'][field], value)
        self.assertEqual((view['task'], view['report'], view['hostSession']),
                         (str(self.task), str(self.report), resumed['session']))
        self.assertEqual([(item['round'], item['attempt']) for item in view['rounds']],
                         [(1, first['environment']['attempt']), (1, second['environment']['attempt'])])
        self.assertEqual(view['audit'][:-1], original_view['audit'])
        event = view['audit'][-1]
        for field, expected in [('fromAttempt', self.original['attempt']), ('toAttempt', view['attempt']),
                                ('session', resumed['session']), ('result', 'accepted')]:
            self.assertEqual(event[field], expected)
        self.assertEqual(event['priorWorker']['exit_code'], 17)
        self.assertEqual(event['priorBackendHandle']['pane'], self.h['pane'])
        self.assertEqual(event['priorCreate'], self.original['create'])
        self.assertEqual(original_view['audit'][-1]['closeOperationId'],
                         f'close:{self.original["attempt"]}')
        self.assertTrue(view['backendAlive'])
        self.assertEqual(view['backendHandle']['service'], self.h['service'])
        self.assertEqual(view['backendHandle']['endpoint'], self.h['endpoint'])
        self.assertNotEqual(view['backendHandle']['pane'], self.h['pane'])
        self.assertNotEqual(view['backendHandle']['nonce'], self.h['nonce'])
        self.assertNotEqual(resumed['receipt'], self.h['receipt'])
        self.assertNotEqual(first['pid'], second['pid'])
        self.assertEqual(second['environment'], {'dispatch': self.dispatch_id, 'round': '1',
                                                'attempt': resumed['attempt'], 'task': str(self.task),
                                                'report': str(self.report)})
        self.assertEqual((second['argv'][0], second['argv'][-2]), ('resume', resumed['session']))
        self.assertEqual(sum('Your session was interrupted' in arg for arg in second['argv']), 1)
        self.assertIn(str(self.task), second['argv'][-1])
        self.assertEqual([m for m in self.requests[index:] if m in self.effect_methods],
                         ['workspace.create', 'workspace.report_metadata', 'layout.apply'])
        self.assertEqual((self.ledger.read_bytes(), self.report.read_bytes(), self.task.read_bytes()),
                         (ledger, report, task))
        self.assertEqual(view['messages'], original_view['messages'])
        self.assert_resume_without_effect('Herdr resume already recorded')

        (self.root / 'finish-resumed').touch()
        receipt = self.receipt('resumed-done')
        self.assert_claimed('done', receipt)
        old, new = self.status()['messages']
        self.assertEqual((old['id'], old['messageId'], old['attempt']),
                         (self.question['message'], self.question['messageId'], self.original['attempt']))
        self.assertEqual((new['id'], new['messageId'], new['attempt']),
                         (receipt['message'], receipt['messageId'], resumed['attempt']))
        self.assertEqual(self.report.read_bytes(), report)
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        print(json.dumps({'dispatch': self.dispatch_id, 'session': resumed['session'],
                          'oldAttempt': old['attempt'], 'newAttempt': new['attempt'],
                          'originalMessageId': old['messageId'], 'resumedMessageId': new['messageId'],
                          'resumePrompts': 1, 'repeatCreated': False}), flush=True)

    def test_public_resume_without_confirmed_stop_does_not_launch(self):
        self.launch_original()
        self.assertTrue(self.status()['backendAlive'])
        self.assert_resume_without_effect('original Herdr worker has no confirmed exit receipt')
        self.stop_first()
        stop = self.wait_tree_stop()
        parked = stop.with_suffix('.parked')
        stop.rename(parked)
        try:
            missing = json.loads(self.assert_resume_without_effect(
                'original Herdr worker tree has no matching stop receipt'))
            self.assertEqual((missing['error'], missing['attempt']),
                             ('lifecycle_reconcile_required', self.original['attempt']))
        finally:
            parked.rename(stop)
        Path(self.original['create']['receipt']).unlink()
        self.assert_resume_without_effect('lifecycle_reconcile_required')

    def test_public_resume_without_saved_chat_does_not_launch(self):
        self.launch_original()
        self.stop_first()
        rollout, = Path(self.env['CODEX_HOME']).glob('sessions/*/*/*/rollout-*.jsonl')
        rollout.unlink()
        self.assert_resume_without_effect('cannot verify the original Codex conversation')

    def test_public_resume_keeps_newly_observed_original_writer(self):
        self.launch_original(keep_pane=True)
        (self.root / 'stop-first').touch()
        self.wait_exit(Path(self.original['create']['receipt']), 17)
        self.wait_tree_stop()
        writer = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])

        def stop_writer():
            if writer.poll() is None:
                writer.terminate()
            writer.wait(timeout=5)

        self.addCleanup(stop_writer)
        before = self.snapshot()
        index = len(self.requests)
        self.observed_writer = writer.pid
        try:
            refused = json.loads(self.cli('resume', self.dispatch_id,
                                          error='original Herdr pane or writers are still present'))
        finally:
            self.observed_writer = None
        self.assertEqual((refused['error'], refused['attempt']),
                         ('lifecycle_reconcile_required', self.original['attempt']))
        self.assertFalse(self.effect_methods.intersection(self.requests[index:]))
        self.assertIn('pane.process_info', self.requests[index:])
        after = self.snapshot()
        self.assertEqual(before[1:], after[1:])
        old, saved = [json.loads(snapshot[0][1]) for snapshot in (before, after)]
        old_writers = old['backendHandle'].pop('writerProcesses')
        saved_writers = saved['backendHandle'].pop('writerProcesses')
        self.assertEqual(saved, old)
        self.assertEqual(saved_writers[:-1], old_writers)
        self.assertNotIn(writer.pid, [process['pid'] for process in old_writers])
        added = saved_writers[-1]
        self.assertEqual(added['pid'], writer.pid)
        self.assertTrue(added['creation'].startswith('linux:'))

        (self.root / 'release-original-pane').touch()
        herdr_public.live.wait_for(lambda: self.h['pane'] not in {
            pane['pane_id'] for pane in self.service.api('session.snapshot')['snapshot']['panes']
        })
        herdr_public.live.wait_for(lambda: not any(map(self.writer_alive, old_writers)))
        self.assertIsNone(writer.poll())
        self.assertTrue(self.writer_alive(added))
        self.assert_resume_without_effect('original Herdr pane or writers are still present')
        self.assertEqual(self.requests[index:].count('workspace.create'), 0)
        stop_writer()
        resumed = self.cli('resume', self.dispatch_id)
        self.assertEqual(resumed['result'], 'accepted')
        self.assertNotEqual(resumed['attempt'], self.original['attempt'])
        self.assertEqual(self.requests[index:].count('workspace.create'), 1)
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        print(json.dumps({'oldAttempt': self.original['attempt'], 'newAttempt': resumed['attempt'],
                          'savedWriter': added, 'launchesBeforeWriterStopped': 0}), flush=True)

    def test_public_resume_reports_unconfirmed_zero_exit_once(self):
        self.launch_original()
        self.stop_first()
        (self.root / 'resume-exit-zero').touch()
        self.hold_zero_layout = True
        index = len(self.requests)
        answer = json.loads(self.cli('resume', self.dispatch_id, error='lifecycle_reconcile_required'))
        self.assertFalse(self.hold_zero_layout)
        self.assertEqual((answer['result'], answer['round']), ('uncertain', 1))
        self.assertNotEqual(answer['attempt'], self.original['attempt'])
        view = self.status()
        audit = view['audit'][-1]
        self.assertEqual((audit['operation'], audit['result'], audit['attempt']),
                         ('resume', 'uncertain', answer['attempt']))
        self.assertEqual(view['state'], 'unknown')
        message, = self.cli('wait', '--dispatch', self.dispatch_id, '--scope', self.row['scope'],
                            '--timeout', '4')['messages']
        self.assertEqual((message['dispatch'], message['attempt'], message['round'], message['kind'], message['type']),
                         (self.dispatch_id, answer['attempt'], 1, 'startup_failure', 'exited'))
        fault = message['fault']
        zero = self.zero_receipt['startupZero']
        self.assertEqual((fault['category'], fault['exitCode'], fault['launchId']),
                         ('process_exited', 0, Path(answer['receipt']).stem))
        self.assertEqual((fault['pid'], fault['processStart']), (zero['pid'], zero['processStart']))
        self.assertTrue(fault['processStart'].startswith('linux:'))
        ack = self.cli('ack', message['messageId'])
        self.assertEqual((ack['message'], ack['messageId'], ack['state'], ack['duplicate']),
                         (message['id'], message['messageId'], 'processed', False))
        self.assertTrue(self.cli('ack', message['id'])['duplicate'])
        self.assert_resume_without_effect('previous lifecycle operation is unresolved')
        self.cli('wait', '--dispatch', self.dispatch_id, '--scope', self.row['scope'], '--timeout', '0',
                 error='"messages":[]')
        old, notice = self.status()['messages']
        self.assertEqual(old['messageId'], self.question['messageId'])
        self.assertEqual((notice['id'], notice['messageId'], notice['state']),
                         (message['id'], message['messageId'], 'processed'))
        self.assertEqual([m for m in self.requests[index:] if m in self.effect_methods],
                         ['workspace.create', 'workspace.report_metadata', 'layout.apply'])
        launches = [json.loads(line) for line in self.launch_log.read_text().splitlines()]
        self.assertEqual(len(launches), 2)
        self.assertEqual(launches[-1]['pid'], fault['pid'])
        self.assertEqual(launches[-1]['argv'][-2], answer['session'])
        print(json.dumps({'attempt': answer['attempt'], 'audit': answer['audit'],
                          'notice': message['messageId'], 'fault': fault, 'ack': ack}), flush=True)


if __name__ == '__main__':
    unittest.main()
