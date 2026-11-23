"""Linux integration: python3 tests/herdr_public.py HERDR WFC [-v]."""
import fcntl
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import threading
import time
import unittest
import uuid

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

    def test_runtime_read_evidence_and_independent_recovery(self):
        hold = threading.Event()
        hide_pane = threading.Event()
        hold_snapshot = threading.Event()
        requests = []
        hidden = []
        stalled_snapshots = []
        methods = []
        withheld = []
        peer_closed = []
        trace = []
        started = time.monotonic()
        self.addCleanup(lambda: print(json.dumps({"case": "herdr-recovery-request-trace",
                                                   "events": trace[-64:]}), flush=True))
        secret = "private-herdr-transport-token"
        screen = self.root / "screen"
        screen.write_text("Ready.")
        self.worker.write_text(r"""
import time
from pathlib import Path
last = None
while True:
    text = Path("screen").read_text()
    if text != last:
        print("\x1b[2J\x1b[H" + text, flush=True)
        last = text
    time.sleep(.025)
""")
        self.env["WFC_MAX_TURN_SECONDS"] = "0"

        def record(req, phase):
            trace.append({"method": req["method"], "requestId": req["id"],
                          "phase": phase, "ms": int((time.monotonic() - started) * 1000)})

        def read_request(stream):
            frame = stream.readline()
            if not frame:
                return None
            req = json.loads(frame)
            methods.append(req["method"])
            poller = select.poll()
            poller.register(stream, select.POLLHUP)
            if any(flags & select.POLLHUP for _, flags in poller.poll(0)):
                self.assertEqual(stream.read(1), b"", "a closed client carried another frame")
                record(req, "closed_before_forward")
                return None
            if req["method"] == "session.snapshot" and hold_snapshot.is_set():
                record(req, "snapshot_withheld_before_read")
                stalled_snapshots.append(req["id"])
                poller.modify(stream, select.POLLIN | select.POLLHUP)
                self.assertTrue(poller.poll(3000), "the snapshot probe did not close its connection")
                self.assertEqual(stream.read(1), b"")
                return None
            if req["method"] != "pane.read":
                return frame
            pane = req["params"]["pane_id"]
            key = (req["id"], pane)
            requests.append(key)
            if not hold.is_set():
                return frame
            # Forward the actual request once, then withhold only this reply.
            reply = json.loads(live.exchange(self.service.endpoint, req))
            self.assertEqual(reply["id"], req["id"])
            self.assertEqual(reply["result"]["type"], "pane_read")
            self.assertEqual(reply["result"]["read"]["pane_id"], pane)
            withheld.append(key)
            record(req, "withheld")

            # Each probe owns its deadline. Releasing only when the outer
            # wait exits would queue a later probe behind the withheld reply.
            poller.modify(stream, select.POLLIN | select.POLLHUP)
            self.assertTrue(poller.poll(3000), "the timed probe did not close its connection")
            self.assertEqual(stream.read(1), b"", "a withheld connection carried another frame")
            peer_closed.append(key)
            record(req, "peer_closed")
            return None

        def observe_reply(req, reply):
            if req["method"] == "session.snapshot" and hide_pane.is_set():
                snapshot = reply["result"]["snapshot"]
                panes = snapshot["panes"]
                snapshot["panes"] = [p for p in panes if p["pane_id"] != self.h["pane"]]
                self.assertEqual(len(panes) - len(snapshot["panes"]), 1)
                hidden.append(req["id"])
                record(req, "bound_pane_absent")
            if req["method"] == "pane.read":
                record(req, "reply")
            return reply

        def run_wait(timeout, quiet=False):
            args = ("wait", "--scope", self.row["scope"], "--dispatch", self.dispatch_id,
                    "--stall-after", "0", "--timeout", timeout)
            result = self.cli(*args, error='"timedOut":true' if quiet else None)
            return json.loads(result)["messages"] if quiet else result["messages"]

        def slow_wait(timeout, quiet=False, may_retry=False):
            start = len(withheld)
            hold.set()
            try:
                messages = run_wait(timeout, quiet)
            finally:
                hold.clear()
            # The proxy serves one connection at a time. A fresh private ping
            # proves it drained earlier connections before checking their EOFs.
            ping = {"id": uuid.uuid4().hex, "method": "ping", "params": {}}
            pong = json.loads(live.exchange(endpoint, ping))
            self.assertEqual(pong["id"], ping["id"])
            self.assertEqual(pong["result"]["type"], "pong")
            record(ping, "proxy_drained")
            reads = withheld[start:]
            self.assertTrue(reads, "the bound pane.read reached Herdr")
            if not may_retry:
                self.assertEqual(len(reads), 1)
            self.assertTrue(all(pane == self.h["pane"] for _, pane in reads))
            self.assertEqual(peer_closed[start:], reads)
            return messages

        def show(text):
            tmp = screen.with_suffix(".tmp")
            tmp.write_text(text)
            tmp.replace(screen)
            live.wait_for(lambda: text in self.service.api("pane.read", pane_id=self.h["pane"],
                source="recent", format="text", strip_ansi=True, lines=10)["read"]["text"])

        def ack(notice):
            for key in ("scope", "dispatch", "round", "attempt"):
                self.assertEqual(notice[key], self.row[key])
            first = self.cli("ack", notice["messageId"])
            self.assertEqual((first["state"], first["statusVersion"]), ("processed", 3))
            self.assertTrue(self.cli("ack", notice["messageId"])["duplicate"])

        def verify(notices, expected):
            self.assertEqual([m["fault"]["evidence"] for m in notices], expected)
            self.assertNotIn(secret, json.dumps(notices))
            for message in notices:
                ack(message)

        with self.service.fault_proxy(observe_reply, read_frame=read_request) as endpoint:
            self.dispatch(endpoint=endpoint)
            hide_pane.set()
            self.assertFalse(run_wait("1.4", quiet=True))
            self.assertTrue(hidden)
            self.assertFalse(requests, "a missing pane has no valid pane.read request")
            hide_pane.clear()
            self.assertFalse(run_wait("1.4", quiet=True))
            self.assertTrue(requests, "the original bound pane was read successfully")
            state = Path(self.env["WFC_STATE"])
            watch = state / "dispatches" / f"{self.dispatch_id}.host-watch"
            execution = json.loads(watch.read_text())["execution"]
            self.assertTrue(execution)

            def absence():
                previous_requests, previous_hidden = len(requests), len(hidden)
                hide_pane.set()
                notices = run_wait("3")
                verify(notices, ["backend_unavailable"])
                self.assertEqual(self.status()["hostActivity"]["readState"], "unavailable")
                self.assertFalse(run_wait("1.4", quiet=True))
                self.assertEqual(len(requests), previous_requests)
                self.assertGreater(len(hidden), previous_hidden)
                hide_pane.clear()
                restored = run_wait("3")
                verify(restored, ["read_restored"])
                self.assertEqual(self.status()["hostActivity"]["readState"], "readable")
                self.assertEqual(json.loads(watch.read_text())["execution"], execution)
                return notices[0]["messageId"], restored[0]["messageId"]

            absent, restored = absence()
            absent_again, restored_again = absence()
            self.assertEqual(len({absent, restored, absent_again, restored_again}), 4)

            # A reply from a different peer at the exact path cannot prove absence.
            self.assertEqual(self.h["service"]["pid"], os.getpid())
            self.assertNotEqual(self.h["service"]["pid"], self.service.server.pid)
            proxy_backup = endpoint.with_suffix(".held")
            endpoint.rename(proxy_backup)
            try:
                self.service.endpoint.rename(endpoint)
                try:
                    self.assertFalse(run_wait("1.4", quiet=True))
                finally:
                    endpoint.rename(self.service.endpoint)
            finally:
                proxy_backup.rename(endpoint)

            previous_requests = len(requests)
            hold_snapshot.set()
            try:
                self.assertFalse(run_wait("1.4", quiet=True))
            finally:
                hold_snapshot.clear()
            self.assertTrue(stalled_snapshots)
            self.assertEqual(len(requests), previous_requests)
            lockfile = state / "dispatches" / f"{self.dispatch_id}.herdr-lock"
            start = len(requests)
            rpc_count = len(methods)
            with lockfile.open("rb") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                self.assertFalse(run_wait("1.4", quiet=True))
                self.assertEqual(len(methods), rpc_count)
                self.assertEqual(len(requests), start)
            self.assertFalse(run_wait(".55", quiet=True))
            self.assertGreater(len(requests), start)
            self.assertFalse(slow_wait(".65", quiet=True))
            first = slow_wait("3")
            verify(first, ["backend_unavailable"])
            self.assertFalse(slow_wait("1.4", quiet=True, may_retry=True))

            show(f"Quota exceeded: {secret}")
            before = len(trace)
            quota_restored = run_wait("3")
            self.assertTrue(any(event["phase"] == "reply" for event in trace[before:]),
                            "the proxy accepted a fresh pane.read after the clipped retries")
            verify(quota_restored, ["screen_notice", "read_restored"])
            quota = self.status()["hostActivity"]
            self.assertEqual((quota["state"], quota["fault"]["category"], quota["readState"]),
                             ("blocked", "quota", "readable"))
            again = slow_wait("3")
            verify(again, ["backend_unavailable"])
            self.assertNotEqual(first[0]["messageId"], again[0]["messageId"])
            self.assertEqual(len({req_id for req_id, _ in requests}), len(requests))
            self.assertEqual(self.status()["hostActivity"]["readState"], "unavailable")

            ledger = state / "messages.json"
            messages = json.loads(ledger.read_text())
            question = {key: self.row[key] for key in ("dispatch", "scope", "round", "attempt", "role")}
            messages.append(dict(question, type="question", id=len(messages) + 1, messageId="m-test-question",
                                 state="processed", statusVersion=3,
                                 question_payload={"text": "private pending answer", "options": []}))
            pending = ledger.with_suffix(".tmp")
            pending.write_text(json.dumps(messages))
            pending.replace(ledger)
            show(f"Approval required: {secret}")
            permission_restored = run_wait("3")
            verify(permission_restored, ["screen_notice", "read_restored"])
            activity = self.status()["hostActivity"]
            self.assertEqual((activity["state"], activity["fault"]["category"], activity["readState"],
                              activity["awaitingAnswer"]),
                             ("awaiting_permission", "permission_required", "readable", True))
            self.assertFalse(run_wait(".55", quiet=True))
            self.assertNotIn(secret, ledger.read_text())
            self.assertEqual(self.cli("close", self.dispatch_id)["result"], "completed")
            hide_pane.set()
            self.assertFalse(run_wait("1.4", quiet=True))
            print(json.dumps({"case": "herdr-transport-classification", "dispatch": self.dispatch_id,
                              "snapshotAbsence": [absent, absent_again],
                              "snapshotRestoration": [restored, restored_again],
                              "sideServiceSilent": True, "unprovedReadSilent": True,
                              "closedSilent": True,
                              "attempt": self.row["attempt"], "pane": self.h["pane"],
                              "busyIssuedNoRequest": True, "clippedReadSilent": True,
                              "slowReadReceipts": [first[0]["messageId"], again[0]["messageId"]],
                              "quotaRestored": quota_restored[-1]["messageId"],
                              "permissionRestored": permission_restored[-1]["messageId"],
                              "pendingAnswerPreserved": True, "close": "completed"}), flush=True)

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

    def assert_reply_worker_finished(self, question, outcome, earlier=(), *, question_received=False):
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
        if not question_received:
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
        hide_pane = False
        sent_input = threading.Event()
        release_input = threading.Event()

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
                    sent_input.set()
                    self.assertTrue(release_input.wait(10), "reply fixture did not release the receipt")
                    return None
            if hide_pane and method == 'session.snapshot':
                snapshot = reply["result"]["snapshot"]
                panes = snapshot["panes"]
                snapshot["panes"] = [p for p in panes if p["pane_id"] != self.h["pane"]]
                self.assertEqual(len(panes) - len(snapshot["panes"]), 1)
            return reply

        with self.service.fault_proxy(observe, read_frame=read_frame) as proxy:
            question = self.setup_reply_worker(proxy)
            number = str(question['message'])
            self.env["WFC_MAX_TURN_SECONDS"] = "0"
            wait = ("wait", "--scope", self.row["scope"], "--dispatch", self.dispatch_id,
                    "--stall-after", "0", "--timeout")
            received, = self.cli(*wait, "0")["messages"]
            self.assertEqual((received["type"], received["messageId"]), ("question", question["messageId"]))

            def quiet():
                result = self.cli(*wait, "1.4", error='"timedOut":true')
                self.assertEqual(json.loads(result)["messages"], [])

            quiet()
            watch = Path(self.env["WFC_STATE"]) / "dispatches" / f"{self.dispatch_id}.host-watch"
            baseline = watch.read_bytes()
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
            with subprocess.Popen([live.DRIVER, "reply", number, "once"], env=self.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as writer:
                try:
                    self.assertTrue(sent_input.wait(7), "the original peer did not receive the reply")
                    quiet()
                    self.assertEqual(watch.read_bytes(), baseline)
                finally:
                    release_input.set()
                output, error = writer.communicate(timeout=12)
            self.assertEqual((writer.returncode, output), (1, ""), error)
            self.assertIn("uncertain", error)
            self.assertTrue(self.status()['pendingInput'])
            replies = self.reply_attempts()
            self.assertEqual([item["inputResult"] for item in replies], ["definitely-not-sent", "uncertain"])
            for missing, evidence, read_state in (
                    (True, "backend_unavailable", "unavailable"),
                    (False, "read_restored", "readable")):
                hide_pane = missing
                notice, = self.cli(*wait, "3")["messages"]
                self.assertEqual((notice["kind"], notice["fault"]["evidence"]), ("host_runtime", evidence))
                for key in ("dispatch", "scope", "round", "attempt"):
                    self.assertEqual(notice[key], self.row[key])
                self.assertEqual(self.cli("ack", notice["messageId"])["state"], "processed")
                status = self.status()
                self.assertTrue(status['pendingInput'])
                self.assertEqual((status["hostActivity"]["readState"], status["hostActivity"]["state"]),
                                 (read_state, "awaiting_answer"))
                original, = [item for item in status["messages"] if item["type"] == "question"]
                self.assertEqual((original["messageId"], original["state"]), (question["messageId"], "received"))
                self.assertEqual([item for item in status["messages"] if item["type"] == "reply"], replies)
            self.assert_reply_worker_finished(question, 'uncertain', earlier=('definitely-not-sent',),
                                              question_received=True)
            self.assertEqual(sends, ['pane.send_input'])
            print(json.dumps({"case": "herdr-reply-diagnostics", "question": question["messageId"],
                              "reply": replies[-1]["messageId"], "afterZeroWrite": "definitely-not-sent",
                              "afterLostReceipt": "uncertain", "sends": sends}), flush=True)

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
