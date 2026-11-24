"""Public CLI tests against the unmodified fixed product, using actual Herdr."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import unittest

import herdr_public


class CurrentHerdr(herdr_public.PublicLifecycle):
    def setUp(self):
        super().setUp()
        self.project = self.root / 'ordinary project'
        self.project.mkdir()
        subprocess.run(['git', 'init', '-q', self.project], check=True, env=self.env)
        (self.project / 'README.md').write_text('isolated fixture project\n')
        self.root = self.project
        self.report = self.project / 'report.md'
        self.state = Path(self.env['WFC_STATE'])
        self.requests = []
        self.endpoint = self.enterContext(self.service.fault_proxy(self.record_request))
        self.env['WFC_HERDR_ENDPOINT'] = str(self.endpoint)

    def record_request(self, request, reply):
        self.requests.append(request['method'])
        return reply

    def launch(self, questions=0):
        self.dispatch(endpoint=self.endpoint, command=[
            sys.executable, '-u', str(Path(__file__).with_name('conversation_worker.py').resolve()),
            str(questions),
        ])
        self.launches = self.project / 'worker-launches'
        self.pid = int(herdr_public.live.wait_for(lambda: self.launches.exists() and self.launches.read_text()))
        self.wait_for_composer()

    def wait_for_composer(self):
        herdr_public.live.wait_for(lambda: '› Ask Codex to do anything' in self.reply_screen())

    def receipt(self, name):
        path = self.project / (name + '.json')
        return json.loads(herdr_public.live.wait_for(lambda: path.exists() and path.read_bytes()))

    def wait_receipt(self, kind, receipt):
        message, = self.cli('wait', '--scope', self.row['scope'], '--dispatch', self.dispatch_id,
                            '--timeout', 8)['messages']
        self.assertEqual((message['type'], message['id'], message['messageId']),
                         (kind, receipt['message'], receipt['messageId']))
        self.assertEqual((message['dispatch'], message['round'], message['attempt']),
                         (self.dispatch_id, self.row['round'], self.row['attempt']))
        confirmed = self.cli('ack', receipt['messageId'])
        self.assertEqual((confirmed['message'], confirmed['messageId'], confirmed['state']),
                         (receipt['message'], receipt['messageId'], 'processed'))

    def change_owner(self, action, owner):
        return self.cli('owner', action, '--scope', self.row['scope'], '--owner', owner, '--lease', 180)

    def complete(self):
        (self.project / 'finish-report').touch()
        completed = self.receipt('work-complete')
        self.wait_receipt('done', completed)
        self.assertEqual(self.report.read_text(), 'ordinary project fixture complete\n')
        return completed

    def business_state(self):
        paths = [self.state / 'dispatches' / f'{self.dispatch_id}.json', self.state / 'messages.json',
                 self.state / 'controller-owners.json', self.report, self.launches, self.project / 'worker-input']
        return {path.name: hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None
                for path in paths}

    def assert_unchanged_by(self, command, error):
        before = self.business_state()
        before_requests = len(self.requests)
        self.cli(*command, error=error)
        self.assertEqual(self.requests[before_requests:], [])
        self.assertEqual(self.business_state(), before)

    def assert_same_worker(self):
        view = self.status()
        self.assertTrue(view['backendAlive'])
        for key in ('dispatch', 'round', 'attempt'):
            self.assertEqual(view[key], self.row[key])
        for key in ('endpoint', 'service', 'workspace', 'pane', 'terminal', 'nonce'):
            self.assertEqual(view['backendHandle'][key], self.h[key])
        self.assertEqual(view['backendHandle']['writerProcesses'][0], self.h['writerProcesses'][0])
        info = self.service.api('pane.process_info', pane_id=self.h['pane'])['process_info']
        self.assertIn(self.pid, [info['shell_pid'], *(p['pid'] for p in info.get('foreground_processes', []))])
        self.assertEqual(self.launches.read_text().splitlines(), [str(self.pid)])

    def test_two_questions_and_scope_takeover_keep_one_worker_and_completion(self):
        self.launch(questions=2)
        old = self.change_owner('acquire', 'controller-a')
        self.env['WFC_CONTROLLER_TOKEN'] = old['token']
        first = self.receipt('question-1')
        self.wait_receipt('question', first)
        self.cli('reply', first['message'], 'first answer')
        second = self.receipt('question-2')
        self.wait_for_composer()
        current = self.change_owner('takeover', 'controller-b')
        self.assertEqual(current['generation'], old['generation'] + 1)
        self.assert_unchanged_by(('reply', second['message'], 'stale answer'), 'stale_controller_owner')
        self.env['WFC_CONTROLLER_TOKEN'] = current['token']
        self.wait_receipt('question', second)
        self.cli('reply', second['message'], 'second answer')
        self.assert_unchanged_by(('reply', second['message'], 'duplicate answer'), 'already answered')
        completed = self.complete()
        self.assert_same_worker()
        view = self.status()
        self.assertEqual(view['controllerOwner']['generation'], current['generation'])
        self.assertEqual(len(view['rounds']), 1)
        self.assertEqual(view['rounds'][0]['result']['attempt'], self.row['attempt'])
        self.assertFalse(view['pendingInput'])
        self.assertEqual([m['type'] for m in view['messages']], ['question', 'reply', 'question', 'reply', 'done'])
        self.assertEqual([(m['id'], m['messageId']) for m in view['messages'][::2]],
                         [(r['message'], r['messageId']) for r in (first, second, completed)])
        self.assertEqual([(m['to'], m['questionMessageId'], m['inputResult']) for m in view['messages'][1::2]],
                         [(r['message'], r['messageId'], 'accepted') for r in (first, second)])
        self.assertEqual((self.project / 'worker-input').read_text().splitlines(), [
            f"[router] Answer to question {first['message']}: first answer",
            f"[router] Answer to question {second['message']}: second answer",
        ])
        self.assertEqual(self.requests.count('pane.send_input'), 2)
        self.assertEqual(self.requests.count('pane.send_keys'), 0)
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')

    def test_completed_herdr_assignment_has_no_next_round_or_cross_host_handoff(self):
        self.launch()
        self.complete()
        self.assert_same_worker()
        task, report = self.project / 'next-task.md', self.project / 'next-report.md'
        task.write_text('next ordinary project task\n')
        options = ('--task', task, '--report', report)
        self.assert_unchanged_by(('continue', self.dispatch_id, *options), "'continue' is unavailable")
        self.assert_unchanged_by(('handoff', self.dispatch_id, '--host', 'claude', *options), 'unexpected arguments')
        self.assert_same_worker()
        self.assertEqual(self.cli('close', self.dispatch_id)['result'], 'completed')
        self.assertFalse(self.status()['backendAlive'])
        self.assert_unchanged_by(('resume', self.dispatch_id, *options), 'dispatch using the tmux backend')
        self.assertFalse(report.exists())
        self.assertFalse(self.status()['backendAlive'])

    def test_offline_herdr_resume_preserves_unfinished_assignment(self):
        self.launch(questions=2)
        question = self.receipt('question-1')
        self.wait_receipt('question', question)
        self.assert_same_worker()
        (self.project / 'exit-worker').touch()

        def stopped():
            view = self.status()
            return view['backendAlive'] is False and (view['launch'] or {}).get('category') == 'process_exited'

        herdr_public.live.wait_for(stopped, seconds=12)
        self.assert_unchanged_by(('resume', self.dispatch_id), 'dispatch using the tmux backend')
        view = self.status()
        self.assertFalse(view['backendAlive'])
        self.assertFalse(self.report.exists())
        message, = view['messages']
        self.assertEqual((message['type'], message['id'], message['messageId']),
                         ('question', question['message'], question['messageId']))


if __name__ == '__main__':
    unittest.main()
