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
        self.endpoint = self.enterContext(self.service.fault_proxy(self.record_request))
        self.dispatch(endpoint=self.endpoint, command=[
            sys.executable, '-u', str(Path(__file__).with_name('resume_worker.py').resolve()),
        ])
        self.original = self.row
        self.path = self.state / 'dispatches' / (self.dispatch_id + '.json')
        self.ledger = self.state / 'messages.json'
        self.launch_log = self.root / 'launches.jsonl'
        self.question = self.receipt('first-question')
        self.assert_claimed('question', self.question)

    def record_request(self, request, response):
        self.requests.append(request['method'])
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
        # Keep an actual archive event from the original attempt in the row.
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def snapshot(self):
        launches = sorted(self.state.joinpath('launches').glob('*'))
        return [(path.name, path.read_bytes()) for path in
                [self.path, self.ledger, self.task, self.report, self.launch_log, *launches]]

    def assert_resume_without_effect(self, error):
        before = self.snapshot()
        index = len(self.requests)
        self.cli('resume', self.dispatch_id, error=error)
        self.assertFalse(self.effect_methods.intersection(self.requests[index:]), self.requests[index:])
        self.assertEqual(self.snapshot(), before)

    def test_public_resume_reuses_saved_chat_and_task_once(self):
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
        self.assertTrue(self.status()['backendAlive'])
        self.assert_resume_without_effect('original Herdr worker has no confirmed exit receipt')
        self.stop_first()
        Path(self.original['create']['receipt']).unlink()
        self.assert_resume_without_effect('lifecycle_reconcile_required')

    def test_public_resume_without_saved_chat_does_not_launch(self):
        self.stop_first()
        rollout, = Path(self.env['CODEX_HOME']).glob('sessions/*/*/*/rollout-*.jsonl')
        rollout.unlink()
        self.assert_resume_without_effect('cannot verify the original Codex conversation')


if __name__ == '__main__':
    unittest.main()
