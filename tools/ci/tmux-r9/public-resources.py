"""Public R6 resource and worker outcomes, with one private tmux server per test."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

candidate, baseline = (str(Path(p).resolve()) for p in sys.argv[1:3])
del sys.argv[1:3]
tmux = shutil.which("tmux")
assert tmux


class PublicResources(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="w42-r6-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.state = self.root / "state"
        self.records = self.state / "dispatches"
        self.socket = self.root / "sock"
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.env = dict(PATH=os.environ["PATH"], HOME=str(self.root), TERM="xterm",
                        WFC_STATE=str(self.state), WFC_TMUX_SOCKET=str(self.socket))
        self.addCleanup(self.tm, "kill-server", check=False)
        self.tm("new-session", "-d", "-s", "sentinel", "/bin/cat")
        (self.root / "task").touch()
        script = self.root / "worker.py"
        script.write_text("""import os, sys, time
from pathlib import Path
identity = os.environ['WFC_DISPATCH']
with Path('started').open('a') as marker:
    print(identity, os.environ['WFC_ATTEMPT'], file=marker)
while not Path(identity + '.exit').exists(): time.sleep(.02)
sys.exit(37)
""")
        (self.root / "config").write_text(json.dumps(
            {"hosts": {"codex": {"command": [sys.executable, str(script)]}}}))
        self.neighbor = self.deploy()["dispatch"]
        started = self.root / "started"
        self.wait(lambda: started.is_file() and any(
            line.split()[:1] == [self.neighbor] for line in started.read_text().splitlines()
        ))

    def cli(self, *args, binary=candidate, extra=None, code=0, body=None):
        result = subprocess.run([binary, *args], cwd=self.root,
                                env={**self.env, **(extra or {})}, text=True, input=body,
                                capture_output=True, timeout=20)
        self.assertEqual(result.returncode, code, (args, result.stdout, result.stderr))
        return json.loads(result.stdout) if code == 0 else result.stderr.strip()

    def tm(self, *args, check=True):
        return subprocess.run([tmux, "-S", str(self.socket), "-f", "/dev/null", *args],
                              env=self.env, cwd=self.root, text=True, capture_output=True,
                              check=check, timeout=10).stdout.strip()

    def private(self, op, d, **fields):
        return self.cli("--tmux-backend", body=json.dumps(dict(op=op, dispatch=d, **fields)))

    def deploy(self, **kw):
        return self.cli("dispatch", "--host", "codex", "--task", "task", "--report", "report",
                        "--scope", "publication", "--cwd", ".", "--launch-config", "config", **kw)

    def status(self, d):
        return self.cli("status", "--dispatch", d)["dispatches"][0]

    def exists(self, d):
        return "wfc-" + d in self.tm("list-sessions", "-F", "#{session_name}").splitlines()

    def assert_neighbor(self):
        self.assertTrue(self.exists(self.neighbor))
        self.assertEqual(self.status(self.neighbor)["worker"]["state"], "running")
        self.assertIn("sentinel", self.tm("list-sessions", "-F", "#{session_name}").splitlines())

    def assert_never_started(self, d):
        self.assertFalse(any(line.split()[0] == d
                             for line in (self.root / "started").read_text().splitlines()))

    def wait(self, check):
        deadline = time.monotonic() + 10
        while not check():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.025)

    def shim(self, script):
        (self.bin_dir / "tmux").write_text(f"#!{sys.executable}\n" + script)
        (self.bin_dir / "tmux").chmod(0o700)
        return {"PATH": str(self.bin_dir) + ":" + self.env["PATH"]}

    def retained(self, producer):
        gate = self.root / "host-gate"
        supervisor = self.root / "wait-host.py"
        supervisor.write_text("""import os, sys, time
from pathlib import Path
gate, *argv = sys.argv[1:]
while not Path(gate).exists(): time.sleep(.02)
os.execv(argv[0], argv)
""")
        env = self.shim(f"""import os, signal, subprocess, sys
argv = sys.argv[1:]
if "new-session" in argv:
    at = argv.index("--") + 1
    argv[at:at] = [{sys.executable!r}, {str(supervisor)!r}, {str(gate)!r}]
    subprocess.run([{tmux!r}, *argv], check=True)
    os.kill(os.getppid(), signal.SIGKILL)
    sys.exit(0)
os.execv({tmux!r}, [{tmux!r}, *argv])
""")
        diagnostic = self.deploy(binary=producer, extra=env, code=1)
        paths = [p for p in self.records.glob("*.json") if p.stem != self.neighbor]
        self.assertEqual(len(paths), 1, diagnostic)
        row = json.loads(paths[0].read_text())
        d = row["dispatch"]
        self.assertEqual(row["create"]["result"], "uncertain", row)
        pane = self.tm("display-message", "-pt", f"=wfc-{d}:", "#{pane_id}")
        self.assertTrue(pane.startswith("%"), pane)
        self.tm("set-option", "-w", "-t", pane, "remain-on-exit", "on")
        return d, row["attempt"], pane, gate

    def open_gate(self, gate, pane):
        gate.touch()
        self.wait(lambda: self.tm("display-message", "-pt", pane, "#{pane_dead}") == "1")

    def uncertain_close(self, d, **kw):
        diagnostic = self.cli("close", d, code=1, **kw)
        self.assertIn("uncertain", diagnostic)
        return diagnostic

    def pending(self, d, a):
        row = self.status(d)
        self.assertEqual(row["worker"], {"state": "not_started"})
        self.assertEqual((row["resourceCleanup"], row["backendResult"]), ("uncertain", "uncertain"))
        self.assertIsNot(row["backendAlive"], False)
        self.assertNotIn("closeEffect", row)
        self.assertEqual(row["closeOperationId"], f"close:{a}")
        self.assertEqual(row["revocation"]["executionId"], a)
        self.assertEqual(row["revocation"]["dispatch"], d)
        self.assertEqual(row["revocation"]["closeOperationId"], f"close:{a}")
        return row["revocation"]

    def test_retained_cleanup_and_same_name_successor(self):
        d, a, pane, gate = self.retained(candidate)
        self.uncertain_close(d)
        fact = self.pending(d, a)
        self.assertEqual(json.loads((self.state / "launches" / f"{fact['launchId']}.json").read_text()),
                         fact)
        self.uncertain_close(d)
        self.assertTrue(self.exists(d))
        self.open_gate(gate, pane)
        self.assert_never_started(d)
        parked = self.root / "parked"
        self.socket.rename(parked)
        try:
            self.uncertain_close(d)
            self.assertEqual(self.pending(d, a), fact)
        finally:
            parked.rename(self.socket)
        self.tm("rename-session", "-t", f"=wfc-{d}:", "renamed-original")
        self.tm("set-environment", "-ut", "=renamed-original:", "WFC_TMUX_LAUNCH")
        self.uncertain_close(d)
        self.assertEqual(self.pending(d, a), fact)
        self.assertEqual(self.tm("display-message", "-pt", pane, "#{session_name}"), "renamed-original")
        self.tm("rename-session", "-t", "=renamed-original:", "wfc-" + d)
        self.tm("set-environment", "-t", f"=wfc-{d}:", "WFC_TMUX_LAUNCH", fact["launchId"])

        first = self.cli("close", d)
        self.assertEqual((first["closeEffect"], first["resourceCleanup"]),
                         ("unstarted_session_closed", "confirmed"))
        self.assertEqual(first["worker"], {"state": "not_started"})
        self.assertEqual(first["closeOperationId"], f"close:{a}")
        self.assertEqual(first["revocation"], fact)
        self.assertFalse(self.exists(d))
        launch = self.private("create", d, executionId="successor", cwd=str(self.root), argv=["/bin/cat"])
        self.assertEqual(launch["result"], "accepted")
        replay = self.cli("close", d)
        self.assertEqual({k: replay[k] for k in first}, first)
        view = self.status(d)
        self.assertEqual((view["closeEffect"], view["resourceCleanup"], view["backendAlive"]),
                         ("unstarted_session_closed", "confirmed", False))
        self.assertEqual(view["worker"], first["worker"])
        self.assertTrue(self.private("inspect", d, executionId="successor")["workerBound"])
        self.assert_neighbor()
        self.assert_never_started(d)
        print(json.dumps(first), flush=True)

    def test_cleanup_confirmation_lost(self):
        d, a, pane, gate = self.retained(candidate)
        self.uncertain_close(d)
        fact = self.pending(d, a)
        self.open_gate(gate, pane)
        env = self.shim(f"""import os, subprocess, sys
args = sys.argv[1:]
if any("kill-session" in part for part in args):
    subprocess.run([{tmux!r}, *args], check=True, stdout=subprocess.DEVNULL)
    sys.exit(1)
os.execv({tmux!r}, [{tmux!r}, *args])
""")
        self.uncertain_close(d, extra=env)
        self.assertFalse(self.exists(d))
        self.uncertain_close(d)
        self.assertEqual(self.pending(d, a), fact)
        self.assert_never_started(d)
        self.assert_neighbor()
        print(json.dumps({"dispatch":d, "resourceCleanup":"uncertain", "revocation":fact}), flush=True)

    def test_real_r5_archive_stays_unconfirmed(self):
        d, a, pane, gate = self.retained(baseline)
        old = self.cli("close", d, binary=baseline)
        self.assertEqual((old["closeEffect"], old["worker"]), ("already_absent", {"state": "not_started"}))
        archive = self.records / f"{d}.{a}.closed"
        original = json.loads(archive.read_bytes())
        self.assertNotIn("resourceCleanup", original)
        self.assertEqual(self.pending(d, a), old["revocation"])
        self.assertTrue(self.exists(d))
        self.open_gate(gate, pane)
        self.uncertain_close(d)
        self.assertEqual(self.pending(d, a), old["revocation"])
        self.assertTrue(self.exists(d))
        stored = json.loads(archive.read_bytes())
        self.assertEqual(stored["revocation"], old["revocation"])
        self.assertNotEqual(stored.get("resourceCleanup"), "confirmed")
        self.assert_neighbor()
        self.assert_never_started(d)
        print(json.dumps({"old":old, "upgraded":self.status(d)}), flush=True)

    def exit_with_remaining_window(self, producer):
        row = self.deploy(binary=producer)
        d, a = row["dispatch"], row["attempt"]
        launch_receipt = Path(row["create"]["receipt"])
        pane = self.tm("display-message", "-pt", f"=wfc-{d}:", "#{pane_id}")
        sibling_pane = self.tm("new-window", "-d", "-P", "-F", "#{pane_id}",
                               "-t", f"=wfc-{d}:", "/bin/cat")
        (self.root / (d + ".exit")).touch()
        self.wait(lambda: json.loads(launch_receipt.read_bytes()).get("exitCode") == 37)
        self.wait(lambda: pane not in self.tm("list-panes", "-a", "-F", "#{pane_id}").splitlines())
        return row, sibling_pane

    def test_r5_private_absence_without_public_archive(self):
        row, sibling_pane = self.exit_with_remaining_window(baseline)
        d, a = row["dispatch"], row["attempt"]
        launch = Path(row["create"]["receipt"])
        marker = launch.with_suffix(".revoked")
        marker.touch()  # R5 treated existence alone as a reason for absence.
        parked = self.root / "parked"
        self.socket.rename(parked)
        try:
            old = self.cli("close", d, binary=baseline)
            self.assertEqual((old["closeEffect"], old["worker"]["exit_code"]), ("already_absent", 37))
            (self.records / f"{d}.{a}.closed").unlink()
        finally:
            parked.rename(self.socket)
            marker.unlink()
        view = self.status(d)
        self.assertEqual(view["worker"], old["worker"])
        self.assertEqual((view["backendResult"], view["resourceCleanup"], view["backendAlive"]),
                         ("uncertain", "uncertain", None))
        self.assertNotIn("closeEffect", view)
        self.uncertain_close(d)
        self.assertFalse((self.records / f"{d}.{a}.closed").exists())
        self.assertIn(sibling_pane, self.tm("list-panes", "-a", "-F", "#{pane_id}").splitlines())
        self.assert_neighbor()
        print(json.dumps({"old":old, "upgraded":view}), flush=True)

    def test_mismatched_marker_and_known_worker_exit(self):
        row, sibling_pane = self.exit_with_remaining_window(candidate)
        d, a = row["dispatch"], row["attempt"]
        launch_receipt = Path(row["create"]["receipt"])
        receipt = self.status(d)["worker"]
        self.assertEqual(receipt, {"state":"exited", "exit_code":37, "signal":None})

        # Old archive has a real worker fact, but its resource claim is stale.
        archive = self.records / f"{d}.{a}.closed"
        previous = dict(dispatch=d, round=1, attempt=a, executionId=a, closeOperationId=f"close:{a}",
                        launchId=launch_receipt.stem, closeEffect="already_absent", worker=receipt)
        archive.write_text(json.dumps(previous))
        marker = launch_receipt.with_suffix(".revoked")
        marker.write_text(json.dumps(dict(category="not_started", dispatch=d,
                                          executionId="a-aaaaaaaaaaaa", closeOperationId="close:a-aaaaaaaaaaaa",
                                          launchId=launch_receipt.stem)))
        parked = self.root / "parked"
        self.socket.rename(parked)
        try:
            offline = self.status(d)
            self.assertEqual(offline["worker"], receipt)
            self.assertEqual(offline["resourceCleanup"], "uncertain")
            self.assertEqual(offline["closeOperationId"], f"close:{a}")
            self.assertIsNone(offline["backendAlive"])
            self.uncertain_close(d)
            self.assertEqual(json.loads(archive.read_bytes()), previous)
        finally:
            parked.rename(self.socket)
        self.assertIn(sibling_pane, self.tm("list-panes", "-a", "-F", "#{pane_id}").splitlines())
        marker.unlink()
        closed = self.cli("close", d)
        self.assertEqual((closed["closeEffect"], closed["resourceCleanup"]), ("worker_exited", "confirmed"))
        self.assertEqual(closed["worker"], receipt)
        self.assertEqual(closed["closeOperationId"], f"close:{a}")
        self.assert_neighbor()
        print(json.dumps(closed), flush=True)


if __name__ == "__main__":
    unittest.main()
