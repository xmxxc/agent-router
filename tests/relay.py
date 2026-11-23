"""Linux relay contracts through a single CLI, with a private Codex queue."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

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
    print('x' * 70000, flush=True)
    print('Error: no rollout found for thread ' + sys.argv[3], file=sys.stderr)
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
