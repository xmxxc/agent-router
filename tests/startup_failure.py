"""Public Linux startup failure reports, delivered through the existing inbox."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
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

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_tmux_zero_follows_the_create_confirmation(self):
        self.start_tmux()
        real_tmux = self.tmux[0]
        shim_dir = self.cwd / "bin"
        shim_dir.mkdir()
        shim = shim_dir / "tmux"
        gate = self.cwd / "pre-confirmation-exit"
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
        probe = textwrap.dedent("""\
            import json
            import os
            from pathlib import Path
            import time

            launches = Path(os.environ["WFC_STATE"]) / "launches"
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                for path in launches.glob("*.json"):
                    receipt = json.loads(path.read_text())
                    if receipt.get("category") == "started" and receipt.get("pid") == os.getpid():
                        ready = time.monotonic()
                        time.sleep(.35)
                        elapsed = time.monotonic() - ready
                        Path("pre-confirmation-exit").write_text(json.dumps({"launchId": receipt["launchId"],
                            "pid": os.getpid(), "startedToExitMs": int(elapsed * 1000)}))
                        raise SystemExit(0)
                time.sleep(.005)
            raise SystemExit(125)
        """)
        old_path = self.env["PATH"]
        self.env["PATH"] = f"{shim_dir}:{old_path}"
        try:
            self.deploy("late-zero", [sys.executable, "-c", probe], host="codex", code=1)
        finally:
            self.env["PATH"] = old_path
        evidence = json.loads(gate.read_text())
        self.assertGreaterEqual(evidence["startedToExitMs"], 350)
        late = self.wait("late-zero")
        fault = late["fault"]
        self.assertEqual((late["type"], fault["exitCode"]), ("exited", 0))
        self.assertEqual((fault["launchId"], fault["pid"]), (evidence["launchId"], evidence["pid"]))
        self.assertEqual(self.check_ack(late)["worker"]["exit_code"], 0)
        quiet = self.cli("wait", "--scope", "late-zero", "--dispatch", late["dispatch"],
                         "--timeout", ".35", code=3)
        self.assertFalse(quiet["messages"])
        print(json.dumps({"case": "late-zero", "messageId": late["messageId"],
                          "attempt": late["attempt"], "exitCode": fault["exitCode"], **evidence}), flush=True)

        accepted = self.deploy("accepted-zero", ["/bin/sh", "-c",
            'for i in $(seq 200); do test ! -e release-accepted || exit 0; sleep .01; done; exit 125'],
            host="codex")
        (self.cwd / "release-accepted").touch()
        dispatch = accepted["dispatch"]
        self.until(lambda: self.cli("status", "--dispatch", dispatch)["dispatches"][0]["worker"]["state"] == "exited",
                   "wait for accepted worker to exit")
        row = self.cli("status", "--dispatch", dispatch)["dispatches"][0]
        self.assertEqual(row["worker"]["exit_code"], 0)
        self.assertNotIn("startupFailure", row)
        self.assertFalse(self.cli("wait", "--scope", "accepted-zero", "--timeout", "0", code=3)["messages"])
        runtime, = self.cli("wait", "--scope", "accepted-zero", "--dispatch", dispatch,
                            "--timeout", "4")["messages"]
        self.assertEqual((runtime["kind"], runtime["type"], runtime["fault"]["exitCode"]),
                         ("host_runtime", "exited", 0))
        self.assertEqual(runtime["attempt"], accepted["attempt"])
        self.assertGreater(runtime["fault"]["pid"], 0)
        self.assertRegex(runtime["fault"]["processStart"], r"^linux:[0-9a-f-]+:\d+$")
        current = self.cli("status", "--dispatch", dispatch)["dispatches"][0]
        self.assertNotIn("startupFailure", current)
        self.assertEqual([m["messageId"] for m in current["messages"]], [runtime["messageId"]])
        quiet = self.cli("wait", "--scope", "accepted-zero", "--dispatch", dispatch,
                         "--timeout", ".35", code=3)
        self.assertFalse(quiet["messages"])
        print(json.dumps({"case": "accepted-zero", "scope": runtime["scope"],
                          "dispatch": dispatch, "attempt": runtime["attempt"],
                          "messageId": runtime["messageId"], "fault": runtime["fault"]}), flush=True)


    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_host_runtime_observations(self):
        self.start_tmux()
        fixture = textwrap.dedent(r"""
            import json
            import os
            from pathlib import Path
            import subprocess
            import sys
            import time

            command = Path(sys.argv[1])
            boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
            start = Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19]
            command.with_suffix(".identity").write_text(json.dumps({
                "pid": os.getpid(), "processStart": f"linux:{boot}:{start}",
            }))
            previous = None
            frame = 0
            while True:
                view = command.read_text()
                if view == "done":
                    subprocess.run([os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                                    "--attempt", os.environ["WFC_ATTEMPT"]], check=True)
                    break
                if view == "exit":
                    sys.exit(23)
                if view != previous or view in ("refresh", "quota-refresh"):
                    frame += 1
                    text = {"refresh": f"Working: frame {frame}",
                            "quota-refresh": f"Quota exceeded: frame {frame}"}.get(view, view)
                    print("\x1b[2J\x1b[H" + text, flush=True)
                    previous = view
                time.sleep(.05)
            """)

        def write(path, content):
            temp = path.with_suffix(".tmp")
            temp.write_text(content)
            temp.replace(path)

        def launch(host, scope, screen):
            command = self.cwd / scope
            write(command, screen)
            job = self.deploy(scope, [sys.executable, "-c", fixture, str(command)], host=host)
            return command, job

        def observe(job, *, timeout="4", code=0, stall="0", deadline="0", scope=None):
            return self.cli("wait", "--scope", scope or job["scope"], "--dispatch", job["dispatch"],
                            "--timeout", timeout, "--stall-after", stall, code=code,
                            extra={"WFC_MAX_TURN_SECONDS": deadline})["messages"]

        def assert_notice(job, message, kind, category, evidence, state):
            self.assertEqual((message["dispatch"], message["round"], message["attempt"], message["scope"]),
                             (job["dispatch"], 1, job["attempt"], job["scope"]))
            self.assertEqual((message["type"], message["kind"], message["host"]),
                             (kind, "host_runtime", job["host"]))
            self.assertEqual((message["fault"]["category"], message["fault"]["evidence"]),
                             (category, evidence))
            self.assertEqual((message["state"], message["statusVersion"]), ("received", 2))
            status = self.cli("status", "--dispatch", job["dispatch"])["dispatches"][0]
            self.assertEqual(status["hostActivity"]["state"], state)
            self.assertNotIn(status["state"], ("done", "completed"))
            self.assertNotIn(SECRET, json.dumps(status))
            body = {key: message[key] for key in
                    ("attempt", "report", "role", "round", "scope", "kind", "host", "message", "fault")}
            encoded = json.dumps(body, sort_keys=True, ensure_ascii=False)
            ack = self.cli("ack", message["messageId"], "--body", encoded)
            self.assertEqual((ack["state"], ack["statusVersion"], ack["duplicate"]),
                             ("processed", 3, False))
            self.assertTrue(self.cli("ack", message["messageId"], "--body", encoded)["duplicate"])
            print(json.dumps({"case": "runtime-tmux", "dispatch": job["dispatch"],
                              "attempt": job["attempt"], "scope": job["scope"], "host": job["host"],
                              "messageId": message["messageId"], "state": state,
                              "fault": message["fault"], "ackVerified": True}), flush=True)

        for host, screen, kind, category, state in (
            ("codex", "Quota exceeded", "blocked", "quota", "blocked"),
            ("opencode", "Approval required", "blocked", "permission_required", "awaiting_permission"),
            ("claude", "Connection reset", "unknown", "protocol_disconnected", "disconnected"),
        ):
            with self.subTest(host=host):
                command, job = launch(host, host, f"{screen}: {SECRET}")
                self.assertFalse(observe(job, scope="unrelated", timeout=".15", code=3))
                with ThreadPoolExecutor(max_workers=2) as pool:
                    # Different scopes stay independent while two fresh wait
                    # processes for the right scope contend for the same receipt.
                    futures = [pool.submit(subprocess.run,
                        [BINARY, "wait", "--scope", host, "--dispatch", job["dispatch"], "--timeout", "3"],
                        cwd=self.cwd, env=self.env, capture_output=True, text=True, timeout=8)
                        for _ in range(2)]
                    results = [future.result() for future in futures]
                self.assertEqual(sorted(r.returncode for r in results), [0, 3], results)
                message, = [item for r in results for item in json.loads(r.stdout)["messages"]]
                assert_notice(job, message, kind, category, "screen_notice", state)
                write(command, f"{screen}: {SECRET}-updated")
                self.assertFalse(observe(job, timeout="1.25", code=3, stall=".1"))

                write(command, f"A new result is ready: {SECRET}")
                recovery, = observe(job)
                assert_notice(job, recovery, "recovered", "recovered", "changed_activity", "active")
                self.assertFalse(observe(job, timeout=".25", code=3))
                write(command, f"{screen}: {SECRET}")
                again, = observe(job)
                assert_notice(job, again, kind, category, "screen_notice", state)
                self.assertNotEqual(again["messageId"], message["messageId"])

                write(command, "done")
                done, = observe(job)
                self.assertEqual(done["type"], "done")
                self.assertFalse(observe(job, timeout=".15", code=3))
                complete = self.cli("status", "--dispatch", job["dispatch"])["dispatches"][0]
                self.assertEqual(complete["state"], "done")
                self.assertNotIn("hostActivity", complete)

        idle_command, idle = launch("codex", "idle", "Ready")
        stalled, = observe(idle, stall=".5")
        assert_notice(idle, stalled, "stalled", "timeout", "unchanged_screen", "stalled")
        self.assertIsInstance(stalled["fault"]["elapsedSeconds"], int)
        self.assertFalse(observe(idle, stall=".5", timeout="1.25", code=3))
        write(idle_command, f"Step two is ready: {SECRET}")
        recovery, = observe(idle)
        assert_notice(idle, recovery, "recovered", "recovered", "changed_activity", "active")
        stalled_again, = observe(idle, stall=".5")
        assert_notice(idle, stalled_again, "stalled", "timeout", "unchanged_screen", "stalled")
        self.assertNotEqual(stalled["messageId"], stalled_again["messageId"])

        _, refreshing = launch("claude", "refreshing", "refresh")
        deadline, = observe(refreshing, stall=".5", deadline="2")
        assert_notice(refreshing, deadline, "unknown", "timeout", "turn_deadline", "timed_out")
        self.assertGreaterEqual(deadline["fault"]["elapsedSeconds"], 2)
        self.assertFalse(observe(refreshing, stall=".5", deadline="2", timeout="1.25", code=3))

        _, quota_refresh = launch("claude", "quota-refresh", "quota-refresh")
        quota, = observe(quota_refresh, deadline="3")
        assert_notice(quota_refresh, quota, "blocked", "quota", "screen_notice", "blocked")
        deadline, = observe(quota_refresh, deadline="3", timeout="6")
        assert_notice(quota_refresh, deadline, "unknown", "timeout", "turn_deadline", "timed_out")
        self.assertFalse(observe(quota_refresh, deadline="3", timeout="1.25", code=3))

        command, alternating = launch("codex", "alternating-faults", "Quota exceeded")
        quota, = observe(alternating)
        assert_notice(alternating, quota, "blocked", "quota", "screen_notice", "blocked")
        write(command, "Connection reset")
        disconnect, = observe(alternating)
        assert_notice(alternating, disconnect, "unknown", "protocol_disconnected", "screen_notice", "disconnected")
        for repeated in ("Quota exceeded: updated", "Connection reset: updated"):
            write(command, repeated)
            self.assertFalse(observe(alternating, timeout="1.25", code=3))
        write(command, "The response has arrived.")
        recovered, = observe(alternating)
        assert_notice(alternating, recovered, "recovered", "recovered", "changed_activity", "active")
        write(command, "Quota exceeded")
        quota_again, = observe(alternating)
        assert_notice(alternating, quota_again, "blocked", "quota", "screen_notice", "blocked")
        self.assertNotEqual(quota_again["messageId"], quota["messageId"])

        banner, readonly = launch("codex", "readonly-banner",
            f"■ This chat uses {SECRET}, but Codex now uses other,\n"
            "  so this chat is read-only.")
        fault, = observe(readonly)
        assert_notice(readonly, fault, "blocked", "permission_required", "screen_notice", "awaiting_permission")
        write(banner, "This conversation is read-only or unavailable; no operation was sent.\n"
                      "The current output has arrived.")
        recovered, = observe(readonly)
        assert_notice(readonly, recovered, "recovered", "recovered", "changed_activity", "active")
        write(banner, "• This conversation is unavailable. Its cached transcript\n"
                      "  and draft remain here; input is paused.")
        fault, = observe(readonly)
        assert_notice(readonly, fault, "blocked", "permission_required", "screen_notice", "awaiting_permission")

        command, blocked = launch("codex", "other-faults", "Login required")
        for screen, kind, category, state in (
            ("Login required", "blocked", "authentication", "blocked"),
            ("Policy rejected", "blocked", "policy_rejected", "blocked"),
            ("Read-only session", "blocked", "permission_required", "awaiting_permission"),
            ("Request timed out", "unknown", "timeout", "timed_out"),
        ):
            with self.subTest(screen=screen):
                write(command, f"{screen}: {SECRET}")
                message, = observe(blocked)
                assert_notice(blocked, message, kind, category, "screen_notice", state)
        write(command, "exit")
        message, = observe(blocked)
        assert_notice(blocked, message, "exited", "process_exited", "verified_exit", "exited")
        self.assertEqual(message["fault"]["exitCode"], 23)
        identity = json.loads(command.with_suffix(".identity").read_text())
        self.assertEqual({key: message["fault"][key] for key in identity}, identity)
        self.assertFalse(observe(blocked, timeout="1.25", code=3))

        command, awaiting = launch("opencode", "awaiting", f"Quota exceeded: {SECRET}")
        # The Rust base reserves ask/reply. Seed a private question receipt of
        # the existing shape and exercise the real controller read and wait.
        ledger = self.state / "messages.json"
        messages = json.loads(ledger.read_text())
        question = dict(type="question", id=len(messages) + 1, messageId="m-question",
                        state="processed", statusVersion=3,
                        question_payload={"text": "fixture question", "options": ["one", "two"]},
                        **{key: awaiting[key] for key in ("dispatch", "round", "attempt", "scope", "role")})
        write(ledger, json.dumps([*messages, {**question, "scope": "unrelated"}]))
        fault, = observe(awaiting)
        assert_notice(awaiting, fault, "blocked", "quota", "screen_notice", "blocked")
        messages = json.loads(ledger.read_text())
        write(ledger, json.dumps([*messages, {**question, "id": len(messages) + 1,
                                             "messageId": "m-current-question"}]))
        self.assertFalse(observe(awaiting, timeout="1.25", deadline=".1", code=3))
        status = self.cli("status", "--dispatch", awaiting["dispatch"])["dispatches"][0]
        self.assertEqual(status["hostActivity"]["state"], "awaiting_answer")
        self.assertNotEqual(status["state"], "done")
        write(command, "Ordinary output while awaiting the answer")
        self.assertFalse(observe(awaiting, stall=".2", deadline=".2", timeout="1.25", code=3))
        write(command, "Approval required")
        permission, = observe(awaiting, deadline=".1")
        assert_notice(awaiting, permission, "blocked", "permission_required", "screen_notice", "awaiting_permission")
        write(command, "exit")
        terminal, = observe(awaiting)
        assert_notice(awaiting, terminal, "exited", "process_exited", "verified_exit", "exited")
        self.assertEqual(terminal["fault"]["exitCode"], 23)
        self.assertFalse(observe(awaiting, timeout="1.25", code=3))

        _, fenced = launch("codex", "close-fence", "Quota exceeded")
        lock_dir = self.state / "close-locks"
        lock_dir.mkdir(exist_ok=True)
        close_lock = lock_dir / f'{fenced["dispatch"]}.{fenced["attempt"]}'
        archive = self.state / "dispatches" / f'{fenced["dispatch"]}.{fenced["attempt"]}.closed'
        with close_lock.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertFalse(observe(fenced, timeout="1.25", code=3))
            write(archive, json.dumps({**{key: fenced[key] for key in ("dispatch", "round", "attempt")},
                                       "resourceCleanup": "uncertain", "closeEffect": "already_absent",
                                       "worker": {"state": "exited", "exit_code": 0}}))
        fault, = observe(fenced)
        assert_notice(fenced, fault, "blocked", "quota", "screen_notice", "blocked")
        view = self.cli("status", "--dispatch", fenced["dispatch"])["dispatches"][0]
        self.assertEqual(view["worker"]["state"], "running")

        _, archived = launch("codex", "archive-done", "Quota exceeded")
        record = self.state / "dispatches" / f'{archived["dispatch"]}.json'
        row = json.loads(record.read_text())
        done = {key: archived[key] for key in ("scope", "dispatch", "round", "attempt", "role", "report")}
        row["messages"] = [{**done, "type": "done", "messageId": "m-archived-done", "state": "processed"}]
        write(record, json.dumps(row))
        self.assertFalse(observe(archived, timeout="1.25", code=3))
        completed = self.cli("status", "--dispatch", archived["dispatch"])["dispatches"][0]
        self.assertEqual(completed["state"], "done")
        self.assertEqual([m["messageId"] for m in completed["messages"]], ["m-archived-done"])
        print(json.dumps({"case": "runtime-history-close-fence", "uncertainArchiveNotice": fault["messageId"],
                          "archivedCompletion": "m-archived-done", "closeLockSuppressed": True}), flush=True)
        self.assertNotIn(SECRET, ledger.read_text())
        for path in (self.state / "dispatches").glob("*.host-watch"):
            self.assertNotIn(SECRET, path.read_text())
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
