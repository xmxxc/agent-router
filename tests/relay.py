"""Linux relay contracts through a single CLI, with a private Codex queue."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
import hashlib
import json
import os
import select
import shutil
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import time
from threading import Event
import unittest
import uuid

import runtime_process

WFC = str(Path(sys.argv.pop(1)).resolve())
PREFIX = "[router] Messages for the controller: "


class Relay(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.thread = str(uuid.uuid4())
        self.env = {"PATH": str(self.root), "HOME": tmp.name,
                    "WFC_STATE": str(self.root / "state"), "CODEX_THREAD_ID": self.thread}
        day = self.root / ".codex/sessions/2026/01/01"
        day.mkdir(parents=True)
        self.rollout = day / f"rollout-time-{self.thread}.jsonl"
        self.meta(self.thread)
        (self.root / "task").write_text("private relay test")
        self.queue = self.root / "codex"
        self.queue.write_text(f"#!{sys.executable}\n" + """
import json, os, sys, time
from pathlib import Path
with open('calls', 'a') as f:
    f.write(json.dumps(sys.argv[1:]) + '\\n')
assert not any(k.upper() in ('WFC_CONTROLLER_TOKEN','WFC_WORKER_CALLBACK','WFC_OWNER_TOKEN') for k in os.environ)
while Path('gate').exists():
    time.sleep(.01)
mode = os.environ.get('OUTCOME')
if mode == 'missing':
    print(' ' * 70000, flush=True)
    print('codex queue: Error: no rollout found for thread id ' + sys.argv[3], file=sys.stderr)
sys.exit(1 if mode else 0)
""")
        self.queue.chmod(0o700)

    def meta(self, ident, source="cli"):
        self.rollout.write_text(json.dumps({"type": "session_meta", "payload": {"id": ident, "source": source}}) + "\n")

    def cli(self, *args, code=0, **env):
        p = subprocess.run([WFC, *args], cwd=self.root, env=dict(self.env, **env),
                           capture_output=True, text=True, timeout=10)
        self.assertEqual(p.returncode, code, (args, p.stderr, p.stdout))
        return p.stdout

    def data(self, *args, **env):
        return json.loads(self.cli(*args, **env))

    def publish(self, scope):
        d = self.data("dispatch", "--task", "task", "--role", "复核", "--scope", scope,
                      "--report", "task", "--cwd", ".", "--", "/bin/true")
        return self.data("done", "--report", "task", "--attempt", d["attempt"], WFC_DISPATCH=d["dispatch"])

    def calls(self):
        path = self.root / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def until(self, check):
        end = time.monotonic() + 5
        while time.monotonic() < end:
            result = check()
            if result:
                return result
            time.sleep(.025)
        self.fail("timed out waiting for " + repr(check))

    def test_question_envelope_matches_python_json_separators(self):
        worker = """import os, subprocess, sys
subprocess.run([sys.argv[1], 'ask', '选甲,乙？', '--options', '甲,乙',
                '--attempt', os.environ['WFC_ATTEMPT']], check=True)
"""
        record = self.data("dispatch", "--task", "task", "--role", "复核", "--scope", "ours",
                           "--cwd", ".", "--", sys.executable, "-c", worker, WFC)
        self.until(lambda: self.data("status")["undelivered"])
        ledger = Path(self.env["WFC_STATE"]) / "messages.json"
        question, = json.loads(ledger.read_bytes())
        worker_fields = {key: question[key] for key in ("dispatch", "round", "attempt", "role", "scope")}
        worker_history = [
            dict(worker_fields, id=question["id"] + 1, messageId=f"m-{uuid.uuid4().hex}",
                 state="unsent", statusVersion=3, reply_payload={
                     "to": question["id"], "questionMessageId": question["messageId"],
                     "answer": "unsent reply", "inputResult": "definitely-not-sent",
                     "diagnostic": "fixture", "category": None,
                 }),
            dict(worker_fields, id=question["id"] + 2, messageId=f"m-{uuid.uuid4().hex}",
                 state="unsent", statusVersion=1, type="reclaimed", report=str(self.root / "task")),
        ]
        ledger.write_text(json.dumps([question, *worker_history]))
        self.cli("relay", "--scope", "ours", "--codex", "--for", "1")
        command, = self.calls()
        envelope = command[4]
        self.assertTrue(envelope.startswith(PREFIX))
        raw = envelope[len(PREFIX):]
        batch = json.loads(raw)
        self.assertEqual(raw, json.dumps(batch, ensure_ascii=False))
        message, = batch["messages"]
        self.assertEqual((message["type"], message["question"], message["options"]),
                         ("question", "选甲,乙？", ["甲", "乙"]))
        self.assertEqual((message["id"], message["messageId"]), (question["id"], question["messageId"]))
        self.assertEqual((message["dispatch"], message["attempt"]), (record["dispatch"], record["attempt"]))
        confirmed = self.data("ack", message["messageId"])
        self.assertEqual((confirmed["state"], confirmed["statusVersion"]), ("processed", 4))
        self.assertEqual(json.loads(ledger.read_bytes())[1:], [dict(row, request_key=None) for row in worker_history])

    def test_startup_failure_uses_the_existing_scope_and_ack(self):
        self.cli("dispatch", "--task", "task", "--role", "启动验证", "--scope", "failure",
                 "--report", "task", "--cwd", ".", "--", str(self.root / "missing"), code=1)
        unrelated = self.publish("other")
        self.cli("relay", "--codex", "--scope", "failure", "--for", "1.5")
        command, = self.calls()
        notice, = json.loads(command[4][len(PREFIX):])["messages"]
        self.assertEqual((notice["scope"], notice["kind"], notice["type"]),
                         ("failure", "startup_failure", "blocked"))
        self.assertEqual(notice["fault"]["category"], "executable_missing")
        ack = self.data("ack", notice["messageId"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))
        self.assertTrue(self.data("ack", notice["messageId"])["duplicate"])
        empty = self.data("wait", "--scope", "failure", "--timeout", "0", code=3)
        self.assertFalse(empty["messages"])
        other, = self.data("wait", "--scope", "other", "--timeout", "0")["messages"]
        self.assertEqual((other["messageId"], other["type"]), (unrelated["messageId"], "done"))

    def start_private_tmux(self):
        socket = self.root / "socket"
        tmux = [shutil.which("tmux"), "-S", str(socket), "-f", "/dev/null"]
        self.env.update(PATH=os.pathsep.join((str(self.root), os.environ["PATH"])),
                        WFC_TMUX_SOCKET=str(socket), TERM="xterm", WFC_MAX_TURN_SECONDS="0")
        subprocess.run([*tmux, "new-session", "-d", "-s", "sentinel", "/bin/cat"],
                       env=self.env, check=True, timeout=10)
        self.addCleanup(subprocess.run, [*tmux, "kill-server"], env=self.env,
                        capture_output=True, timeout=10)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_runtime_notice_through_the_private_queue(self):
        self.start_private_tmux()
        secret = "private-queue-fixture-token"
        config = self.root / "launch.json"
        fixture = f"import time; print('Quota exceeded: {secret}', flush=True); time.sleep(20)"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, "-c", fixture]}}}))
        job = self.data("dispatch", "--task", "task", "--role", "worker", "--scope", "failure",
                        "--report", "task", "--cwd", ".", "--host", "codex", "--launch-config", str(config))

        self.cli("relay", "--codex", "--scope", "other", "--for", ".25")
        self.assertEqual(self.calls(), [])
        self.cli("relay", "--codex", "--scope", "failure", "--for", "2")
        [call] = self.calls()
        self.assertTrue(call[4].startswith(PREFIX))
        self.assertNotIn(secret, call[4])
        [notice] = json.loads(call[4][len(PREFIX):])["messages"]
        self.assertEqual((notice["kind"], notice["type"], notice["fault"]["category"]),
                         ("host_runtime", "blocked", "quota"))
        for key in ("dispatch", "round", "attempt", "scope"):
            self.assertEqual(notice[key], job[key])
        row, = self.data("status", "--dispatch", job["dispatch"])["dispatches"]
        saved, = row["messages"]
        for key in ("id", "messageId", "kind", "type", "dispatch", "round", "attempt", "scope", "fault"):
            self.assertEqual(saved[key], notice[key])
        self.assertEqual((saved["to"], saved["delivered"], saved["state"], saved["statusVersion"]),
                         ("controller", True, "received", 3))
        ack = self.data("ack", notice["messageId"])
        self.assertEqual((ack["message"], ack["messageId"]), (notice["id"], notice["messageId"]))
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))
        self.cli("relay", "--codex", "--scope", "failure", "--for", "1.25")
        self.assertEqual(self.calls(), [call])
        print(json.dumps({"case": "runtime-private-queue", "scope": notice["scope"],
                          "dispatch": notice["dispatch"], "attempt": notice["attempt"],
                          "messageId": notice["messageId"], "fault": notice["fault"],
                          "queueCalls": 1, "ack": ack}), flush=True)

    def slow_tmux_probe(self):
        real_tmux = shutil.which("tmux")
        trace = self.root / "slow-probes"
        slow = self.root / "slow-probe-gate"
        shim = self.root / "tmux"
        shim.write_text(f"#!{sys.executable}\n" + """
import json, os, subprocess, sys
from pathlib import Path
argv = sys.argv[1:]
if "list-panes" not in argv or not Path(SLOW_GATE).exists():
    os.execv(REAL_TMUX, [REAL_TMUX, *argv])
child = subprocess.Popen(["/bin/sleep", "25"])
with Path(TRACE).open("a") as log:
    print(json.dumps(dict(shim=os.getpid(), descendant=child.pid, group=os.getpgrp(),
                          target=argv[argv.index("-t") + 1])), file=log)
sys.exit(child.wait())
""".replace("REAL_TMUX", repr(real_tmux)).replace("TRACE", repr(str(trace))).replace("SLOW_GATE", repr(str(slow))))
        shim.chmod(0o700)

        def probes():
            return [json.loads(line) for line in trace.read_text().splitlines()] if trace.exists() else []

        return slow, probes

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_unverified_tmux_timeout_and_abandoned_relay(self):
        self.start_private_tmux()
        config = self.root / "launch.json"
        fixture = "import time; print('Ready.', flush=True); time.sleep(60)"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, "-c", fixture]}}}))
        job = self.data("dispatch", "--task", "task", "--role", "worker", "--scope", "ours",
                        "--report", "task", "--cwd", ".", "--host", "codex", "--launch-config", str(config))
        wait = ("wait", "--scope", "ours", "--dispatch", job["dispatch"], "--stall-after", "0")
        self.assertFalse(self.data(*wait, "--timeout", ".55", code=3)["messages"])
        watch = self.root / "state" / "dispatches" / f'{job["dispatch"]}.host-watch'
        self.assertTrue(json.loads(watch.read_text())["execution"])
        slow, probes = self.slow_tmux_probe()
        slow.touch()
        self.assertFalse(self.data(*wait, "--timeout", ".55", code=3)["messages"])
        self.assertTrue(probes(), "the clipped invocation reached the slow client")

        before = len(probes())
        self.assertFalse(self.data(*wait, "--timeout", "1.55", code=3)["messages"])
        self.assertGreater(len(probes()), before, "the complete attempt reached the shim")
        other = self.publish("ours")
        [answer] = self.data("wait", "--scope", "ours", "--timeout", "0")["messages"]
        self.assertEqual(answer["messageId"], other["messageId"])

        runtime_process.subreaper(self)
        before = len(probes())
        relay = self.foreground(6)
        item = self.until(lambda: next(iter(probes()[before:]), None))
        owned = self.until(lambda: runtime_process.capture(relay))
        shim = runtime_process.process(item["shim"])
        descendant = runtime_process.process(item["descendant"])
        self.assertIsNotNone(shim)
        self.assertIsNotNone(descendant)
        self.assertEqual(item["group"], owned[1].pid)
        backend = runtime_process.process(shim.parent)
        self.assertIsNotNone(backend)
        self.assertEqual(backend.parent, owned[1].pid)
        self.assertEqual(descendant.parent, shim.pid)
        cleanup = runtime_process.abort_and_reap(self, relay, owned, (backend, shim, descendant))
        self.assertFalse(self.calls(), "cancellation did not send a new queue message")

        slow.unlink()
        self.assertFalse(self.data(*wait, "--timeout", ".65", code=3)["messages"])
        status, = self.data("status", "--dispatch", job["dispatch"])["dispatches"]
        self.assertFalse(status["messages"])
        self.assertEqual(self.data("close", job["dispatch"])["result"], "completed")
        print(json.dumps({"case": "abandoned-relay", "scope": job["scope"], "attempt": job["attempt"],
                          "unverifiedTimeoutSilent": True,
                          "otherReceipt": answer["messageId"], "slowClient": shim.pid,
                          "slowClientStart": shim.started, "descendant": descendant.pid,
                          "descendantStart": descendant.started, **cleanup}), flush=True)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_tmux_bound_absence_is_reported_and_recovers(self):
        self.start_private_tmux()
        cfg = self.root / "launch.json"
        code = "import time; print('Ready.', flush=True); time.sleep(60)"
        cfg.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, "-c", code]}}}))
        job = self.data("dispatch", "--task", "task", "--role", "worker", "--scope", "ours",
                        "--report", "task", "--cwd", ".", "--host", "codex", "--launch-config", str(cfg))
        dispatch = job["dispatch"]
        session = f"wfc-{dispatch}"
        tmux = [shutil.which("tmux"), "-S", self.env["WFC_TMUX_SOCKET"]]
        run_tmux = lambda *args: subprocess.check_output([*tmux, *args], text=True).strip()
        pane = run_tmux("display-message", "-p", "-t", session, "#{session_id} #{pane_id}")
        wait = ("wait", "--scope", "ours", "--dispatch", dispatch, "--stall-after", "0")

        def quiet(timeout="1.25"):
            self.assertFalse(self.data(*wait, "--timeout", timeout, code=3)["messages"])

        def notice(evidence, read_state):
            [message] = self.data(*wait, "--timeout", "3")["messages"]
            self.assertEqual((message["kind"], message["fault"]["evidence"]), ("host_runtime", evidence))
            for key in ("scope", "round", "attempt", "dispatch"):
                self.assertEqual(message[key], job[key])
            row, = self.data("status", "--dispatch", dispatch)["dispatches"]
            self.assertEqual(row["hostActivity"]["readState"], read_state)
            self.assertEqual(self.data("ack", message["messageId"])["state"], "processed")
            self.assertTrue(self.data("ack", message["messageId"])["duplicate"])
            return message["messageId"]

        quiet()
        watch = self.root / "state" / "dispatches" / f"{dispatch}.host-watch"
        execution = json.loads(watch.read_text())["execution"]
        run_tmux("rename-session", "-t", session, "renamed-original")
        quiet()  # Same session/pane still appears in the pinned server's roster.
        run_tmux("rename-session", "-t", "renamed-original", session)
        launch = run_tmux("show-environment", "-t", session, "WFC_TMUX_LAUNCH").split("=", 1)[1]
        run_tmux("set-environment", "-t", session, "WFC_TMUX_LAUNCH", "0" * 32)
        quiet()  # A returned foreign binding does not establish absence.
        run_tmux("set-environment", "-t", session, "WFC_TMUX_LAUNCH", launch)

        # Isolate a reversible negative roster on the real, pinned private
        # server. Everything other than these two reported results passes through.
        gate = self.root / "reported-absence"
        shim = self.root / "tmux"
        shim.write_text(f"#!{sys.executable}\n" + f"""
import os, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
real = {tmux[0]!r}
if Path({str(gate)!r}).exists() and "list-panes" in args:
    if {f'={session}:'!r} in args:
        print({"can't find session: " + session!r}, file=sys.stderr)
        sys.exit(1)
    if "-a" in args and "#{{session_id}} #{{pane_id}}" in args:
        result = subprocess.run([real, *args], capture_output=True, text=True, close_fds=False)
        if result.returncode:
            sys.stderr.write(result.stderr)
            sys.exit(result.returncode)
        for line in result.stdout.splitlines():
            if line != {pane!r}:
                print(line)
        sys.exit(0)
os.execv(real, [real, *args])
""")
        shim.chmod(0o700)

        gate.touch()
        wrong, = self.data("status", "--dispatch", dispatch)["dispatches"]
        self.assertFalse(wrong["backendAlive"])
        lock = self.root / "state" / "close-locks" / f'{dispatch}.{job["attempt"]}'
        with lock.open("rb") as close_lock:
            fcntl.flock(close_lock, fcntl.LOCK_EX)
            quiet()  # A full negative reply cannot publish while a close owns this execution.
        first = notice("backend_unavailable", "unavailable")
        quiet()
        gate.unlink()
        recovered = notice("read_restored", "readable")
        self.assertEqual(json.loads(watch.read_text())["execution"], execution)
        gate.touch()
        again = notice("backend_unavailable", "unavailable")
        self.assertEqual(len({first, recovered, again}), 3)
        quiet()
        gate.unlink()
        self.assertEqual(self.data("close", dispatch)["result"], "completed")
        gate.touch()
        quiet()
        print(json.dumps({"case": "tmux-bound-absence", "scope": job["scope"], "attempt": job["attempt"],
                          "returnedAbsence": [first, again], "readRestored": recovered,
                          "renameAndForeignBindingSilent": True, "confirmedCloseSilent": True}), flush=True)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_codex_unicode_appended_activity(self):
        self.start_private_tmux()
        commands = self.root / "screen.json"
        secret = "private-codex-fixture-token"
        lines = [f"Quota exceeded: {secret}"]
        commands.write_text(json.dumps(lines))
        fixture = textwrap.dedent("""
            import json
            import time
            from pathlib import Path
            count = 0
            while True:
                lines = json.loads(Path("screen.json").read_text())
                for line in lines[count:]:
                    print(line, flush=True)
                count = len(lines)
                time.sleep(.025)
            """)
        config = self.root / "launch.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, "-c", fixture]}}}))
        job = self.data("dispatch", "--task", "task", "--role", "worker", "--scope", "ours",
                        "--report", "task", "--cwd", ".", "--host", "codex", "--launch-config", str(config))
        wait = ("wait", "--scope", job["scope"], "--dispatch", job["dispatch"], "--stall-after", "0")

        def append(line):
            lines.append(line)
            tmp = commands.with_suffix(".tmp")
            tmp.write_text(json.dumps(lines))
            tmp.replace(commands)

        def observe(category, evidence):
            notice, = self.data(*wait, "--timeout", "3")["messages"]
            self.assertEqual((notice["fault"]["category"], notice["fault"]["evidence"]),
                             (category, evidence))
            self.assertEqual((notice["attempt"], notice["scope"]), (job["attempt"], job["scope"]))
            self.assertNotIn(secret, json.dumps(notice))
            self.assertEqual(self.data("ack", notice["messageId"])["state"], "processed")
            return notice["messageId"]

        def quiet():
            self.assertFalse(self.data(*wait, "--timeout", "1.3", code=3)["messages"])

        first = observe("quota", "screen_notice")
        append("Elapsed: 2s")
        quiet()
        append("• Time elapsed: 3 seconds")
        quiet()
        append("• 4s")
        quiet()
        append("• Working (4s • esc to interrupt)")
        quiet()
        append("Processing (5s)")
        quiet()
        append("普通尾注只是界面文本")
        quiet()
        for i in range(10):
            append(f"普通尾注 {i}" if i % 2 else f"Processing (1m{i + 6}s)")
        quiet()
        screen = subprocess.check_output(
            [shutil.which("tmux"), "-S", self.env["WFC_TMUX_SOCKET"], "capture-pane", "-p",
             "-t", f'=wfc-{job["dispatch"]}:'], text=True, timeout=5)
        self.assertIn(f"Quota exceeded: {secret}", screen)
        self.assertIn("普通尾注 9", screen)
        append("• 已拿到结果。")
        with_punctuation = observe("recovered", "changed_activity")
        quiet()
        append("Quota exceeded")
        second = observe("quota", "screen_notice")
        self.assertNotEqual(first, second)
        append("• 已生成下一批结果")
        without_punctuation = observe("recovered", "changed_activity")
        quiet()
        append("Quota exceeded")
        third = observe("quota", "screen_notice")
        append("• 2 files changed.")
        numeric = observe("recovered", "changed_activity")
        self.assertEqual(len({with_punctuation, without_punctuation, numeric}), 3)
        self.assertEqual(self.data("close", job["dispatch"])["result"], "completed")
        print(json.dumps({"case": "codex-unicode-appended-activity", "scope": job["scope"],
                          "attempt": job["attempt"], "quotaReceipts": [first, second, third],
                          "recoveryReceipts": [with_punctuation, without_punctuation, numeric],
                          "ordinaryFootersSilent": True}), flush=True)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_timer_redraw_does_not_delay_stall(self):
        self.start_private_tmux()
        fixture = textwrap.dedent(r"""
            import time
            from pathlib import Path
            frame = 0
            while True:
                frame += 1
                body = "Ready."
                if Path("real-output").exists():
                    body += "\n• 已生成实际结果。"
                print(f"\x1b[2J\x1b[H{body}\n• Working (1h2m{frame}s • esc to interrupt)", flush=True)
                Path("frame").write_text(str(frame))
                time.sleep(.09)
            """)
        config = self.root / "launch.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, "-c", fixture]}}}))
        job = self.data("dispatch", "--task", "task", "--role", "worker", "--scope", "ours",
                        "--report", "task", "--cwd", ".", "--host", "codex", "--launch-config", str(config))
        dispatch = job["dispatch"]
        wait = ("wait", "--scope", "ours", "--dispatch", dispatch, "--stall-after", "1.3")
        self.assertFalse(self.data(*wait, "--timeout", ".65", code=3)["messages"])
        watch = self.root / "state" / "dispatches" / f"{dispatch}.host-watch"
        started = json.loads(watch.read_text())["since"]
        frame = self.root / "frame"
        first = self.until(lambda: frame.read_text() if frame.exists() else "")
        stalled, = self.data(*wait, "--timeout", "4")["messages"]
        self.assertEqual((stalled["type"], stalled["fault"]["evidence"]),
                         ("stalled", "unchanged_screen"))
        self.assertGreaterEqual(stalled["fault"]["elapsedSeconds"], 1)
        last = self.until(lambda: frame.read_text())
        self.assertGreater(int(last), int(first) + 2)
        self.assertEqual(json.loads(watch.read_text())["since"], started)
        self.assertEqual(self.data("ack", stalled["messageId"])["state"], "processed")
        (self.root / "real-output").touch()
        recovered, = self.data(*wait, "--timeout", "3")["messages"]
        self.assertEqual((recovered["type"], recovered["fault"]["evidence"]),
                         ("recovered", "changed_activity"))
        self.assertGreater(json.loads(watch.read_text())["since"], started)
        for receipt in (stalled, recovered):
            self.assertEqual((receipt["dispatch"], receipt["attempt"]), (dispatch, job["attempt"]))
        self.assertEqual(self.data("ack", recovered["messageId"])["state"], "processed")
        self.assertEqual(self.data("close", dispatch)["result"], "completed")
        print(json.dumps({"case": "timer-redraw", "firstFrame": int(first), "lastFrame": int(last),
                          "stall": stalled["messageId"], "recovery": recovered["messageId"]}), flush=True)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_runtime_scope_takeover_fences_the_inflight_read(self):
        self.start_private_tmux()
        config = self.root / "launch.json"
        program = textwrap.dedent("""
            import time
            from pathlib import Path
            print("Ready.", flush=True)
            while not Path("quota").exists():
                time.sleep(.025)
            print("Quota exceeded", flush=True)
            time.sleep(60)
            """)
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, "-c", program]}}}))
        job = self.data("dispatch", "--task", "task", "--role", "worker", "--scope", "ours",
                        "--report", "task", "--cwd", ".", "--host", "codex", "--launch-config", str(config))
        first = self.data("owner", "acquire", "--scope", "ours", "--owner", "first")
        old = {"WFC_CONTROLLER_TOKEN": first["token"]}
        wait = ("wait", "--scope", "ours", "--dispatch", job["dispatch"], "--stall-after", "0")
        self.assertFalse(self.data(*wait, "--timeout", ".65", code=3, **old)["messages"])
        watch = self.root / "state" / "dispatches" / f'{job["dispatch"]}.host-watch'
        bound = json.loads(watch.read_text())["execution"]
        sentinel = self.publish("neighbor")
        slow, probes = self.slow_tmux_probe()
        slow.touch()
        waiting = subprocess.Popen([WFC, *wait, "--timeout", "5"], cwd=self.root,
                                   env=dict(self.env, **old), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)

        def cleanup():
            if waiting.poll() is None:
                waiting.kill()
            waiting.communicate(timeout=5)

        self.addCleanup(cleanup)
        self.until(probes)
        current = self.data("owner", "takeover", "--scope", "ours", "--owner", "current")
        new = {"WFC_CONTROLLER_TOKEN": current["token"]}
        stdout, stderr = waiting.communicate(timeout=5)
        self.assertEqual((waiting.returncode, stdout, json.loads(stderr)["error"]),
                         (1, "", "stale_controller_owner"))
        slow.unlink()
        self.assertEqual(json.loads(watch.read_text())["execution"], bound)
        self.assertFalse(self.data(*wait, "--timeout", ".65", code=3, **new)["messages"])
        [neighbor] = self.data("wait", "--scope", "neighbor", "--timeout", "0")["messages"]
        self.assertEqual(neighbor["messageId"], sentinel["messageId"])

        tmux = [shutil.which("tmux"), "-S", self.env["WFC_TMUX_SOCKET"]]
        session = f'wfc-{job["dispatch"]}'
        subprocess.run([*tmux, "display-message", "-t", session, "-p", "#{pane_id}"],
                       capture_output=True, check=True, timeout=5)
        # Deliver an actual host notice to the verified pane via the worker.
        # A marker file lets the fixture's stdout supply the text.
        (self.root / "quota").touch()
        [message] = self.data(*wait, "--timeout", "3", **new)["messages"]
        self.assertEqual((message["type"], message["kind"], message["fault"]["category"]),
                         ("blocked", "host_runtime", "quota"))
        for key in ("dispatch", "round", "attempt", "scope"):
            self.assertEqual(message[key], job[key])
        stale = subprocess.run([WFC, "ack", message["messageId"]], cwd=self.root,
                               env=dict(self.env, **old), capture_output=True, text=True, timeout=5)
        self.assertEqual((stale.returncode, json.loads(stale.stderr)["error"]),
                         (1, "stale_controller_owner"))
        ack = self.data("ack", message["messageId"], **new)
        self.assertEqual((ack["messageId"], ack["state"]), (message["messageId"], "processed"))
        self.assertTrue(self.data("ack", message["messageId"], **new)["duplicate"])
        self.assertEqual(self.data("ack", neighbor["messageId"])["state"], "processed")
        self.assertEqual(self.data("close", job["dispatch"], **new)["result"], "completed")
        print(json.dumps({"case": "runtime-scope-takeover", "scope": job["scope"],
                          "attempt": job["attempt"], "generation": current["generation"],
                          "receipt": message["messageId"], "neighborReceipt": neighbor["messageId"]}),
              flush=True)

    def test_detached_scope(self):
        other = self.publish("other")
        args = ["relay", "--scope", "ours", "--codex", "--detach", "--for", "1.5"]
        child = self.data(*args)
        pid = child["pid"]
        self.assertNotEqual(os.getsid(pid), os.getsid(0))
        self.until(lambda: any(w["pid"] == pid and w["scope"] == "ours" and w["kind"] == "relay"
                               for w in self.data("status")["waiters"]))
        wanted = self.publish("ours")
        self.until(self.calls)
        self.until(lambda: not self.data("status")["waiters"])
        command, = self.calls()
        self.assertEqual(command[:4], ["queue", "--thread", self.thread, "--message"])
        self.assertEqual(command[5:], ["--resume"])
        self.assertTrue(command[4].startswith(PREFIX))
        m, = json.loads(command[4][len(PREFIX):])["messages"]
        self.assertEqual((m["messageId"], m["id"], m["scope"], m["state"], m["delivery"], m["deliveryAttempt"]),
                         (wanted["messageId"], wanted["message"], "ours", "unknown", "pending",
                          "relay:" + wanted["messageId"] + ":2"))
        ack = self.data("ack", wanted["messageId"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))
        pending, = self.data("wait", "--scope", "other", "--timeout", "0")["messages"]
        self.assertEqual(pending["messageId"], other["messageId"])
        def exited():
            stat = Path(f"/proc/{pid}/stat")
            try:
                return stat.read_text().rsplit(")", 1)[1].split()[0] == "Z"
            except FileNotFoundError:
                return True
        self.until(exited)
        print(json.dumps({"controlledQueue": self.thread, "pid": pid, "relayExited": True,
                          "message": m["id"], "messageId": m["messageId"], "ack": ack,
                          "otherScopeMessageId": pending["messageId"]}), flush=True)

    def foreground(self, seconds, scope="ours", **env):
        p = subprocess.Popen([WFC, "relay", "--scope", scope, "--codex", "--for", str(seconds)],
                             env=dict(self.env, **env), cwd=self.root,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        def cleanup():
            if p.poll() is None:
                p.kill()
            p.communicate(timeout=5)
        self.addCleanup(cleanup)
        return p

    def test_two_scopes_target_separate_threads_across_batches(self):
        other_thread = str(uuid.uuid4())
        other_rollout = self.rollout.with_name(f"rollout-time-{other_thread}.jsonl")
        other_rollout.write_text(json.dumps({"type": "session_meta", "payload": {"id": other_thread}}))
        initial = [self.publish("ours") for _ in range(2)]
        other = self.publish("theirs")
        runners = [self.foreground(3), self.foreground(3, scope="theirs", CODEX_THREAD_ID=other_thread)]
        self.until(lambda: len(self.calls()) >= 2)
        later = self.publish("ours")
        self.until(lambda: len(self.calls()) >= 3)
        for proc in runners:
            output = proc.communicate(timeout=5)
            self.assertEqual(proc.returncode, 0, output)
        by_thread = {}
        for cmd in self.calls():
            batch = json.loads(cmd[4][len(PREFIX):])["messages"]
            by_thread.setdefault(cmd[2], []).append([m["messageId"] for m in batch])
        self.assertEqual(by_thread, {self.thread: [[m["messageId"] for m in initial], [later["messageId"]]],
                                     other_thread: [[other["messageId"]]]})

    def test_two_relays_and_wait_claim_once_and_resolution_wins(self):
        first, second = self.publish("ours"), self.publish("ours")
        gate = self.root / "gate"
        gate.touch()
        self.addCleanup(gate.unlink, missing_ok=True)
        runners = [self.foreground(2.0, WFC_CONTROLLER_TOKEN="private-test-token",
                                   wfc_owner_token="private-test-token") for _ in range(2)]
        self.until(self.calls)
        [cmd] = self.calls()
        batch = json.loads(cmd[4][len(PREFIX):])["messages"]
        self.assertEqual([m["messageId"] for m in batch], [first["messageId"], second["messageId"]])
        self.assertEqual([m["state"] for m in batch], ["unknown", "unknown"])
        self.data("wait", "--timeout", "0", code=3)
        self.assertEqual([m["state"] for m in self.data("status")["undelivered"]], ["unknown"] * 2)
        m = batch[0]
        body = {k: m[k] for k in ("attempt", "head", "dirty", "report", "role", "round", "scope")}
        digest = hashlib.sha256(json.dumps(body, ensure_ascii=False, sort_keys=True).encode()).hexdigest()
        claim = ["resolve", m["messageId"], "--actor", "reviewer", "--reason", "observed",
                 "--evidence", "private queue record", "--body-hash", digest]
        self.data(*claim)
        ack = self.data("ack", m["messageId"])
        gate.unlink()
        for r in runners:
            output = r.communicate(timeout=5)
            self.assertEqual(r.returncode, 0, output)
        self.assertEqual(len(self.calls()), 1)
        self.assertEqual(self.data(*claim), dict(ack, duplicate=True))
        self.assertEqual(self.data("ack", second["messageId"])["statusVersion"], 4)

    def test_late_rejection_settles_only_its_original_attempts(self):
        first, second = [self.publish("ours") for _ in range(2)]
        (self.root / "gate").touch()
        proc = self.foreground(8, OUTCOME="missing")
        self.until(self.calls)
        call, = self.calls()
        m, other = json.loads(call[4][len(PREFIX):])["messages"]
        self.assertEqual([m["deliveryAttempt"], other["deliveryAttempt"]],
                         ["relay:" + r["messageId"] + ":2" for r in (first, second)])
        body = {k: m[k] for k in ("attempt", "head", "dirty", "report", "role", "round", "scope")}
        digest = hashlib.sha256(json.dumps(body, ensure_ascii=False, sort_keys=True).encode()).hexdigest()
        self.data("resolve", first["messageId"], "--actor", "reviewer", "--reason", "observed",
                  "--evidence", "private queue record", "--body-hash", digest)
        self.data("ack", first["messageId"])
        later = self.publish("ours")
        (self.root / "gate").unlink()
        out = proc.communicate(timeout=5)
        self.assertEqual(proc.returncode, 1, out)
        self.assertEqual(self.data("ack", first["messageId"])["statusVersion"], 4)
        remaining = self.data("status")["undelivered"]
        self.assertEqual([(r["messageId"], r["state"], r["statusVersion"]) for r in remaining],
                         [(second["messageId"], "unsent", 3), (later["messageId"], "unsent", 1)])
        self.cli("relay", "--codex", "--for", ".6")
        replay = json.loads(self.calls()[1][4][len(PREFIX):])["messages"]
        self.assertEqual([r["deliveryAttempt"] for r in replay],
                         ["relay:" + second["messageId"] + ":4", "relay:" + later["messageId"] + ":2"])

    def test_queue_outcomes_and_restart(self):
        args = ["relay", "--codex-thread", self.thread.upper(), "--scope", "ours", "--for", ".8"]
        for mode in ("missing", "error", "timeout", "absent"):
            with self.subTest(mode=mode):
                m = self.publish("ours")
                prior = len(self.calls())
                env = {}
                if mode == "absent":
                    self.queue.rename(self.root / "disabled")
                elif mode == "timeout":
                    (self.root / "gate").touch()
                    env = {"WFC_QUEUE_TIMEOUT": ".5"}
                else:
                    env = {"OUTCOME": mode}
                self.cli(*args, code=int(mode == "missing"), **env)
                (self.root / "gate").unlink(missing_ok=True)
                if mode == "absent":
                    (self.root / "disabled").rename(self.queue)
                self.assertEqual(len(self.calls()) - prior, int(mode != "absent"))
                self.cli(*args)
                uncertain = mode in ("error", "timeout")
                self.assertEqual(len(self.calls()) - prior, int(mode != "absent") + int(not uncertain))
                if uncertain:
                    self.cli("ack", m["messageId"], code=1)
                    self.data("wait", "--timeout", "0", code=3)
                else:
                    self.assertEqual(self.data("ack", m["messageId"])["state"], "processed")

    def test_ambiguous_queue_diagnostics_stay_unknown(self):
        self.queue.write_text(f"#!{sys.executable}\n" + """
import sys
print('codex queue: no rollout found for thread 00000000-0000-0000-0000-000000000000')
print('previous diagnostic: no rollout found for thread ' + sys.argv[3])
print('no rollout found for thread ' + sys.argv[3] + ' ' * 600 + 'might occur if offline')
sys.exit(1)
""")
        m = self.publish("ours")
        self.cli("relay", "--codex", "--for", ".5")
        row, = self.data("status")["undelivered"]
        self.assertEqual((row["messageId"], row["state"], row["statusVersion"]), (m["messageId"], "unknown", 2))

    def test_contradictory_refusal_never_resends(self):
        self.queue.write_text(f"#!{sys.executable}\n" + """
import os, sys
from pathlib import Path
with Path('accepted').open('a') as output:
    output.write(sys.argv[5] + '\\n')
refusal = 'Error: no rollout found for thread ' + sys.argv[3]
lines = [refusal, 'Final result: confirmation lost after submission']
if os.environ.get('REVERSE'):
    lines.reverse()
print(*lines, sep='\\n', file=sys.stderr)
sys.exit(1)
""")
        for reverse in ("", "1"):
            receipt = self.publish("ours")
            self.cli("relay", "--codex", "--scope", "ours", "--for", ".4", REVERSE=reverse)
            self.cli("relay", "--codex", "--scope", "ours", "--for", ".1")
            received = (self.root / "accepted").read_text().splitlines()
            accepted = [json.loads(line[len(PREFIX):])["messages"][0]["messageId"] for line in received]
            self.assertEqual(accepted.count(receipt["messageId"]), 1)
            row = self.data("status")["undelivered"][-1]
            self.assertEqual((row["messageId"], row["state"], row["statusVersion"]),
                             (receipt["messageId"], "unknown", 2))

    def test_ordinary_backlog_preserves_queue_order_across_relays(self):
        scope = "ordinary-review"
        report = "reports/ordinary-review/release-candidate/dispatch-result.md"
        (self.root / report).parent.mkdir(parents=True)
        (self.root / report).write_text("completed review")
        d = self.data("dispatch", "--task", "task", "--scope", scope, "--role", "release-reviewer",
                      "--report", report, "--cwd", ".", "--", "/bin/true")
        receipts = [self.data("done", "--report", report, "--attempt", d["attempt"], "--message-id",
                              f"receipt-{i}", WFC_DISPATCH=d["dispatch"]) for i in range(291)]
        (self.root / "gate").touch()
        runners = [self.foreground(5, scope=scope) for _ in range(2)]
        self.until(self.calls)
        self.assertEqual(len(self.calls()), 1)
        (self.root / "gate").unlink()
        for proc in runners:
            output = proc.communicate(timeout=8)
            self.assertEqual(proc.returncode, 0, output)
        self.cli("relay", "--codex", "--scope", scope, "--for", ".1")
        batches = [cmd[4] for cmd in self.calls()]
        sizes = [len(text.encode()) + 1 for text in batches]
        groups = [json.loads(text[len(PREFIX):])["messages"] for text in batches]
        if os.sysconf("SC_PAGE_SIZE") == 4096:
            self.assertGreater(len(sizes), 1)
        self.assertLessEqual(max(sizes), 32 * os.sysconf("SC_PAGE_SIZE"))
        originals = [(r["message"], r["messageId"]) for r in receipts]
        self.assertEqual([(m["id"], m["messageId"]) for group in groups for m in group], originals)
        self.assertEqual([m["deliveryAttempt"] for group in groups for m in group],
                         [f"relay:{mid}:2" for _, mid in originals])
        self.data("wait", "--scope", scope, "--timeout", "0", code=3)
        for _, mid in originals:
            ack = self.data("ack", mid)
            self.assertEqual((ack["messageId"], ack["state"], ack["statusVersion"]), (mid, "processed", 4))
        self.assertTrue(self.data("ack", originals[-1][1])["duplicate"])
        print(json.dumps({"batchSizesIncludingNul": sizes, "batchCounts": list(map(len, groups)),
                          "originalAcks": len(originals), "first": originals[0], "last": originals[-1]}))

    def test_escaped_single_over_limit_leaves_legal_messages_available(self):
        first = self.publish("ours")
        role = '"' * (16 * os.sysconf("SC_PAGE_SIZE"))
        d = self.data("dispatch", "--task", "task", "--scope", "ours", "--role", role,
                      "--report", "task", "--cwd", ".", "--", "/bin/true")
        large = self.data("done", "--report", "task", "--attempt", d["attempt"], WFC_DISPATCH=d["dispatch"])
        last = self.publish("ours")
        other = self.publish("theirs")
        diagnostic = self.cli("relay", "--codex", "--scope", "ours", "--for", ".7")
        self.assertEqual(diagnostic.count(large["messageId"]), 1)
        cmd, = self.calls()
        messages = json.loads(cmd[4][len(PREFIX):])["messages"]
        self.assertEqual([m["messageId"] for m in messages], [first["messageId"], last["messageId"]])
        remaining = self.data("status")["undelivered"]
        self.assertEqual([(r["messageId"], r["state"], r["statusVersion"]) for r in remaining],
                         [(r["messageId"], "unsent", 1) for r in (large, other)])
        row, = self.data("wait", "--scope", "ours", "--timeout", "0")["messages"]
        self.assertEqual(row["messageId"], large["messageId"])
        for receipt in (first, large, last):
            self.assertEqual(self.data("ack", receipt["messageId"])["state"], "processed")
        print(json.dumps({"oversized": diagnostic.strip(), "queueIds": [m["messageId"] for m in messages],
                          "manualId": row["messageId"], "other": other["messageId"]}))

    def test_shorthand_home_validates_and_launches_in_the_same_home(self):
        self.queue.write_text(f"#!{sys.executable}\n" + """
import os, sys
from pathlib import Path
assert Path(os.environ['CODEX_HOME']) == Path.home() / '.codex'
assert list((Path(os.environ['CODEX_HOME']) / 'sessions').rglob('*-' + sys.argv[3] + '.jsonl'))
""")
        for home in ("~/.codex", str(self.root / ".codex")):
            m = self.publish("ours")
            self.cli("relay", "--codex", "--for", ".5", CODEX_HOME=home)
            self.assertEqual(self.data("ack", m["messageId"])["state"], "processed")
        self.meta(self.thread, {"subagent": {}})
        self.cli("relay", "--codex", "--for", ".1", CODEX_HOME="~/.codex", code=1)

    def test_queue_group_cleanup_preserves_detached_host(self):
        # Hold all three identities live until their pidfds are open. The host
        # represents a service which starts a new session for its own lifetime.
        self.queue.write_text(f"#!{sys.executable}\n" + """
import json, os, subprocess, sys, time
from pathlib import Path
helper = subprocess.Popen(['/bin/sleep', '12'])
host = subprocess.Popen(['/bin/sleep', '12'], start_new_session=True, stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
Path('pids.tmp').write_text(json.dumps([os.getpid(), helper.pid, host.pid, os.getppid()]))
Path('pids.tmp').rename('pids')
while not Path('release').exists():
    time.sleep(.01)
""")
        def exited(fd, millis):
            poller = select.poll()
            poller.register(fd, select.POLLIN)
            return bool(poller.poll(millis))

        def cleanup(fd):
            try:
                if not exited(fd, 0):
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                self.assertTrue(exited(fd, 2000))
            finally:
                os.close(fd)

        for outcome in ("success", "timeout", "relay-killed", "supervisor-killed"):
            with self.subTest(outcome=outcome):
                receipt = self.publish("ours")
                proc = self.foreground(1.5 if outcome == "timeout" else 4)
                pids_path = self.root / "pids"
                self.until(pids_path.exists)
                queue, helper, host, supervisor = json.loads(pids_path.read_text())
                fds = []
                for pid in (queue, helper, host, supervisor):
                    fd = os.pidfd_open(pid)
                    self.addCleanup(cleanup, fd)
                    fds.append(fd)
                self.assertEqual(os.getpgid(helper), queue)
                self.assertEqual(os.getsid(host), host)
                if outcome == "relay-killed":
                    proc.kill()
                elif outcome == "supervisor-killed":
                    signal.pidfd_send_signal(fds[3], signal.SIGKILL)
                elif outcome == "success":
                    (self.root / "release").touch()
                out = proc.communicate(timeout=6)
                self.assertEqual(proc.returncode, -signal.SIGKILL if outcome == "relay-killed" else 0, out)
                self.assertTrue(exited(fds[0], 1000))
                self.assertTrue(exited(fds[1], 1000))
                self.assertFalse(exited(fds[2], 0))
                self.assertEqual(self.data("status")["waiters"], [])
                if outcome == "supervisor-killed":
                    row = self.data("status")["undelivered"][-1]
                    self.assertEqual((row["messageId"], row["state"], row["statusVersion"]),
                                     (receipt["messageId"], "unknown", 2))
                pids_path.unlink()
                if outcome == "supervisor-killed":
                    (self.root / "release").touch()
                    self.cli("relay", "--codex", "--scope", "ours", "--for", ".3")
                    self.assertFalse(pids_path.exists())
                (self.root / "release").unlink(missing_ok=True)

    def test_scoped_delivery_beside_complete_and_truncated_reservations(self):
        def publish(scope):
            dispatch = self.data("dispatch", "--task", "task", "--report", "task", "--role", "review",
                                 "--scope", scope, "--cwd", ".", "--", "/bin/true")
            receipt = self.data("done", "--report", "task", "--attempt", dispatch["attempt"],
                                WFC_DISPATCH=dispatch["dispatch"])
            path = Path(self.env["WFC_STATE"]) / "dispatches" / (dispatch["dispatch"] + ".json")
            return receipt, path

        def deliver(scope):
            self.cli("relay", "--codex", "--scope", scope, "--for", "1.5")

        def confirm(receipt):
            ack = self.data("ack", receipt["messageId"])
            self.assertEqual((ack["message"], ack["state"], ack["statusVersion"]),
                             (receipt["message"], "processed", 4))
            return ack

        other, path = publish("neighbor")
        pending = path.with_suffix(".pending")
        path.rename(pending)
        first = self.publish("ready")
        deliver("ready")
        full_ack = confirm(first)

        damaged, path = publish("ready")
        torn = path.with_suffix(".pending")
        path.rename(torn)
        fragment = b'{"dispatch":'
        torn.write_bytes(fragment)
        stale, stale_path = publish("ready")
        row = json.loads(stale_path.read_bytes())
        row["attempt"] = "a-000000000000"
        stale_path.write_text(json.dumps(row))
        protected, owner_path = publish("owned")
        row = json.loads(owner_path.read_bytes())
        row["controllerOwner"] = {"owner": "fixture", "generation": 1}
        owner_path.write_text(json.dumps(row))
        second = self.publish("ready")
        self.cli("relay", "--codex", "--for", ".1", code=1)
        self.cli("relay", "--codex", "--scope", "owned", "--for", ".1", code=1)
        deliver("ready")
        torn_ack = confirm(second)
        deliver("ready")
        sent = [m["messageId"] for call in self.calls()
                for m in json.loads(call[4][len(PREFIX):])["messages"]]
        self.assertEqual(sent, [first["messageId"], second["messageId"]])
        for r in (other, damaged, stale, protected):
            self.cli("ack", r["messageId"], code=1)
        ledger = json.loads((Path(self.env["WFC_STATE"]) / "messages.json").read_bytes())
        self.assertEqual({(r["messageId"], r["state"], r["statusVersion"]) for r in ledger if r["state"] != "processed"},
                         {(r["messageId"], "unsent", 1) for r in (other, damaged, stale, protected)})
        self.assertEqual(torn.read_bytes(), fragment)
        self.assertTrue(pending.is_file())
        print(json.dumps({"completePending": pending.name, "completeAck": full_ack,
                          "truncatedPending": torn.name, "truncatedNeighborAck": torn_ack,
                          "untouched": {"differentScope": other["messageId"], "damaged": damaged["messageId"],
                                        "differentAttempt": stale["messageId"], "owned": protected["messageId"]}}))

    def test_conflicting_complete_generations_preserve_owner_and_identity(self):
        original = self.publish("ours")
        ready = self.publish("ours")
        rows = self.data("status")["undelivered"]
        path = Path(self.env["WFC_STATE"]) / "dispatches" / (rows[0]["dispatch"] + ".json")
        reservation = json.loads(path.read_bytes())
        pending = path.with_suffix(".pending")
        reservation["controllerOwner"] = {"owner": "fixture"}
        pending.write_text(json.dumps(reservation))
        self.cli("relay", "--codex", "--scope", "ours", "--for", ".2", code=1)
        self.assertEqual(self.calls(), [])
        reservation.pop("controllerOwner")
        reservation["attempt"] = "a-000000000001"
        pending.write_text(json.dumps(reservation))
        self.cli("relay", "--codex", "--scope", "ours", "--for", "1.5")
        self.assertEqual(self.data("ack", ready["messageId"])["state"], "processed")
        self.cli("ack", original["messageId"], code=1)
        call, = self.calls()
        row, = json.loads(call[4][len(PREFIX):])["messages"]
        self.assertEqual(row["messageId"], ready["messageId"])

    def test_running_relay_refreshes_repaired_and_legacy_owner_records(self):
        state = Path(self.env["WFC_STATE"])
        directory = state / "dispatches"
        blocked = self.publish("ours")
        held = self.data("dispatch", "--task", "task", "--report", "task", "--role", "fixture",
                         "--scope", "ours", "--cwd", ".", "--", "/bin/true")
        owner = self.data("dispatch", "--task", "task", "--report", "task", "--role", "fixture",
                          "--scope", "ours", "--cwd", ".", "--", "/bin/true")
        other = self.publish("theirs")
        undelivered = {m["messageId"]: m["dispatch"] for m in self.data("status")["undelivered"]}
        blocked_dispatch, other_dispatch = (undelivered[r["messageId"]] for r in (blocked, other))
        original = directory / f"{blocked_dispatch}.json"
        pending = original.with_suffix(".pending")
        pending.write_text("{")
        # Finite historical records have no messages. They still belong to
        # the legacy owner census, but cannot spend the short delivery window.
        for index in range(1, 35):
            ident = f"d-{index:08x}"
            (directory / f"{ident}.json").write_text(json.dumps({
                "dispatch": ident, "scope": "history", "round": 1, "attempt": "a-123456789abc",
                "role": "fixture", "cwd": str(self.root), "report": str(self.root / "task"),
            }))
        for ident in (blocked_dispatch, held["dispatch"], owner["dispatch"], other_dispatch):
            self.until(lambda: self.data("status", "--dispatch", ident)["dispatches"][0]
                       ["worker"]["state"] == "exited")
        directory_version = directory.stat().st_ctime_ns
        relay = self.foreground(7)
        self.until(lambda: any(w["pid"] == relay.pid for w in self.data("status")["waiters"]))
        self.assertEqual(self.calls(), [])

        pending.write_bytes(original.read_bytes())
        self.assertEqual(directory.stat().st_ctime_ns, directory_version)
        first, = self.until(self.calls)
        repaired, = json.loads(first[4][len(PREFIX):])["messages"]
        self.assertEqual(repaired["messageId"], blocked["messageId"])
        self.until(lambda: any(m["messageId"] == repaired["messageId"] and m["state"] == "received"
                               for row in self.data("status", "--dispatch", repaired["dispatch"])["dispatches"]
                               for m in row["messages"]))
        self.assertEqual(self.data("ack", repaired["messageId"])["state"], "processed")

        guard = state / f"relay-{hashlib.sha256(b'ours').hexdigest()}.lock"
        with guard.open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            wanted = self.data("done", "--report", "task", "--attempt", held["attempt"],
                               WFC_DISPATCH=held["dispatch"])
            path = directory / f"{owner['dispatch']}.json"
            marked = json.loads(path.read_bytes())
            marked["controllerOwner"] = {"owner": "historical fixture"}
            path.write_text(json.dumps(marked))
            self.assertEqual(directory.stat().st_ctime_ns, directory_version)
            stdout, stderr = relay.communicate(timeout=4)
            self.assertEqual((relay.returncode, stdout), (1, ""), stderr)
            self.assertIn("controller owner authorization required for relay", stderr)
        self.assertEqual(len(self.calls()), 1)

        self.cli("relay", "--scope", "theirs", "--codex", "--for", ".2")
        second = self.calls()[1]
        neighbor, = json.loads(second[4][len(PREFIX):])["messages"]
        self.assertEqual(neighbor["messageId"], other["messageId"])
        self.assertEqual(self.data("ack", neighbor["messageId"])["state"], "processed")
        remaining = self.data("status")["undelivered"]
        self.assertEqual([(m["messageId"], m["state"]) for m in remaining],
                         [(wanted["messageId"], "unsent")])
        print(json.dumps({"repaired": repaired["messageId"], "ownerFence": wanted["messageId"],
                          "neighbor": neighbor["messageId"], "sameRelayPid": relay.pid}), flush=True)

    def test_short_relay_retries_across_ordinary_neighbor_updates(self):
        target = self.publish("ours")
        neighbor = self.publish("theirs")
        messages = {m["messageId"]: m for m in self.data("status")["undelivered"]}
        directory = Path(self.env["WFC_STATE"]) / "dispatches"
        path = directory / (messages[neighbor["messageId"]]["dispatch"] + ".json")
        row = json.loads(path.read_bytes())
        for index in range(1, 41):
            ident = f"d-{index:08x}"
            (directory / f"{ident}.json").write_text(json.dumps({
                "dispatch": ident, "scope": "ours", "round": 1, "attempt": "a-123456789abc",
                "role": "fixture", "cwd": str(self.root), "report": str(self.root / "task"),
            }))

        stop, changed = Event(), Event()

        def update_neighbor():
            for revision in range(24):
                scratch = path.with_suffix(".fixture")
                scratch.write_text(json.dumps({**row, "fixtureRevision": revision}))
                scratch.replace(path)
                changed.set()
                if stop.wait(.01):
                    break
            return revision + 1

        with ThreadPoolExecutor(max_workers=1) as writers:
            writer = writers.submit(update_neighbor)
            try:
                self.assertTrue(changed.wait(2))
                short = self.foreground(.35)
                _, stderr = short.communicate(timeout=5)
                self.assertEqual(short.returncode, 0, stderr)
                delivered_in_window = bool(self.calls())
            finally:
                stop.set()
            updates = writer.result(timeout=2)
        self.assertGreater(updates, 1)

        self.cli("relay", "--scope", "ours", "--codex", "--for", ".5")
        call, = self.calls()
        message, = json.loads(call[4][len(PREFIX):])["messages"]
        self.assertEqual((message["id"], message["messageId"]), (target["message"], target["messageId"]))
        self.assertEqual(self.data("ack", target["messageId"])["state"], "processed")
        remaining, = self.data("status")["undelivered"]
        self.assertEqual((remaining["messageId"], remaining["state"]), (neighbor["messageId"], "unsent"))
        print(json.dumps({"shortWindowDelivered": delivered_in_window, "neighborUpdates": updates,
                          "target": target["messageId"],
                          "neighborUnsent": neighbor["messageId"]}), flush=True)

    def test_thread_and_owner_preflight(self):
        args = ["relay", "--codex-thread", self.thread, "--scope", "ours", "--detach", "--for", ".1"]
        self.publish("ours")
        self.cli(*args, CODEX_THREAD_ID=str(uuid.uuid4()), code=1)
        self.meta(str(uuid.uuid4()))
        self.cli(*args, code=1)
        self.meta(self.thread, {"subagent": {}})
        self.cli(*args, code=1)
        self.rollout.unlink()
        self.cli(*args, code=1)
        self.meta(self.thread)
        self.cli(*args, WFC_WORKER_CALLBACK="fixture", code=1)
        file, = (self.root / "state/dispatches").glob("*.json")
        record = json.loads(file.read_text())
        record["controllerOwner"] = {"owner": "another controller", "generation": 1}
        file.write_text(json.dumps(record))
        self.cli(*args, code=1)
        self.cli("relay", "--codex", "--detach", "--for", ".1", code=1)
        self.assertEqual(self.calls(), [])
        self.assertFalse((self.root / "state/relay.log").exists())
        self.assertEqual(self.data("status")["waiters"], [])

    def test_relay_exit_during_queue_preserves_claim(self):
        self.publish("ours")
        (self.root / "gate").touch()
        child = self.foreground(0)
        self.until(self.calls)
        child.kill()
        child.communicate(timeout=5)
        (self.root / "gate").unlink()
        stale = self.root / "state/relays/stale.json"
        os.mkfifo(stale)
        self.assertEqual(self.data("status")["waiters"], [])
        self.cli("relay", "--codex", "--scope", "ours", "--for", ".15")
        self.assertEqual(len(self.calls()), 1)
        self.assertEqual(self.data("status")["undelivered"][0]["state"], "unknown")

    def test_deadline_bounds_locked_inbox_and_slow_queue(self):
        self.cli("relay", "--codex", "--for", "1e-12")
        self.publish("ours")
        with (self.root / "state/messages.lock").open("r") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.cli("relay", "--codex", "--scope", "ours", "--for", ".2")
        self.assertEqual(self.calls(), [])
        (self.root / "gate").touch()
        self.cli("relay", "--codex", "--scope", "ours", "--for", "1", WFC_QUEUE_TIMEOUT="30")
        self.assertEqual(len(self.calls()), 1)
        self.assertEqual(self.data("status")["waiters"], [])
        self.assertEqual(self.data("status")["undelivered"][0]["state"], "unknown")


if __name__ == "__main__":
    unittest.main()
