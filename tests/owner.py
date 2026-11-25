"""Linux controller ownership through the public CLI and a controlled queue."""
import errno
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import unittest

import relay


class Owner(relay.Relay):
    def dispatch(self, scope, *program, **env):
        return self.data("dispatch", "--task", "task", "--report", "task", "--role", "worker",
                         "--scope", scope, "--cwd", ".", "--", *program, **env)

    def done(self, worker, **env):
        return self.data("done", "--report", "task", "--attempt", worker["attempt"],
                         WFC_DISPATCH=worker["dispatch"], **env)

    def record(self, worker):
        return self.data("status", "--dispatch", worker["dispatch"])["dispatches"][0]

    def run_cli(self, *args, **env):
        return subprocess.run([relay.WFC, *args], cwd=self.root,
                              env=dict(self.env, **env), capture_output=True,
                              text=True, timeout=10)

    def error(self, error, *args, **env):
        proc = self.run_cli(*args, **env)
        self.assertEqual((proc.returncode, proc.stdout), (1, ""), proc)
        detail = json.loads(proc.stderr)
        self.assertEqual(detail["error"], error)
        return detail

    def start(self, *args, **env):
        proc = subprocess.Popen([relay.WFC, *args], cwd=self.root,
                                env=dict(self.env, **env), stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)

        def cleanup():
            if proc.poll() is None:
                proc.kill()
            proc.communicate(timeout=5)

        self.addCleanup(cleanup)
        return proc

    @unittest.skipIf(os.geteuid() == 0, "script lookup requires a regular user")
    def test_startup_preflight_rechecks_scope_owner(self):
        hidden = self.root / "private-script-dir"
        hidden.mkdir()
        denied = hidden / "worker.py"
        denied.touch()
        hidden.chmod(0)
        self.addCleanup(hidden.chmod, 0o700)
        with self.assertRaises(PermissionError):
            denied.stat()
        missing = self.root / "missing-script.py"
        dispatch = ["dispatch", "--host", "codex", "--task", "task", "--report", "task", "--cwd", "."]

        def config_for(path):
            return {"hosts": {"codex": {"runtime": [sys.executable], "command": [str(path)]}}}

        for backend, path in (("tmux", missing), ("tmux", denied), ("herdr", missing), ("herdr", denied)):
            with self.subTest(backend=backend, missing=path == missing):
                scope = f"startup-{backend}-{path.name}"
                fifo = self.root / scope
                os.mkfifo(fifo)
                args = [*dispatch, "--scope", scope, "--backend", backend, "--launch-config", str(fifo)]
                if backend == "tmux":
                    token = self.data("owner", "acquire", "--scope", scope)["token"]
                else:
                    token = "fixture-future-owner"
                    args += ["--herdr-endpoint", str(self.root / "unused-herdr.sock")]
                pending = self.start(*args, WFC_CONTROLLER_TOKEN=token)

                def writer():
                    try:
                        return os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        if error.errno != errno.ENXIO:
                            raise
                        return None

                # Opening the writer proves dispatch passed its initial owner
                # check and is blocked awaiting the rest of the launch config.
                with os.fdopen(self.until(writer), "w") as stream:
                    if backend == "tmux":
                        replacement = self.data("takeover", "--scope", scope)["token"]
                    else:
                        replacement = self.data("acquire", "--scope", scope, "--token", token)["token"]
                    json.dump(config_for(path), stream)
                out, error = pending.communicate(timeout=7)
                self.assertEqual((pending.returncode, out), (1, ""), error)
                if backend == "tmux":
                    detail = json.loads(error)
                    self.assertEqual((detail["error"], detail["scope"], detail["generation"]),
                                     ("stale_controller_owner", scope, 2))
                else:
                    self.assertIn("Herdr dispatch requires an unowned scope", error)
                empty = self.cli("wait", "--scope", scope, "--timeout", "0", code=3,
                                 WFC_CONTROLLER_TOKEN=replacement)
                self.assertEqual(json.loads(empty)["messages"], [])
                self.assertEqual(self.data("status")["dispatches"], [])

    def test_controlled_startup_failure_keeps_owner_notice_and_ack(self):
        scope = "startup-current-owner"
        previous = self.data("acquire", "--scope", scope)
        current = self.data("takeover", "--scope", scope)
        missing = self.root / "missing-program"
        args = ("dispatch", "--task", "task", "--report", "task", "--role", "worker",
                "--scope", scope, "--cwd", ".", "--", str(missing))
        detail = self.error("stale_controller_owner", *args, WFC_CONTROLLER_TOKEN=previous["token"])
        self.assertEqual((detail["scope"], detail["generation"]), (scope, current["generation"]))
        env = {"WFC_CONTROLLER_TOKEN": current["token"]}
        empty = self.cli("wait", "--scope", scope, "--timeout", "0", code=3, **env)
        self.assertEqual(json.loads(empty)["messages"], [])
        self.assertEqual(self.data("status")["dispatches"], [])

        failed = self.run_cli(*args, **env)
        self.assertEqual(failed.returncode, 1, failed)
        notice, = self.data("wait", "--scope", scope, "--timeout", "4", **env)["messages"]
        self.assertEqual((notice["scope"], notice["round"], notice["fault"]["category"]),
                         (scope, 1, "executable_missing"))
        ack = self.data("ack", notice["messageId"], **env)
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 3))
        again = self.data("ack", notice["messageId"], **env)
        self.assertTrue(again["duplicate"])
        self.assertEqual(again["messageId"], notice["messageId"])
        row = self.record(notice)
        self.assertEqual((row["worker"]["state"], row["attempt"]), ("not_started", notice["attempt"]))

    def test_lease_aliases_and_race(self):
        commands = [
            self.start(action, "--scope", "ours", "--owner", action, "--token", action)
            for action in ("acquire", "claim")
        ]
        active = []
        for proc in commands:
            out, err = proc.communicate(timeout=5)
            if proc.returncode == 0:
                active.append(json.loads(out))
            else:
                self.assertEqual((proc.returncode, out), (1, ""), err)
                self.assertEqual(json.loads(err)["error"], "scope_owned")
        first, = active
        self.assertEqual((first["scope"], first["action"], first["generation"]),
                         ("ours", "acquire", 1))

        renew = self.data("renew", "--scope", "ours", "--lease", "-1",
                          "--owner-token", first["token"], WFC_CONTROLLER_TOKEN="wrong",
                          WFC_OWNER_TOKEN="wrong", WFC_CONTROLLER_ID="renewed")
        self.assertEqual((renew["owner"], renew["token"], renew["generation"], renew["action"]),
                         ("renewed", first["token"], 1, "renew"))
        stale = self.error("stale_controller_owner", "owner", "renew", "--scope", "ours",
                           WFC_OWNER_TOKEN=first["token"])
        self.assertEqual((stale["owner"], stale["active"], stale["generation"]),
                         (first["owner"], False, 1))
        self.error("owner_token_conflict", "claim", "--scope", "ours", "--token", first["token"])
        second = self.data("owner", "acquire", "--scope", "ours")
        self.assertEqual(second["generation"], 2)
        self.error("owner_token_conflict", "takeover", "--scope", "ours", "--token", second["token"])
        third = self.data("takeover", "--scope", "ours", WFC_CONTROLLER_TOKEN=second["token"])
        self.assertEqual(third["generation"], 3)
        self.assertNotIn(third["token"], (first["token"], second["token"]))
        self.error("owner_token_conflict", "takeover", "--scope", "ours", "--token", first["token"])
        neighbor = self.data("owner", "claim", "--scope", "neighbor")
        self.assertEqual((neighbor["action"], neighbor["generation"]), ("acquire", 1))
        print(json.dumps({"leaseRace": "one winner", "generations": [1, 2, 3],
                          "neighborGeneration": 1}), flush=True)

    def test_takeover_fences_wait_and_ack_but_worker_keeps_its_callback(self):
        legacy = self.dispatch("ours", "/bin/true")
        pending = self.publish("neighbor")
        first = self.data("owner", "acquire", "--scope", "  ours  ", "--owner", "A")
        a = {"WFC_CONTROLLER_TOKEN": first["token"]}
        script = """
import json, os, subprocess, sys, time
from pathlib import Path

secret = os.environ["WFC_WORKER_CALLBACK"]
assert secret and not any(k.upper() in ("WFC_CONTROLLER_TOKEN", "WFC_OWNER_TOKEN") for k in os.environ)
binary = sys.argv[1]
for args in (["status"], ["reclaim"], ["takeover", "--scope", "ours"]):
    result = subprocess.run([binary, *args], text=True, capture_output=True, timeout=5)
    assert result.returncode == 1 and not result.stdout
    assert json.loads(result.stderr)["error"] == "worker_callback_only"
Path("worker-ready").touch()
deadline = time.monotonic() + 12
while not Path("release-worker").exists():
    assert time.monotonic() < deadline
    time.sleep(.025)
args = [binary, "done", "--report", os.environ["WFC_REPORT"], "--attempt", os.environ["WFC_ATTEMPT"]]
receipt = json.loads(subprocess.check_output(args, timeout=5))
retry = json.loads(subprocess.check_output(args, timeout=5))
assert retry["messageId"] == receipt["messageId"] and retry["duplicate"]
"""
        worker = self.dispatch("ours", sys.executable, "-c", script, relay.WFC,
                               WFC_OWNER_TOKEN="lower-priority", **a)
        release = self.root / "release-worker"
        self.addCleanup(release.touch, exist_ok=True)
        self.until(lambda: (self.root / "worker-ready").exists())
        old_receipt = self.done(legacy, **a)
        old_wait = self.data("wait", "--scope", "ours", "--timeout", "0",
                             WFC_OWNER_TOKEN=first["token"])["messages"]
        self.assertEqual([m["messageId"] for m in old_wait], [old_receipt["messageId"]])
        waiting = self.start("wait", "--scope", "ours", "--timeout", "10", **a)
        second = self.data("owner", "takeover", "--scope", "ours", "--owner", "B")
        b = {"WFC_CONTROLLER_TOKEN": second["token"]}
        out, err = waiting.communicate(timeout=5)
        self.assertEqual((waiting.returncode, out), (1, ""), err)
        self.assertEqual(json.loads(err)["error"], "stale_controller_owner")
        self.error("stale_controller_owner", "wait", "--scope", "ours", "--timeout", "0", **a)
        self.error("stale_controller_owner", "ack", old_receipt["messageId"], **a)
        self.error("stale_controller_owner", "ack", old_receipt["messageId"],
                   WFC_CONTROLLER_TOKEN=first["token"], WFC_OWNER_TOKEN=second["token"])
        self.error("controller_owner_required", "wait", "--timeout", "0", **b)
        self.error("controller_owner_required", "wait", "--dispatch", legacy["dispatch"], "--timeout", "0", **b)
        self.error("controller_owner_required", "relay", "--codex", "--detach", **b)
        self.error("stale_controller_owner", "close", worker["dispatch"], **a)
        self.error("stale_controller_owner", "done", "--report", "task",
                   WFC_DISPATCH=legacy["dispatch"], **a)
        self.error("invalid_worker_callback", "done", "--report", "task", "--attempt", worker["attempt"],
                   WFC_DISPATCH=worker["dispatch"], WFC_WORKER_CALLBACK=worker["attempt"], **b)
        release.touch()
        claimed, = self.data("wait", "--scope", "ours", "--timeout", "5", **b)["messages"]
        self.assertEqual((claimed["dispatch"], claimed["attempt"], claimed["statusVersion"]),
                         (worker["dispatch"], worker["attempt"], 2))
        receipt = self.data("ack", claimed["messageId"], "--owner-token", second["token"], **a)
        self.assertEqual((receipt["state"], receipt["statusVersion"]), ("processed", 3))
        self.error("stale_controller_owner", "ack", claimed["messageId"], **a)
        self.error("stale_controller_owner", "resolve", claimed["messageId"], "--actor", "A",
                   "--reason", "controlled", "--evidence", "controlled", "--body-hash", "0" * 64, **a)
        self.assertEqual(self.data("ack", old_receipt["messageId"], **b)["state"], "processed")
        unowned, = self.data("wait", "--scope", "neighbor", "--timeout", "0")["messages"]
        self.assertEqual(unowned["messageId"], pending["messageId"])
        self.assertEqual(self.data("ack", pending["messageId"])["state"], "processed")
        current = self.record(worker)
        self.assertEqual(current["controllerOwner"], {"owner": "B", "generation": 2,
                         "leaseUntil": second["leaseUntil"], "active": True})
        self.assertNotIn("workerCallbackHash", worker)
        self.assertNotIn("workerCallbackHash", current)
        self.assertNotIn(second["token"], json.dumps(current))
        self.until(lambda: self.record(worker)["worker"]["state"] == "exited")
        self.assertEqual(self.record(worker)["worker"]["exit_code"], 0)
        self.data("close", worker["dispatch"], **b)
        print(json.dumps({"callbackReceipt": receipt, "workerAttempt": worker["attempt"],
                          "ownerGeneration": 2, "unownedReceipt": unowned["messageId"]}), flush=True)

    def test_question_requires_current_owner_or_original_worker_callback(self):
        first = self.data("owner", "acquire", "--scope", "ours", "--owner", "A")
        a = {"WFC_CONTROLLER_TOKEN": first["token"]}
        script = """
import json, os, subprocess, sys, time
from pathlib import Path

assert os.environ["WFC_WORKER_CALLBACK"]
assert "WFC_CONTROLLER_TOKEN" not in os.environ and "WFC_OWNER_TOKEN" not in os.environ
Path("ask-ready").touch()
deadline = time.monotonic() + 20
while not Path("ask-go").exists():
    assert time.monotonic() < deadline
    time.sleep(.02)
args = [sys.argv[1], "ask", "Worker question?", "--attempt", os.environ["WFC_ATTEMPT"],
        "--message-id", "worker-question"]
first = json.loads(subprocess.check_output(args, timeout=8))
replay = json.loads(subprocess.check_output(args, timeout=8))
assert replay["duplicate"] and replay["messageId"] == first["messageId"]
Path("ask-result.tmp").write_text(json.dumps(first))
Path("ask-result.tmp").rename("ask-result")
while not Path("ask-exit").exists() and time.monotonic() < deadline:
    time.sleep(.02)
"""
        worker = self.dispatch("ours", sys.executable, "-c", script, relay.WFC, **a)
        self.addCleanup((self.root / "ask-exit").touch, exist_ok=True)
        self.addCleanup((self.root / "ask-go").touch, exist_ok=True)
        self.until(lambda: (self.root / "ask-ready").exists())
        second = self.data("owner", "takeover", "--scope", "ours", "--owner", "B")
        b = {"WFC_CONTROLLER_TOKEN": second["token"]}
        worker_env = {"WFC_DISPATCH": worker["dispatch"]}
        request = ("ask", "Controller question?", "--attempt", worker["attempt"],
                   "--message-id", "controller-question")
        stale = self.error("stale_controller_owner", *request, **worker_env, **a)
        self.assertEqual((stale["scope"], stale["operation"]), ("ours", "ask"))
        invalid = self.error("invalid_worker_callback", *request, **worker_env, **b,
                             WFC_WORKER_CALLBACK=worker["attempt"])
        self.assertEqual(invalid["operation"], "ask")
        self.error("invalid_worker_callback", *request, "--callback-ref", "0" * 64, **worker_env, **b)
        controller = self.data(*request, "--owner-token", second["token"], **worker_env, **a)
        (self.root / "ask-go").touch()
        result = self.root / "ask-result"
        self.until(result.exists)
        question = json.loads(result.read_bytes())
        messages = self.data("wait", "--scope", "ours", "--timeout", "0", **b)["messages"]
        self.assertEqual([(m["messageId"], m["type"], m["attempt"]) for m in messages],
                         [(r["messageId"], "question", worker["attempt"]) for r in (controller, question)])
        self.error("stale_controller_owner", "ask", "Worker question?",
                   "--attempt", worker["attempt"], "--message-id", "worker-question",
                   **worker_env, **a)
        self.error("stale_controller_owner", "ack", question["messageId"], **a)
        self.assertEqual(self.data("ack", question["messageId"], **b)["state"], "processed")
        self.assertNotIn("workerCallbackHash", self.record(worker))
        self.assertEqual(self.data("close", worker["dispatch"], **b)["result"], "completed")

    def test_takeover_waits_for_scoped_delivery_and_unregisters_detached_relay(self):
        first = self.data("owner", "acquire", "--scope", "ours")
        a = {"WFC_CONTROLLER_TOKEN": first["token"]}
        worker = self.dispatch("ours", "/bin/true", **a)
        gate = self.root / "gate"
        gate.touch()
        old = self.data("relay", "--codex", "--scope", "ours", "--detach", "--for", "20",
                        "--owner-token", first["token"], WFC_CONTROLLER_TOKEN="wrong")
        pid = old["pid"]
        pidfd = os.pidfd_open(pid)

        def cleanup():
            try:
                if not select.select([pidfd], [], [], 0)[0]:
                    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                self.assertTrue(select.select([pidfd], [], [], 5)[0])
            finally:
                os.close(pidfd)

        self.addCleanup(cleanup)
        self.addCleanup(gate.unlink, missing_ok=True)
        self.until(lambda: any(w["pid"] == pid for w in self.data("status")["waiters"]))
        self.assertNotIn(first["token"].encode(), Path(f"/proc/{pid}/cmdline").read_bytes())
        receipt = self.done(worker, **a)
        self.until(self.calls)
        self.assertEqual(self.record(worker)["messages"][0]["state"], "unknown")
        contender = self.start("takeover", "--scope", "ours", "--owner", "B")
        # These writes complete while the queue is blocked and the contender is waiting.
        other = self.data("claim", "--scope", "neighbor")
        neighbor = self.dispatch("neighbor", "/bin/true", WFC_OWNER_TOKEN=other["token"])
        progress = self.done(neighbor, WFC_OWNER_TOKEN=other["token"])
        self.assertIsNone(contender.poll())
        self.assertEqual(self.record(worker)["controllerOwner"]["generation"], 1)
        gate.unlink()
        stdout, stderr = contender.communicate(timeout=5)
        self.assertEqual(contender.returncode, 0, stderr)
        b = {"WFC_OWNER_TOKEN": json.loads(stdout)["token"]}
        self.assertTrue(select.select([pidfd], [], [], 5)[0])
        self.assertEqual(self.data("status")["waiters"], [])
        self.assertIn("stale_controller_owner", Path(old["log"]).read_text())
        self.assertEqual(self.data("ack", receipt["messageId"], **b)["statusVersion"], 4)
        later = self.dispatch("ours", "/bin/true", **b)
        next_receipt = self.done(later, **b)
        self.cli("relay", "--codex", "--scope", "ours", "--for", "1", **b)
        batches = [json.loads(call[4][len(relay.PREFIX):])["messages"] for call in self.calls()]
        self.assertEqual([[m["messageId"] for m in batch] for batch in batches],
                         [[receipt["messageId"]], [next_receipt["messageId"]]])
        self.assertEqual(self.data("ack", next_receipt["messageId"], **b)["statusVersion"], 4)
        self.assertEqual(self.record(neighbor)["messages"][0]["messageId"], progress["messageId"])
        self.assertEqual(self.record(neighbor)["messages"][0]["state"], "unsent")
        print(json.dumps({"oldRelayExited": pid, "queuedBatches": [[m["messageId"] for m in batch]
                         for batch in batches], "concurrentNeighborReceipt": progress["messageId"]}), flush=True)

    def test_relay_settles_its_claim_after_lease_expires(self):
        owner = self.data("acquire", "--scope", "ours", "--lease", "3")
        env = {"WFC_OWNER_TOKEN": owner["token"]}
        worker = self.dispatch("ours", "/bin/true", **env)
        receipt = self.done(worker, **env)
        gate = self.root / "gate"
        gate.touch()
        proc = self.start("relay", "--codex", "--scope", "ours", "--for", "10", **env)
        self.addCleanup(gate.unlink, missing_ok=True)
        self.until(self.calls)
        self.until(lambda: not self.record(worker)["controllerOwner"]["active"])
        gate.unlink()
        out, err = proc.communicate(timeout=5)
        self.assertEqual((proc.returncode, out), (1, ""), err)
        self.assertFalse(json.loads(err)["active"])
        message, = self.record(worker)["messages"]
        self.assertEqual((message["messageId"], message["state"], message["statusVersion"]),
                         (receipt["messageId"], "received", 3))
        self.assertEqual(self.data("status")["waiters"], [])
        self.assertEqual(len(self.calls()), 1)
        print(json.dumps({"expiredLeaseSettlement": message["messageId"],
                          "statusVersion": message["statusVersion"]}), flush=True)


if __name__ == "__main__":
    # Inherit the controlled transport and CLI helpers, without rerunning the
    # existing Relay tests in this focused suite.
    names = sys.argv[1:] or sorted(name for name in vars(Owner) if name.startswith("test_"))
    suite = unittest.TestSuite(Owner(name) for name in names)
    sys.exit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
