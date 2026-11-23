"""Public Linux startup failure reports, delivered through the existing inbox."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

BINARY = Path(sys.argv.pop(1)).resolve()
SECRET = "private-fixture-token"


class StartupFailure(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="wfc-startup-")
        self.addCleanup(tmp.cleanup)
        self.cwd = Path(tmp.name)
        self.state = self.cwd / "state"
        self.env = {"PATH": os.environ["PATH"], "HOME": tmp.name, "TERM": "xterm",
                    "WFC_STATE": str(self.state), "WFC_TMUX_SOCKET": str(self.cwd / "socket")}
        (self.cwd / "task").write_text("private startup fixture")
        (self.cwd / "report").write_text("done")
        self.tmux = [shutil.which("tmux"), "-S", self.env["WFC_TMUX_SOCKET"], "-f", "/dev/null"]

    def cli(self, *args, code=0, extra=None):
        result = subprocess.run([BINARY, *args], cwd=self.cwd, env={**self.env, **(extra or {})},
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, code, (args, result.stdout, result.stderr))
        return json.loads(result.stdout) if result.stdout else result.stderr.strip()

    def deploy(self, scope, argv, *, host=None, runtime=None, code=0):
        args = ["dispatch", "--task", "task", "--cwd", ".", "--report", "report",
                "--scope", scope, "--role", "startup-check"]
        if host is None:
            args += ["--", *argv]
        else:
            config = self.cwd / "launch.json"
            settings = {"command": argv}
            if runtime is not None:
                settings["runtime"] = runtime
            config.write_text(json.dumps({"hosts": {host: settings}}))
            args += ["--host", host, "--launch-config", str(config)]
        return self.cli(*args, code=code)

    def wait(self, scope):
        empty = self.cli("wait", "--scope", "unrelated", "--timeout", "0", code=3)
        self.assertFalse(empty["messages"])
        message, = self.cli("wait", "--scope", scope, "--timeout", "4")["messages"]
        self.assertEqual(message["kind"], "startup_failure")
        self.assertEqual(message["scope"], scope)
        self.assertEqual(message["round"], 1)
        self.assertRegex(message["dispatch"], r"^d-[a-f0-9]+$")
        self.assertRegex(message["attempt"], r"^a-[a-f0-9]+$")
        self.assertEqual((message["state"], message["statusVersion"]), ("received", 2))
        return message

    def until(self, condition, description):
        deadline = time.monotonic() + 4
        while not condition():
            self.assertLess(time.monotonic(), deadline, description)
            time.sleep(.002)

    def check_ack(self, message):
        public = self.cli("status", "--dispatch", message["dispatch"])["dispatches"][0]
        notice, = public["messages"]
        self.assertEqual(notice["messageId"], message["messageId"])
        self.assertEqual(public["startupFailure"], message["fault"])
        self.assertNotIn(SECRET, json.dumps(notice))
        body = {key: message[key] for key in
                ("attempt", "report", "role", "round", "scope", "kind", "host", "message", "fault")}
        encoded = json.dumps(body, sort_keys=True, ensure_ascii=False)
        ack = self.cli("ack", message["messageId"], "--body", encoded)
        self.assertEqual((ack["state"], ack["statusVersion"], ack["duplicate"]),
                         ("processed", 3, False))
        self.assertTrue(self.cli("ack", message["messageId"], "--body", encoded)["duplicate"])
        self.assertFalse(self.cli("wait", "--scope", message["scope"], "--timeout", "0", code=3)["messages"])
        return public

    def test_controlled_failures_and_completion(self):
        self.deploy("missing", [str(self.cwd / SECRET)], code=1)
        message = self.wait("missing")
        fault = message["fault"]
        self.assertEqual((message["type"], fault["category"]), ("blocked", "executable_missing"))
        self.assertTrue(all(fault[field] is None for field in ("exitCode", "pid", "processStart", "launchId")))
        self.assertEqual(self.check_ack(message)["worker"]["state"], "not_started")

        invalid = self.cwd / (SECRET + "-invalid-format")
        invalid.write_text("invalid executable")
        invalid.chmod(0o700)
        self.deploy("unknown", [str(invalid)], code=1)
        message = self.wait("unknown")
        self.assertEqual((message["type"], message["fault"]["category"], message["message"]),
                         ("unknown", "spawn_failed", "Worker could not start"))
        self.check_ack(message)

        code = self.deploy("exit", ["/bin/sh", "-c", f"printf '%s' {SECRET} >&2; exit 17"])
        message = self.wait("exit")
        self.assertEqual(message["dispatch"], code["dispatch"])
        self.assertEqual(message["attempt"], code["attempt"])
        self.assertEqual((message["type"], message["fault"]["exitCode"]), ("exited", 17))
        self.assertGreater(message["fault"]["pid"], 0)
        self.assertIn("processStart", message["fault"])
        self.check_ack(message)

        self.deploy("signal", ["/bin/sh", "-c", "kill -TERM $$"])
        message = self.wait("signal")
        self.assertIsNone(message["fault"]["exitCode"])
        self.assertEqual(message["fault"]["signal"], 15)
        self.check_ack(message)

        worker = self.deploy("done", ["/bin/sh", "-c",
            f'"{BINARY}" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT"; exit 17'])
        first, = self.cli("wait", "--scope", "done", "--timeout", "4")["messages"]
        self.assertEqual(first["type"], "done")
        self.assertEqual(first["dispatch"], worker["dispatch"])
        end = time.monotonic() + 7
        while True:
            row = self.cli("status", "--dispatch", worker["dispatch"])["dispatches"][0]
            if row["worker"]["state"] != "running":
                break
            self.assertLess(time.monotonic(), end)
            time.sleep(.02)
        self.assertEqual(row["worker"]["exit_code"], 17)
        self.assertNotIn("startupFailure", row)
        self.assertFalse(self.cli("wait", "--scope", "done", "--timeout", "0", code=3)["messages"])

        success = self.deploy("short-success", ["/bin/true"])
        identifier = success["dispatch"]
        self.until(lambda: self.cli("status", "--dispatch", identifier)["dispatches"][0]["worker"]
                   == {"state": "exited", "exit_code": 0, "signal": None}, "wait for clean exit")
        self.assertFalse(self.cli("wait", "--scope", "short-success", "--timeout", "0", code=3)["messages"])

    def test_close_distinguishes_natural_exit_and_cancellation(self):
        worker = textwrap.dedent("""\
            import json
            import os
            from pathlib import Path
            import time

            Path("pids.tmp").write_text(json.dumps([os.getppid(), os.getpid()]))
            Path("pids.tmp").rename("pids.json")
            deadline = time.monotonic() + 5
            while not Path("exit-now").exists() and time.monotonic() < deadline:
                time.sleep(.002)
            raise SystemExit(17 if Path("exit-now").exists() else 125)
        """)
        dispatched = self.deploy("natural-exit", [sys.executable, "-c", worker])
        dispatch, attempt = dispatched["dispatch"], dispatched["attempt"]
        pids = self.cwd / "pids.json"
        self.until(pids.is_file, "wait for worker to record its owner and PID")
        owner, pid = json.loads(pids.read_text())

        def state_of(process):
            return Path(f"/proc/{process}/stat").read_bytes().rsplit(b")", 1)[1].split()[0]

        owner_file = self.state / "dispatches" / f"{dispatch}.{attempt}.owner"
        owner_fd = os.pidfd_open(owner)
        self.addCleanup(os.close, owner_fd)
        signal.pidfd_send_signal(owner_fd, signal.SIGSTOP)
        closer = None
        try:
            self.until(lambda: state_of(owner) == b"T", "wait for stopped owner")
            (self.cwd / "exit-now").touch()
            self.until(lambda: state_of(pid) == b"Z", "wait for natural exit before close")
            closer = subprocess.Popen([BINARY, "close", dispatch], cwd=self.cwd, env=self.env,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.until(lambda: owner_file.read_bytes()[:1] == b"1", "wait for public close request")
        finally:
            signal.pidfd_send_signal(owner_fd, signal.SIGCONT)
            if closer is not None:
                try:
                    close_out, close_err = closer.communicate(timeout=7)
                except subprocess.TimeoutExpired:
                    closer.kill()
                    closer.communicate()
                    raise
        self.assertEqual(closer.returncode, 0, close_err)
        self.assertEqual(json.loads(close_out)["attempt"], attempt)
        message = self.wait("natural-exit")
        self.assertEqual((message["dispatch"], message["attempt"]), (dispatch, attempt))
        self.assertEqual((message["type"], message["fault"]["exitCode"]), ("exited", 17))
        self.assertEqual(self.check_ack(message)["worker"], {"state": "exited", "exit_code": 17, "signal": None})

        active = self.deploy("active-cancel", ["/bin/sleep", "10"])
        self.assertEqual(self.cli("close", active["dispatch"])["attempt"], active["attempt"])
        closed = self.cli("status", "--dispatch", active["dispatch"])["dispatches"][0]
        self.assertEqual(closed["worker"], {"state": "cancelled", "exit_code": None, "signal": signal.SIGKILL})
        self.assertNotIn("startupFailure", closed)
        self.assertFalse(self.cli("wait", "--scope", "active-cancel", "--timeout", "0", code=3)["messages"])

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_hosted_failures(self):
        subprocess.run([*self.tmux, "new-session", "-d", "-s", "sentinel", "/bin/cat"],
                       env=self.env, check=True, timeout=10)
        self.addCleanup(subprocess.run, [*self.tmux, "kill-server"], env=self.env,
                        capture_output=True, timeout=10)
        invalid = self.deploy("invalid-config", [str(self.cwd / SECRET)], host="codex", runtime=SECRET, code=1)
        self.assertIn("invalid launch config", invalid)
        self.assertNotIn(SECRET, invalid)
        self.assertFalse(self.cli("wait", "--scope", "invalid-config", "--timeout", "0", code=3)["messages"])
        self.assertEqual(self.cli("status")["dispatches"], [])

        script = self.deploy("missing-script", [str(self.cwd / SECRET)], host="codex", runtime=[sys.executable], code=1)
        self.assertIn("missing configured script", script)
        self.assertNotIn(SECRET, script)
        message = self.wait("missing-script")
        self.assertEqual((message["type"], message["fault"]["category"]), ("blocked", "executable_missing"))
        self.assertEqual(message["message"], "Configured worker script was not found")
        self.assertTrue(all(message["fault"][field] is None for field in ("pid", "processStart", "exitCode", "launchId")))
        row = self.check_ack(message)
        self.assertEqual((row["attempt"], row["round"], row["scope"]), (message["attempt"], 1, "missing-script"))
        self.assertEqual(row["worker"]["state"], "not_started")
        closed = self.cli("close", message["dispatch"])
        self.assertEqual((closed["result"], closed["attempt"], closed["worker"]),
                         ("completed", message["attempt"], row["worker"]))

        for host in ("codex", "opencode", "claude"):
            with self.subTest(host=host):
                self.deploy(host, [str(self.cwd / (SECRET + "-" + host))], host=host, code=1)
                message = self.wait(host)
                self.assertEqual(message["host"], host)
                self.assertEqual(message["fault"]["category"], "executable_missing")
                self.assertRegex(message["fault"]["launchId"], r"^[a-f0-9]{32}$")
                self.check_ack(message)
        probe = textwrap.dedent("""\
            import json
            import os
            from pathlib import Path
            import sys
            import time

            deadline = time.monotonic() + .15
            pid = os.getpid()
            boot_id = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
            # Field 22 follows the parenthesized command, with field 3 at index 0.
            stat_fields = Path("/proc/self/stat").read_bytes().rsplit(b")", 1)[1].split()
            start_ticks = int(stat_fields[19])
            Path("host-exits-identity.json").write_text(json.dumps({
                "pid": pid, "processStart": f"linux:{boot_id}:{start_ticks}",
            }))

            # tmux writes the started receipt after observing the child's identity.
            launches = Path(os.environ["WFC_STATE"]) / "launches"
            while time.monotonic() < deadline:
                for path in launches.glob("*.json"):
                    receipt = json.loads(path.read_text())
                    if receipt.get("category") == "started" and receipt.get("pid") == pid:
                        print(sys.argv[1], file=sys.stderr)
                        sys.exit(23)
                time.sleep(.005)
            sys.exit("fixture did not observe its started receipt within 150 ms")
        """)
        self.deploy("host-exits", [sys.executable, "-c", probe, SECRET], host="codex", code=1)
        message = self.wait("host-exits")
        self.assertEqual((message["type"], message["fault"]["exitCode"]), ("exited", 23))
        self.assertGreater(message["fault"]["pid"], 0)
        child = json.loads((self.cwd / "host-exits-identity.json").read_text())
        observed = {key: message["fault"][key] for key in ("pid", "processStart")}
        self.assertEqual(observed, child)
        self.check_ack(message)
        print(json.dumps({
            "case": "host-exits", "dispatch": message["dispatch"], "attempt": message["attempt"],
            "scope": message["scope"], "messageId": message["messageId"],
            "exitCode": message["fault"]["exitCode"], "child": child, "reported": observed,
            "ackVerified": True,
        }), flush=True)

        self.deploy("host-zero", ["/bin/true"], host="codex", code=1)
        zero = self.wait("host-zero")
        self.assertEqual((zero["type"], zero["fault"]["exitCode"]), ("exited", 0))
        self.assertGreater(zero["fault"]["pid"], 0)
        self.assertEqual(self.check_ack(zero)["worker"]["exit_code"], 0)

        self.deploy("host-zero-done", ["/bin/sh", "-c",
                    '"$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT"'],
                    host="codex", code=1)
        done, = self.cli("wait", "--scope", "host-zero-done", "--timeout", "4")["messages"]
        self.assertEqual(done["type"], "done")
        row = self.cli("status", "--dispatch", done["dispatch"])["dispatches"][0]
        self.assertEqual(row["attempt"], done["attempt"])
        self.assertEqual(row["worker"]["exit_code"], 0)
        self.assertNotIn("startupFailure", row)
        self.assertFalse(self.cli("wait", "--scope", "host-zero-done", "--timeout", "0", code=3)["messages"])


if __name__ == "__main__":
    unittest.main()
