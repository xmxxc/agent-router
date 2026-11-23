"""Linux relay contracts through a single CLI, with a private Codex queue."""
import fcntl
import hashlib
import json
import os
import select
import signal
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
Path('pids.tmp').write_text(json.dumps([os.getpid(), helper.pid, host.pid]))
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

        for outcome in ("success", "timeout", "relay-killed"):
            with self.subTest(outcome=outcome):
                self.publish("ours")
                proc = self.foreground(1.5 if outcome == "timeout" else 4)
                pids_path = self.root / "pids"
                self.until(pids_path.exists)
                queue, helper, host = json.loads(pids_path.read_text())
                fds = []
                for pid in (queue, helper, host):
                    fd = os.pidfd_open(pid)
                    self.addCleanup(cleanup, fd)
                    fds.append(fd)
                self.assertEqual(os.getpgid(helper), queue)
                self.assertEqual(os.getsid(host), host)
                if outcome == "relay-killed":
                    proc.kill()
                elif outcome == "success":
                    (self.root / "release").touch()
                out = proc.communicate(timeout=6)
                self.assertEqual(proc.returncode, -signal.SIGKILL if outcome == "relay-killed" else 0, out)
                self.assertTrue(exited(fds[0], 1000))
                self.assertTrue(exited(fds[1], 1000))
                self.assertFalse(exited(fds[2], 0))
                self.assertEqual(self.data("status")["waiters"], [])
                pids_path.unlink()
                (self.root / "release").unlink(missing_ok=True)

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
