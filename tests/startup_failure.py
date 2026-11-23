"""Public Linux startup failure reports, delivered through the existing inbox."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
import uuid

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

    def deploy(self, scope, argv, *, host=None, runtime=None, backend=None, code=0):
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
            if backend == "herdr":
                args += ["--backend", "herdr", "--herdr-endpoint", str(self.cwd / "unused-herdr.sock")]
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

    def start_tmux(self):
        subprocess.run([*self.tmux, "new-session", "-d", "-s", "sentinel", "/bin/cat"],
                       env=self.env, check=True, timeout=10)
        self.addCleanup(subprocess.run, [*self.tmux, "kill-server"], env=self.env,
                        capture_output=True, timeout=10)

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

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_hosted_failures(self):
        self.start_tmux()
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

    def hold_tmux_create(self, gate):
        real_tmux = self.tmux[0]
        shim_dir = self.cwd / "bin"
        shim_dir.mkdir(exist_ok=True)
        shim = shim_dir / "tmux"
        shim.write_text(f"#!{sys.executable}\n" + textwrap.dedent(f"""\
            import os
            from pathlib import Path
            import subprocess
            import sys
            import time

            argv = [{real_tmux!r}, *sys.argv[1:]]
            if "new-session" not in argv:
                os.execv(argv[0], argv)
            result = subprocess.run(argv)
            if result.returncode:
                sys.exit(result.returncode)
            # Hold the genuine tmux response until the real child reaches
            # its exit, so create cannot yet confirm the launch.
            gate = Path({str(gate)!r})
            deadline = time.monotonic() + 3
            while not gate.exists() and time.monotonic() < deadline:
                time.sleep(.005)
            sys.exit(0 if gate.exists() else 1)
        """))
        shim.chmod(0o700)
        return shim_dir

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_tmux_exit_follows_the_create_confirmation(self):
        self.start_tmux()
        gate = self.cwd / "pre-confirmation-exit"
        shim_dir = self.hold_tmux_create(gate)
        probe = textwrap.dedent("""\
            import json
            import os
            from pathlib import Path
            import signal
            import subprocess
            import sys
            import time

            action = sys.argv[1]
            pid = os.getpid()
            boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
            ticks = int(Path("/proc/self/stat").read_bytes().rsplit(b")", 1)[1].split()[19])
            launches = Path(os.environ["WFC_STATE"]) / "launches"
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                for path in launches.glob("*.json"):
                    receipt = json.loads(path.read_text())
                    if receipt.get("category") == "started" and receipt.get("pid") == pid:
                        ready = time.monotonic()
                        if action == "done":
                            subprocess.run([os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                                            "--attempt", os.environ["WFC_ATTEMPT"]], check=True)
                        time.sleep(.35)
                        elapsed = time.monotonic() - ready
                        Path("pre-confirmation-exit").write_text(json.dumps({"launchId": receipt["launchId"],
                            "pid": pid, "processStart": f"linux:{boot}:{ticks}", "startedToExitMs": int(elapsed * 1000)}))
                        if action == "signal":
                            os.kill(pid, signal.SIGTERM)
                            raise SystemExit(125)
                        raise SystemExit(17 if action == "done" else int(action))
                time.sleep(.005)
            raise SystemExit(125)
        """)
        for case, action, code, signum in (("late-zero", "0", 0, None), ("late-nonzero", "17", 17, None),
                                           ("late-signal", "signal", None, signal.SIGTERM), ("late-done", "done", 17, None)):
            with self.subTest(case=case):
                gate.unlink(missing_ok=True)
                old_path = self.env["PATH"]
                self.env["PATH"] = f"{shim_dir}:{old_path}"
                try:
                    self.deploy(case, [sys.executable, "-c", probe, action], host="codex", code=1)
                finally:
                    self.env["PATH"] = old_path
                evidence = json.loads(gate.read_text())
                self.assertGreaterEqual(evidence["startedToExitMs"], 350)
                if action == "done":
                    done, = self.cli("wait", "--scope", case, "--timeout", "0")["messages"]
                    self.assertEqual(done["type"], "done")
                    row = self.cli("status", "--dispatch", done["dispatch"])["dispatches"][0]
                    self.assertEqual(row["worker"]["exit_code"], 17)
                    self.assertNotIn("startupFailure", row)
                    self.assertFalse(self.cli("wait", "--scope", case, "--timeout", "0", code=3)["messages"])
                    continue
                message = self.wait(case)
                fault = message["fault"]
                self.assertEqual((message["type"], fault["exitCode"], fault["signal"]), ("exited", code, signum))
                for field in ("launchId", "pid", "processStart"):
                    self.assertEqual(fault[field], evidence[field])
                self.check_ack(message)
                print(json.dumps({"case": case, "messageId": message["messageId"],
                                  "attempt": message["attempt"], "exitCode": code, "signal": signum, **evidence}), flush=True)

        for code in (0, 17):
            with self.subTest(accepted=code):
                scope = f"accepted-{code}"
                release = self.cwd / f"release-{code}"
                accepted = self.deploy(scope, ["/bin/sh", "-c",
                    f'for i in $(seq 200); do test ! -e {release.name} || exit {code}; sleep .01; done; exit 125'],
                    host="codex")
                release.touch()
                dispatch = accepted["dispatch"]
                self.until(lambda: self.cli("status", "--dispatch", dispatch)["dispatches"][0]["worker"]["state"] == "exited",
                           "wait for accepted worker to exit")
                row = self.cli("status", "--dispatch", dispatch)["dispatches"][0]
                self.assertEqual(row["worker"]["exit_code"], code)
                self.assertNotIn("startupFailure", row)
                self.assertFalse(self.cli("wait", "--scope", scope, "--timeout", "0", code=3)["messages"])

        cancelled = self.deploy("accepted-close", ["/bin/sh", "-c", "exec /bin/sleep 10"], host="codex")
        closed = self.cli("close", cancelled["dispatch"])
        self.assertEqual((closed["attempt"], closed["closeEffect"]), (cancelled["attempt"], "session_closed"))
        self.assertFalse(self.cli("wait", "--scope", "accepted-close", "--timeout", "0", code=3)["messages"])

    @contextmanager
    def staged_startup_exit(self, scope, *, done=False):
        ready = self.cwd / f"{scope}-ready.json"
        release = self.cwd / f"{scope}-release"
        gate = self.cwd / f"{scope}-terminal"
        shim_dir = self.hold_tmux_create(gate)
        script = textwrap.dedent("""\
            import json
            import os
            from pathlib import Path
            import subprocess
            import sys
            import time

            ready, release, terminal = map(Path, sys.argv[1:4])
            if sys.argv[4] == "done":
                subprocess.run([os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                                "--attempt", os.environ["WFC_ATTEMPT"]], check=True)
            temporary = ready.with_suffix(".tmp")
            temporary.write_text(json.dumps({"dispatch": os.environ["WFC_DISPATCH"],
                                            "attempt": os.environ["WFC_ATTEMPT"], "pid": os.getpid()}))
            temporary.rename(ready)
            deadline = time.monotonic() + 2
            while not release.exists() and time.monotonic() < deadline:
                time.sleep(.002)
            if not release.exists():
                raise SystemExit(125)
            terminal.touch()
            raise SystemExit(17)
        """)
        config = self.cwd / f"{scope}.json"
        command = [sys.executable, "-c", script, str(ready), str(release), str(gate), "done" if done else "exit"]
        config.write_text(json.dumps({"hosts": {"codex": {"command": command}}}))
        args = [BINARY, "dispatch", "--task", "task", "--cwd", ".", "--report", "report", "--scope", scope,
                "--role", "startup-check", "--host", "codex", "--launch-config", str(config)]
        env = {**self.env, "PATH": f"{shim_dir}:{self.env['PATH']}"}
        with subprocess.Popen(args, cwd=self.cwd, env=env, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True) as dispatch:
            try:
                self.until(ready.is_file, "await the real worker before releasing exit")
                yield dispatch, release, json.loads(ready.read_text())
            finally:
                release.touch()
                if dispatch.poll() is None:
                    dispatch.kill()
                dispatch.communicate(timeout=7)

    def stop_after_startup_decision(self, scope, *, done=False):
        with self.staged_startup_exit(scope, done=done) as (dispatch, release, child):
            with (self.state / "messages.lock").open("a+b") as ledger:
                fcntl.flock(ledger, fcntl.LOCK_EX | fcntl.LOCK_NB)
                release.touch()
                final = self.state / "dispatches" / f"{child['dispatch']}.json"
                self.until(final.is_file, "await durable create decision while notification is blocked")
                row = json.loads(final.read_text())
                self.assertEqual((row["create"]["result"], row["create"]["category"], row["create"]["exitCode"]),
                                 ("uncertain", "process_exited", 17))
                self.assertEqual((row["scope"], row["round"], row["attempt"]), (scope, 1, child["attempt"]))
                self.assertIsNone(dispatch.poll())
                dispatch.kill()
                dispatch.communicate(timeout=7)
                self.assertEqual(dispatch.returncode, -signal.SIGKILL)
        return child, row

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_startup_notice_recovers_after_dispatch_interruption(self):
        self.start_tmux()
        scope = "interrupted-publish"
        child, row = self.stop_after_startup_decision(scope)
        ledger = self.state / "messages.json"
        self.assertFalse(ledger.exists())
        self.assertFalse(self.cli("wait", "--dispatch", child["dispatch"], "--scope", "wrong-scope", "--timeout", "0", code=3)["messages"])
        self.assertFalse(ledger.exists())
        args = [BINARY, "wait", "--scope", scope, "--timeout", "1.5"]
        with ThreadPoolExecutor(max_workers=2) as waiters:
            results = list(waiters.map(lambda _: subprocess.run(args, cwd=self.cwd, env=self.env,
                                                                capture_output=True, text=True, timeout=6), range(2)))
        self.assertEqual(sorted(result.returncode for result in results), [0, 3], results)
        remaining = [m for result in results for m in json.loads(result.stdout)["messages"]]
        message, = remaining
        self.assertEqual((message["dispatch"], message["scope"], message["attempt"], message["round"]),
                         (child["dispatch"], scope, child["attempt"], 1))
        self.assertEqual((message["fault"]["exitCode"], message["fault"]["pid"], message["fault"]["launchId"]),
                         (17, child["pid"], Path(row["create"]["receipt"]).stem))
        self.check_ack(message)
        self.assertEqual(len(json.loads(ledger.read_text())), 1)
        print(json.dumps({"case": scope, "dispatch": child["dispatch"], "attempt": child["attempt"],
                          "messageId": message["messageId"], "waitExitCodes": [0, 3], "ackVerified": True}), flush=True)

        done_scope = "interrupted-with-done"
        completed, _ = self.stop_after_startup_decision(done_scope, done=True)
        done, = self.cli("wait", "--scope", done_scope, "--timeout", "0")["messages"]
        self.assertEqual((done["type"], done["attempt"]), ("done", completed["attempt"]))
        self.assertFalse(self.cli("wait", "--scope", done_scope, "--timeout", "0", code=3)["messages"])
        row = self.cli("status", "--dispatch", completed["dispatch"])["dispatches"][0]
        self.assertEqual([m["messageId"] for m in row["messages"]], [done["messageId"]])

    def codex_capture(self):
        thread = str(uuid.uuid4())
        sessions = self.cwd / ".codex/sessions/2026/01/01"
        sessions.mkdir(parents=True)
        (sessions / f"rollout-time-{thread}.jsonl").write_text(json.dumps({
            "type": "session_meta", "payload": {"id": thread, "source": "cli"},
        }) + "\n")
        queue = self.cwd / "bin/codex"
        queue.parent.mkdir(exist_ok=True)
        queue.write_text(f"#!{sys.executable}\n" + textwrap.dedent("""\
            import json
            import sys
            with open("queue-calls", "a") as log:
                log.write(json.dumps(sys.argv[1:]) + "\\n")
        """))
        queue.chmod(0o700)
        return self.cwd / "queue-calls", {"CODEX_THREAD_ID": thread, "PATH": f"{queue.parent}:{self.env['PATH']}"}

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_startup_notice_recovers_after_ledger_write_failure(self):
        self.start_tmux()
        scope = "failed-notice-write"
        with self.staged_startup_exit(scope) as (dispatch, release, child):
            blocker = self.state / "messages.pending"
            blocker.mkdir()
            release.touch()
            out, error = dispatch.communicate(timeout=7)
            self.assertEqual((dispatch.returncode, out), (1, ""), error)
            self.assertIn("could not publish confirmed startup failure", error)
            self.assertFalse((self.state / "messages.json").exists())
            blocker.rmdir()
        ledger = self.state / "messages.json"
        queue, extra = self.codex_capture()
        self.cli("relay", "--scope", scope, "--codex", "--for", "1.5", extra=extra)
        call, = [json.loads(line) for line in queue.read_text().splitlines()]
        prefix = "[router] Messages for the controller: "
        self.assertTrue(call[4].startswith(prefix))
        message, = json.loads(call[4][len(prefix):])["messages"]
        self.assertEqual((message["scope"], message["attempt"], message["fault"]["exitCode"]), (scope, child["attempt"], 17))
        ack = self.cli("ack", message["messageId"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))
        self.assertTrue(self.cli("ack", message["messageId"])["duplicate"])
        self.assertFalse(self.cli("wait", "--scope", scope, "--timeout", "0", code=3)["messages"])
        self.assertEqual(len(json.loads(ledger.read_text())), 1)
        print(json.dumps({"case": scope, "dispatch": child["dispatch"], "attempt": child["attempt"],
                          "messageId": message["messageId"], "queueCalls": 1, "ackVerified": True}), flush=True)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_startup_recovery_ignores_conflicting_reservations_and_keeps_neighbors(self):
        self.start_tmux()
        scope, other = "recovery-generations", "other-generation"
        child, _ = self.stop_after_startup_decision(scope)
        dispatch = child["dispatch"]
        pending = self.state / "dispatches" / f"{dispatch}.pending"
        exact = pending.read_bytes()
        ledger = self.state / "messages.json"
        queue, extra = self.codex_capture()
        ready = []
        for name in (scope, other):
            worker = self.deploy(name, ["/bin/true"])
            ready.append(self.cli("done", "--report", "report", "--attempt", worker["attempt"],
                                  extra={"WFC_DISPATCH": worker["dispatch"]})["messageId"])

        conflict = json.loads(exact)
        conflict["attempt"] = "a-000000000001"
        pending.write_text(json.dumps(conflict))
        wait = ("wait", "--dispatch", dispatch, "--scope", scope, "--timeout", "0")
        self.assertFalse(self.cli(*wait, code=3)["messages"])
        for name in (scope, other):
            self.cli("relay", "--scope", name, "--codex", "--for", ".8", extra=extra)
        prefix = "[router] Messages for the controller: "
        sent = [json.loads(json.loads(line)[4][len(prefix):])["messages"] for line in queue.read_text().splitlines()]
        self.assertEqual([[message["messageId"] for message in batch] for batch in sent],
                         [[ready[0]], [ready[1]]])
        for message_id in ready:
            self.assertEqual(self.cli("ack", message_id)["state"], "processed")

        pending.write_bytes(b'{"dispatch":')
        self.assertFalse(self.cli(*wait, code=3)["messages"])
        self.cli("relay", "--scope", scope, "--codex", "--for", ".3", extra=extra)
        self.assertEqual(len(queue.read_text().splitlines()), 2)
        pending.unlink()
        os.mkfifo(pending, 0o600)
        self.assertFalse(self.cli(*wait, code=3)["messages"])
        self.assertEqual(len(json.loads(ledger.read_text())), 2)

        pending.unlink()
        pending.write_bytes(exact)
        notice, = self.cli(*wait)["messages"]
        self.assertEqual((notice["dispatch"], notice["attempt"], notice["fault"]["exitCode"]),
                         (dispatch, child["attempt"], 17))
        self.check_ack(notice)
        self.assertFalse(self.cli(*wait, code=3)["messages"])
        self.assertEqual(len(json.loads(ledger.read_text())), 3)
        print(json.dumps({"case": scope, "dispatch": dispatch, "attempt": child["attempt"],
                          "messageId": notice["messageId"], "neighborAcks": ready,
                          "conflictOrDamagePublished": False, "ackVerified": True}), flush=True)

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
        for backend in ("tmux", "herdr"):
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

                    if backend == "tmux" and script == missing:
                        historical = json.loads(raw)
                        historical["create"].pop("scope")
                        target.write_text(json.dumps(historical))
                        with self.read_only_history(history):
                            self.assertEqual(self.cli("close", dispatch, extra=extra), first)
                        for key, value in (("round", 2), ("attempt", "a-0123456789ab"),
                                           ("scope", "another-scope"), ("result", "uncertain")):
                            changed = json.loads(raw)
                            changed["create"][key] = value
                            target.write_text(json.dumps(changed))
                            with self.read_only_history(history):
                                self.cli("close", dispatch, code=1, extra=extra)
                    print(json.dumps({"case": "script-preflight-readonly", "backend": backend,
                                      "category": category, "scope": scope, "close": first}), flush=True)

if __name__ == "__main__":
    unittest.main()
