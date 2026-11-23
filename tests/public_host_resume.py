"""Controlled Claude public resume and the gated OpenCode public entry in private tmux."""
import json
import os
from pathlib import Path
import shlex
import sqlite3
import subprocess
import sys
import tempfile
import time
import tty
import unittest


CALLBACK_KEYS = ("WFC_DISPATCH", "WFC_ATTEMPT", "WFC_ROUND", "WFC_WORKER_CALLBACK")
OPENCODE_ID = "ses_r20_fixture"


def controlled_worker(host, argv):
    cwd = Path.cwd()
    attempt = os.environ["WFC_ATTEMPT"]
    trace = {
        "argv": argv, "task": os.environ["WFC_TASK"],
        "callback": {key: os.environ[key] for key in CALLBACK_KEYS},
    }
    if host == "claude":
        with (cwd / "claude-starts").open("a", encoding="utf-8") as starts:
            starts.write(f"{attempt}\n")
        resumed = "--resume" in argv
        flag = "--resume" if resumed else "--session-id"
        session = argv[argv.index(flag) + 1]
        plugin = Path(argv[argv.index("--plugin-dir") + 1])
        transcript = Path(os.environ["CLAUDE_CONFIG_DIR"]) / "projects" / "fixture" / (session + ".jsonl")
        if not resumed:
            transcript.parent.mkdir(parents=True, exist_ok=True)
            transcript.write_text('{"type":"user","synthetic":true}\n', encoding="utf-8")
        skip = os.environ.get("FIXTURE_SKIP_CLAUDE_HOOK")
        if skip != ("resume" if resumed else "startup"):
            settings = json.loads((plugin / "hooks/hooks.json").read_text(encoding="utf-8"))
            (entry,) = settings["hooks"]["SessionStart"]
            (hook,) = entry["hooks"]
            source = "resume" if resumed else "startup"
            assert entry["matcher"] == source
            event = {
                "hook_event_name": "SessionStart", "source": source, "session_id": session,
                "cwd": str(cwd), "transcript_path": str(transcript),
            }
            subprocess.run(
                ["/bin/sh", "-c", hook["command"]], input=json.dumps(event),
                text=True, capture_output=True, timeout=5, check=True,
            )
    else:
        brief = argv[argv.index("--prompt") + 1]
        store = Path(os.environ["OPENCODE_DB"])
        with sqlite3.connect(store) as db:
            db.executescript("""
                CREATE TABLE project(id TEXT PRIMARY KEY,worktree TEXT NOT NULL);
                CREATE TABLE session(id TEXT PRIMARY KEY,project_id TEXT,directory TEXT,parent_id TEXT);
                CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);
                CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,session_id TEXT,data TEXT);
            """)
            db.execute("INSERT INTO project VALUES('fixture',?)", (str(cwd),))
            db.execute("INSERT INTO session VALUES(?,'fixture',?,NULL)", (OPENCODE_ID, str(cwd)))
            db.execute("INSERT INTO message VALUES('opening',?,1,?)",
                       (OPENCODE_ID, json.dumps({"role": "user"})))
            db.execute("INSERT INTO part VALUES('brief','opening',?,?)",
                       (OPENCODE_ID, json.dumps({"type": "text", "text": brief})))
        trace["store"] = str(store)
    temp = cwd / f"{attempt}.tmp"
    temp.write_text(json.dumps(trace), encoding="utf-8")
    temp.chmod(0o600)
    temp.rename(cwd / f"{attempt}.json")
    print("Controlled worker ready", flush=True)
    tty.setraw(0)
    os.set_blocking(0, False)
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline and not (cwd / f"{attempt}.quit").exists():
        try:
            typed = os.read(0, 65536)
        except BlockingIOError:
            typed = b""
        if typed:
            with (cwd / f"{attempt}.input").open("ab") as stream:
                stream.write(typed)
        time.sleep(.025)


if sys.argv[1:2] == ["--worker"]:
    controlled_worker(sys.argv[2], sys.argv[3:])
    sys.exit(0)


WFC = Path(sys.argv.pop(1)).resolve()


class PublicHostResume(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="wfc-host-resume-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.work = self.root / "worker space"
        self.work.mkdir()
        self.report_dir = self.root / "reports"
        self.report_dir.mkdir()
        self.state = self.root / "state"
        socket = str(self.root / "server")
        self.env = {
            "PATH": os.environ["PATH"], "HOME": str(self.root), "TERM": "xterm-256color",
            "WFC_STATE": str(self.state), "WFC_TMUX_SOCKET": socket,
        }
        tmux = ["tmux", "-f", "/dev/null", "-S", socket]
        self.addCleanup(subprocess.run, tmux + ["kill-server"], env=self.env,
                        capture_output=True, timeout=5)
        subprocess.run(tmux + ["new-session", "-d", "-s", "fixture", "/bin/cat"],
                       env=self.env, check=True, capture_output=True, timeout=5)
        for name in ("original-task", "next-task"):
            (self.work / name).write_text("Synthetic task.\n", encoding="utf-8")

    def command(self, *args, callback=None):
        return subprocess.run([WFC, *args], cwd=self.work, env={**self.env, **(callback or {})},
                              text=True, capture_output=True, timeout=20)

    def ok(self, *args, callback=None):
        result = self.command(*args, callback=callback)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def refused(self, *args):
        result = self.command(*args)
        self.assertEqual(result.returncode, 1, (result.stdout, result.stderr))
        self.assertFalse(result.stdout)
        reply = json.loads(result.stderr)
        self.assertEqual(reply["error"], "lifecycle_reconcile_required")
        return reply

    def status(self, dispatch):
        return self.ok("status", "--dispatch", dispatch)["dispatches"][0]

    def dispatch(self, host, *, skip=None, launcher=None):
        config = self.root / "launch.json"
        settings = {"CLAUDE_CONFIG_DIR": str(self.root / "profile")} if host == "claude" else {
            "WFC_OPENCODE_ISOLATED_STORE": "1",
        }
        if skip:
            settings["FIXTURE_SKIP_CLAUDE_HOOK"] = skip
        command = [str(launcher)] if launcher else self.worker_command(host)
        config.write_text(json.dumps({"hosts": {host: {
            "command": command,
            "env": settings,
        }}}), encoding="utf-8")
        return self.ok("dispatch", "--host", host, "--backend", "tmux", "--cwd", ".",
                       "--task", "original-task", "--report", self.report_dir / "original.md",
                       "--label", "fixture", "--launch-config", config)

    def worker_command(self, host):
        return [sys.executable, str(Path(__file__).resolve()), "--worker", host]

    def claude_launcher(self):
        launcher = self.root / "claude-fixture"
        command = shlex.join(self.worker_command("claude"))
        launcher.write_text(f'#!/bin/sh\nexec {command} "$@"\n', encoding="utf-8")
        launcher.chmod(0o700)
        return launcher

    def denied_spawn(self, dispatch, *args):
        failure = self.command("resume", dispatch, *args)
        self.assertEqual(failure.returncode, 1, (failure.stdout, failure.stderr))
        self.assertFalse(failure.stdout)
        error = json.loads(failure.stderr)
        self.assertEqual((error["result"], error["launchResult"], error["category"]),
                         ("definitely-not-sent", "definitely-not-sent", "permission_denied"))
        return error

    def trace(self, attempt):
        path = self.work / f"{attempt}.json"
        deadline = time.monotonic() + 5
        while not path.is_file():
            self.assertLess(time.monotonic(), deadline, "worker trace did not appear")
            time.sleep(.025)
        return json.loads(path.read_text(encoding="utf-8"))

    def terminate_worker(self, dispatch, attempt):
        (self.work / f"{attempt}.quit").touch()
        deadline = time.monotonic() + 6
        while True:
            row = self.status(dispatch)
            if row["backendAlive"] is False and row["worker"]["state"] == "exited":
                return
            self.assertLess(time.monotonic(), deadline, "worker exit was not verified")
            time.sleep(.025)

    def test_claude_adopted_uuid_resumes_next_task_once_and_fences_old_callback(self):
        original = self.dispatch("claude")
        dispatch, attempt = original["dispatch"], original["attempt"]
        first = self.trace(attempt)
        uuid = original["hostSession"]
        self.assertEqual(first["argv"][first["argv"].index("--session-id") + 1], uuid)
        self.assertNotIn("trustedBy", original)
        report = self.report_dir / "original.md"
        report.write_text("Original fixture completed.\n", encoding="utf-8")
        self.ok("done", "--report", report, "--attempt", attempt, callback=first["callback"])
        self.terminate_worker(dispatch, attempt)
        resumed = self.ok("resume", dispatch, "--task", "next-task",
                          "--report", self.report_dir / "next.md")
        new_attempt = resumed["attempt"]
        self.assertNotEqual(new_attempt, attempt)
        self.assertEqual((resumed["session"], resumed["round"], resumed["adoptionResult"]),
                         (uuid, 2, "accepted"))
        worker = self.trace(new_attempt)
        argv = worker["argv"]
        self.assertEqual(argv[argv.index("--resume") + 1], uuid)
        self.assertNotIn("--session-id", argv)
        self.assertEqual(sum("Read and do the task in " in arg for arg in argv), 1)
        self.assertIn(str(self.work / "next-task"), argv[0])
        self.assertEqual(Path(worker["task"]), self.work / "next-task")
        refused = self.command("ask", "old attempt", "--attempt", attempt, callback=first["callback"])
        self.assertEqual(refused.returncode, 1)
        self.assertIn("round mismatch", refused.stderr)
        old_attempt = {**first["callback"], "WFC_ROUND": worker["callback"]["WFC_ROUND"]}
        refused = self.command("ask", "old attempt", "--attempt", attempt, callback=old_attempt)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("attempt is no longer accepted", refused.stderr)
        stale = {**worker["callback"], "WFC_WORKER_CALLBACK": first["callback"]["WFC_WORKER_CALLBACK"]}
        refused = self.command("ask", "old callback", "--attempt", new_attempt, callback=stale)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("invalid_worker_callback", refused.stderr)
        self.ok("ask", "current attempt", "--attempt", new_attempt, callback=worker["callback"])
        row = self.status(dispatch)
        self.assertEqual(row["hostSession"], uuid)
        self.assertEqual(len(row["audit"]), 1)
        audit = row["audit"][0]
        self.assertEqual((audit["fromAttempt"], audit["toAttempt"]), (attempt, new_attempt))
        self.assertEqual(audit["claudeAdoption"]["source"], "startup")
        self.assertEqual(len(audit["claudeAdoption"]["receiptHash"]), 64)
        self.assertEqual(audit["adoptionResult"], "accepted")
        self.assertEqual(len(list(self.work.glob("a-*.json"))), 2)
        self.terminate_worker(dispatch, new_attempt)
        self.assertFalse((self.work / f"{new_attempt}.input").exists())

    def test_claude_uuid_without_original_hook_cannot_resume(self):
        original = self.dispatch("claude", skip="startup")
        dispatch, attempt = original["dispatch"], original["attempt"]
        self.trace(attempt)
        self.terminate_worker(dispatch, attempt)
        denial = self.refused("resume", dispatch)
        self.assertEqual(denial["attempt"], attempt)
        self.assertIn("Claude session adoption not verified", denial["diagnostic"])
        self.assertEqual(self.status(dispatch)["attempt"], attempt)
        self.assertFalse(self.status(dispatch).get("audit"))
        self.assertEqual(len(list(self.work.glob("a-*.json"))), 1)

    def test_claude_retries_definite_spawn_failures_with_the_original_uuid(self):
        launcher = self.claude_launcher()
        original = self.dispatch("claude", launcher=launcher)
        dispatch, attempt, uuid = original["dispatch"], original["attempt"], original["hostSession"]
        first = self.trace(attempt)
        self.terminate_worker(dispatch, attempt)

        launcher.chmod(0o600)
        attempts = [attempt]
        for _ in range(2):
            error = self.denied_spawn(dispatch)
            self.assertEqual((error["session"], error["round"]), (uuid, 1))
            self.assertNotIn(error["attempt"], attempts)
            attempts.append(error["attempt"])
            self.assertEqual(list(self.work.glob("a-*.json")), [self.work / f"{attempt}.json"])
        self.assertEqual((self.work / "claude-starts").read_text().splitlines(), [attempt])

        launcher.chmod(0o700)
        assignment = self.state / "dispatches" / f"{dispatch}.json"
        saved_assignment = assignment.read_bytes()
        inconsistent = json.loads(saved_assignment)
        inconsistent["audit"][0]["inputResult"] = "uncertain"
        assignment.write_text(json.dumps(inconsistent), encoding="utf-8")
        before = assignment.read_bytes()
        denied = self.refused("resume", dispatch)
        self.assertIn("non-start audit does not verify", denied["diagnostic"])
        self.assertEqual(assignment.read_bytes(), before)
        assignment.write_bytes(saved_assignment)

        receipt = self.state / "claude-evidence" / f"{dispatch}.{attempt}" / "adopted.json"
        saved = receipt.rename(receipt.with_suffix(".saved"))
        denied = self.refused("resume", dispatch)
        self.assertIn("Claude session adoption not verified", denied["diagnostic"])
        unchanged = self.status(dispatch)
        self.assertEqual(unchanged["attempt"], attempts[-1])
        self.assertEqual(len(unchanged["audit"]), 2)
        self.assertEqual(list(self.work.glob("a-*.json")), [self.work / f"{attempt}.json"])
        saved.rename(receipt)

        scope = self.status(dispatch)["scope"]
        owner = self.ok("acquire", "--scope", scope)
        adjacent = self.ok("acquire", "--scope", "adjacent-fixture")
        ledger = self.state / "messages.json"
        before = (assignment.read_bytes(), ledger.read_bytes())
        wrong = self.command("resume", dispatch, "--owner-token", adjacent["token"])
        self.assertEqual(wrong.returncode, 1, (wrong.stdout, wrong.stderr))
        self.assertFalse(wrong.stdout)
        self.assertEqual(json.loads(wrong.stderr)["error"], "stale_controller_owner")
        self.assertEqual((assignment.read_bytes(), ledger.read_bytes()), before)

        resumed = self.ok("resume", dispatch, "--owner-token", owner["token"])
        current_attempt = resumed["attempt"]
        self.assertNotIn(current_attempt, attempts)
        self.assertEqual((resumed["session"], resumed["round"], resumed["adoptionResult"]),
                         (uuid, 1, "accepted"))
        worker = self.trace(current_attempt)
        argv = worker["argv"]
        self.assertEqual(argv[argv.index("--resume") + 1], uuid)
        self.assertEqual(argv.count("--resume"), 1)
        self.assertNotIn("--session-id", argv)
        self.assertEqual(sum("Read and do the task in " in arg for arg in argv), 1)
        self.assertEqual(worker["task"], first["task"])
        self.assertNotEqual(worker["callback"]["WFC_WORKER_CALLBACK"],
                            first["callback"]["WFC_WORKER_CALLBACK"])
        old = self.command("ask", "old attempt", "--attempt", attempt, callback=first["callback"])
        self.assertEqual(old.returncode, 1)
        self.assertIn("attempt is no longer accepted", old.stderr)
        stale = {**worker["callback"], "WFC_WORKER_CALLBACK": first["callback"]["WFC_WORKER_CALLBACK"]}
        refused = self.command("ask", "old callback", "--attempt", current_attempt, callback=stale)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("invalid_worker_callback", refused.stderr)
        self.ok("ask", "current attempt", "--attempt", current_attempt, callback=worker["callback"])

        audit = self.status(dispatch)["audit"]
        self.assertEqual([(event["fromAttempt"], event["toAttempt"]) for event in audit],
                         list(zip(attempts, [*attempts[1:], current_attempt])))
        self.assertEqual([event["claudeAdoption"]["attempt"] for event in audit], [attempt] * 3)
        self.assertEqual(len({event["claudeAdoption"]["receiptHash"] for event in audit}), 1)
        self.terminate_worker(dispatch, current_attempt)
        self.assertCountEqual(self.work.glob("a-*.json"),
                              [self.work / f"{job}.json" for job in (attempt, current_attempt)])
        self.assertEqual((self.work / "claude-starts").read_text().splitlines(), [attempt, current_attempt])
        self.assertFalse((self.work / f"{current_attempt}.input").exists())

    def test_claude_retries_unstarted_next_task_from_last_adopted_attempt(self):
        launcher = self.claude_launcher()
        original = self.dispatch("claude", launcher=launcher)
        dispatch, first_attempt, uuid = original["dispatch"], original["attempt"], original["hostSession"]
        self.trace(first_attempt)
        self.terminate_worker(dispatch, first_attempt)
        adopted = self.ok("resume", dispatch)
        self.assertEqual((adopted["session"], adopted["adoptionResult"]), (uuid, "accepted"))
        adopted_attempt = adopted["attempt"]
        worker = self.trace(adopted_attempt)
        report = self.report_dir / "original.md"
        report.write_text("Completed before opening the next task.\n", encoding="utf-8")
        self.ok("done", "--report", report, "--attempt", adopted_attempt, callback=worker["callback"])
        self.terminate_worker(dispatch, adopted_attempt)

        launcher.chmod(0o600)
        next_report = self.report_dir / "next.md"
        failure = self.denied_spawn(dispatch, "--task", "next-task", "--report", next_report)
        self.assertEqual((failure["session"], failure["round"]), (uuid, 2))
        launcher.chmod(0o700)
        retried = self.ok("resume", dispatch)
        self.assertEqual((retried["session"], retried["round"], retried["adoptionResult"]),
                         (uuid, 2, "accepted"))
        attempt = retried["attempt"]
        self.assertNotIn(attempt, (first_attempt, adopted_attempt, failure["attempt"]))
        self.assertEqual(Path(self.trace(attempt)["task"]), self.work / "next-task")
        row = self.status(dispatch)
        self.assertEqual(Path(row["report"]), next_report)
        self.assertEqual(row["audit"][-1]["fromAttempt"], failure["attempt"])
        self.assertEqual(row["audit"][-1]["claudeAdoption"]["attempt"], adopted_attempt)
        self.assertEqual(row["audit"][-1]["claudeAdoption"]["source"], "resume")
        self.assertCountEqual(self.work.glob("a-*.json"),
                              [self.work / f"{job}.json" for job in (first_attempt, adopted_attempt, attempt)])
        self.terminate_worker(dispatch, attempt)
        self.assertEqual((self.work / "claude-starts").read_text().splitlines(),
                         [first_attempt, adopted_attempt, attempt])

    def test_claude_missing_resume_hook_keeps_one_unresolved_attempt(self):
        original = self.dispatch("claude", skip="resume")
        dispatch, attempt = original["dispatch"], original["attempt"]
        self.trace(attempt)
        self.terminate_worker(dispatch, attempt)
        with subprocess.Popen(
            [WFC, "resume", dispatch], cwd=self.work, env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ) as pending:
            deadline = time.monotonic() + 8
            while (new_attempt := self.status(dispatch)["attempt"]) == attempt:
                self.assertLess(time.monotonic(), deadline, "new attempt was not published")
                time.sleep(.025)
            worker = self.trace(new_attempt)
            report = self.report_dir / "original.md"
            report.write_text("Completion without confirmed adoption.\n", encoding="utf-8")
            self.assertEqual(self.status(dispatch)["audit"][0]["result"], "pending")
            for operation in (("ask", "unconfirmed attempt"), ("done", "--report", report)):
                callback = self.command(*operation, "--attempt", new_attempt, callback=worker["callback"])
                self.assertEqual(callback.returncode, 1)
                self.assertIn("Claude resume adoption is unconfirmed", callback.stderr)
            self.assertEqual(self.status(dispatch)["audit"][0]["result"], "pending")
            stdout, stderr = pending.communicate(timeout=20)
            self.assertEqual(pending.returncode, 1, (stdout, stderr))
            self.assertFalse(stdout)
            denial = json.loads(stderr)
        self.assertEqual(denial["error"], "lifecycle_reconcile_required")
        self.assertNotEqual(denial["attempt"], attempt)
        self.assertEqual((denial["category"], denial["launchResult"], denial["adoptionResult"]),
                         ("host_adoption_unconfirmed", "accepted", "uncertain"))
        self.trace(denial["attempt"])
        self.terminate_worker(dispatch, denial["attempt"])
        again = self.refused("resume", dispatch)
        self.assertEqual(again["attempt"], denial["attempt"])
        row = self.status(dispatch)
        self.assertEqual(len(row["audit"]), 1)
        self.assertEqual(row["audit"][0]["result"], "uncertain")
        self.assertFalse(row.get("messages"))
        self.assertEqual(len(list(self.work.glob("a-*.json"))), 2)

    def test_opencode_opening_database_cannot_authorize_public_resume(self):
        original = self.dispatch("opencode")
        dispatch, attempt = original["dispatch"], original["attempt"]
        trace = self.trace(attempt)
        self.assertEqual(original["opencodeOpening"]["database"], trace["store"])
        store = Path(trace["store"])
        self.assertTrue(store.is_relative_to(self.state))
        self.assertEqual(store.parent.stat().st_mode & 0o777, 0o700)
        with sqlite3.connect(f"{store.as_uri()}?mode=ro", uri=True) as db:
            self.assertEqual(db.execute("SELECT id FROM session").fetchall(), [(OPENCODE_ID,)])
        self.terminate_worker(dispatch, attempt)
        denial = self.refused("resume", dispatch)
        self.assertIn("authenticated adoption and terminal readiness", denial["diagnostic"])
        current = self.status(dispatch)
        self.assertEqual(current["attempt"], attempt)
        self.assertIsNone(current.get("hostSession"))
        self.assertFalse(current.get("audit"))
        self.assertEqual(len(list(self.work.glob("a-*.json"))), 1)


if __name__ == "__main__":
    unittest.main()
