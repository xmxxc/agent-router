"""Public startup reports. Bundled preflight needs a user manager or authorized delegation."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import stat
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
                    "WFC_STATE": str(self.state)}
        if "WFC_BUNDLED_CGROUP_ROOT" in os.environ:
            self.env["WFC_BUNDLED_CGROUP_ROOT"] = os.environ["WFC_BUNDLED_CGROUP_ROOT"]
        (self.cwd / "task").write_text("private startup fixture")
        (self.cwd / "report").write_text("done")

    def cli(self, *args, code=0, extra=None):
        result = subprocess.run([BINARY, *args], cwd=self.cwd, env={**self.env, **(extra or {})},
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, code, (args, result.stdout, result.stderr))
        return json.loads(result.stdout) if result.stdout else result.stderr.strip()

    def deploy(self, scope, argv, *, host=None, runtime=None, backend=None, code=0, extra=None):
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
            args += ["--host", host, "--backend", backend or "bundled", "--launch-config", str(config)]
            if backend == "herdr":
                args += ["--herdr-endpoint", str(self.cwd / "unused-herdr.sock")]
        return self.cli(*args, code=code, extra=extra)

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

    @unittest.skipIf(os.geteuid() == 0, "script lookup requires a regular user")
    def test_script_lookup_permission_failure(self):
        hidden = self.cwd / SECRET
        hidden.mkdir()
        script = hidden / "script.py"
        script.touch()
        hidden.chmod(0)
        self.addCleanup(hidden.chmod, 0o700)
        with self.assertRaises(PermissionError):
            script.stat()
        error = self.deploy("script-denied", [str(script)], host="codex", runtime=[sys.executable], code=1)
        self.assertIn("permission denied while resolving configured script", error)
        self.assertNotIn(SECRET, error)
        message = self.wait("script-denied")
        fault = message["fault"]
        self.assertEqual((message["type"], fault["category"]), ("blocked", "permission_denied"))
        self.assertEqual(message["message"], "Permission denied while resolving the configured worker script")
        self.assertTrue(all(fault[key] is None for key in ("pid", "processStart", "exitCode", "signal", "launchId")))
        row = self.check_ack(message)
        self.assertEqual((row["scope"], row["attempt"], row["round"]), ("script-denied", message["attempt"], 1))
        self.assertEqual(row["worker"]["state"], "not_started")

    def test_retired_terminal_receipt_remains_read_only(self):
        dispatch, attempt, launch = "d-12345678", "a-123456789abc", "1" * 32
        records, launches = self.state / "dispatches", self.state / "launches"
        records.mkdir(parents=True)
        launches.mkdir()
        receipt = launches / f"{launch}.json"
        receipt.write_text(json.dumps({"category": "process_exited", "exitCode": 17,
                                       "launchId": launch}))
        assignment = records / f"{dispatch}.json"
        assignment.write_text(json.dumps({
            "dispatch": dispatch, "attempt": attempt, "round": 1, "scope": "archived",
            "executionId": attempt, "role": "fixture", "host": "codex", "backend": "tmux",
            "cwd": str(self.cwd), "report": str(self.cwd / "report"),
            "create": {"result": "uncertain", "category": "process_exited", "exitCode": 17,
                       "dispatch": dispatch, "attempt": attempt, "round": 1,
                       "receipt": str(receipt)},
        }))
        before = {path: path.read_bytes() for path in (assignment, receipt)}
        row, = self.cli("status", "--dispatch", dispatch)["dispatches"]
        self.assertTrue(row["historyOnly"])
        self.assertIsNone(row["backendAlive"])
        self.assertEqual(row["worker"]["exit_code"], 17)
        self.assertIn("tmux assignment history is read-only",
                      self.cli("wait", "--dispatch", dispatch, "--timeout", "0", code=1))
        self.assertEqual({path: path.read_bytes() for path in before}, before)
        self.assertFalse((self.state / "messages.json").exists())
        self.assertFalse((self.state / "messages.lock").exists())

    @contextmanager
    def read_only_history(self, history):
        def snapshot():
            return {path.relative_to(history): (path.stat().st_mode, path.stat().st_mtime_ns,
                    path.read_bytes() if path.is_file() else None) for path in [history, *history.rglob("*")]}

        modes = {path: stat.S_IMODE(path.stat().st_mode) for path in [history, *history.rglob("*")]}
        for path, mode in modes.items():
            path.chmod(mode & ~0o222)
        try:
            self.assertFalse(os.access(history, os.W_OK))
            before = snapshot()
            yield
            self.assertEqual(snapshot(), before)
        finally:
            for path, mode in modes.items():
                path.chmod(mode)

    @unittest.skipIf(os.geteuid() == 0, "read-only close requires a regular user")
    def test_script_preflight_close_replays_from_read_only_history(self):
        restricted = self.cwd / SECRET
        restricted.mkdir()
        denied = restricted / "script.py"
        denied.touch()
        restricted.chmod(0)
        self.addCleanup(restricted.chmod, 0o700)
        with self.assertRaises(PermissionError):
            denied.stat()
        missing = self.cwd / "missing.py"
        for backend in ("bundled", "herdr"):
            for script, category in ((missing, "executable_missing"), (denied, "permission_denied")):
                with self.subTest(backend=backend, category=category):
                    scope = f"readonly-{backend}-{category}"
                    self.deploy(scope, [str(script)], host="codex", runtime=[sys.executable], backend=backend, code=1)
                    message = self.wait(scope)
                    self.assertEqual(message["fault"]["category"], category)
                    dispatch = message["dispatch"]
                    first = self.cli("close", dispatch)
                    self.assertEqual((first["result"], first["dispatch"], first["round"], first["attempt"], first["worker"]["state"]),
                                     ("completed", dispatch, 1, message["attempt"], "not_started"))
                    history = self.cwd / scope
                    (history / "dispatches").mkdir(parents=True)
                    target = history / "dispatches" / f"{dispatch}.json"
                    raw = (self.state / "dispatches" / target.name).read_bytes()
                    target.write_bytes(raw)
                    extra = {"WFC_STATE": str(history)}
                    self.assertFalse((history / "controller-owners.json").exists())
                    with self.read_only_history(history):
                        self.assertEqual(self.cli("close", dispatch, extra=extra), first)
                        self.assertEqual(self.cli("close", dispatch, extra=extra), first)

                    print(json.dumps({"case": "script-preflight-readonly", "backend": backend,
                                      "category": category, "scope": scope, "close": first}), flush=True)

    @unittest.skipIf(os.geteuid() == 0, "owner lock checks require a regular user")
    def test_managed_script_preflight_close_keeps_scope_lock(self):
        scope = "managed-preflight"
        owner = self.cli("acquire", "--scope", scope, "--owner", "fixture")
        self.env["WFC_CONTROLLER_TOKEN"] = owner["token"]
        self.deploy(scope, [str(self.cwd / "missing.py")], host="codex", runtime=[sys.executable], code=1)
        message = self.wait(scope)
        dispatch = message["dispatch"]
        first = self.cli("close", dispatch)
        self.assertEqual((first["attempt"], first["worker"]["state"]), (message["attempt"], "not_started"))
        with self.read_only_history(self.state):
            denied = self.cli("close", dispatch, code=1)
            self.assertIn("Permission denied", denied)
            stale = self.cli("close", dispatch, code=1, extra={"WFC_CONTROLLER_TOKEN": "wrong"})
            detail = json.loads(stale)
            self.assertEqual((detail["error"], detail["scope"], detail["generation"]),
                             ("stale_controller_owner", scope, owner["generation"]))
        self.assertEqual(self.cli("close", dispatch), first)


if __name__ == "__main__":
    unittest.main()
