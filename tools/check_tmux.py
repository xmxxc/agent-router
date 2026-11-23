"""Exercise the private backend interface with a fresh tmux server and state roots."""
import json
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import tty
import unittest


def composer(mode, logfile):
    """Controlled terminal host: log actual input bytes and optionally drop Enter."""
    tty.setraw(0)
    draft = b""
    enters = 0
    banner = "This chat uses A, but Codex now uses B, so this chat is read-only."
    refused = "This conversation is read-only or unavailable; no operation was sent."
    notice = banner if mode == "readonly" else refused if mode == "retry-refusal" else ""
    def draw():
        shown = draft.decode("utf-8", errors="replace") or "Ask Codex to do anything"
        print(f"\x1b[2J\x1b[H{notice}\r\n› {shown}\r\n\r\n  model · cwd", end="", flush=True)
    with open(logfile, "a", buffering=1) as log:
        print(json.dumps({"cwd": os.getcwd(), "label_arg": sys.argv[-1],
                          "worker": [os.getenv(k) for k in ("WFC_WORKER_CALLBACK", "WFC_OWNER_TOKEN",
                                                            "WFC_STATE", "WFC_DISPATCH", "CHECK_ENV")]}), file=log)
        draw()
        while True:
            b = os.read(0, 1)
            if not b:
                return
            print(json.dumps([b.hex(), time.monotonic()]), file=log)
            if b == b"\r":
                enters += 1
                if mode == "refusal":
                    notice = refused
                if mode == "retry-refusal":
                    notice = "" if enters == 1 else refused
                    if enters > 1:
                        draft = b""
                elif mode == "change":
                    draft = b"someone else's draft"
                elif mode != "always-drop" and (mode != "drop" or enters > 1):
                    draft = b""
            elif b != b"\x03":
                draft += b
            draw()


if sys.argv[1:2] == ["--composer"]:
    composer(sys.argv[2], sys.argv[3])
    sys.exit(0)


WFC = Path(sys.argv.pop(1)).resolve()
TMUX = shutil.which("tmux")


class Backend(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="wfc-tmux-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.socket = str(self.root / "server")
        self.state = self.root / "state A"
        self.other = self.root / "state B"
        self.work = self.root / "worker space"
        self.work.mkdir()
        self.env = {"PATH": os.environ["PATH"], "HOME": str(self.root), "TERM": "xterm-256color",
                    "LANG": "C.UTF-8", "WFC_TMUX_SOCKET": self.socket, "WFC_TYPED_CHECK_SECONDS": "0.25",
                    "WFC_OWNER_TOKEN": "controller-only-value", "WFC_WORKER_CALLBACK": "stale"}
        self.env.pop("TMUX", None)
        self.tmux = [TMUX, "-f", "/dev/null", "-S", self.socket]
        self.addCleanup(subprocess.run, self.tmux + ["kill-server"], env=self.env, capture_output=True)
        # The sibling keeps the same isolated server alive across close and resume.
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "sibling", "/bin/cat"], env=self.env, check=True)
        self.launch = dict(cwd=str(self.work), argv=["/bin/cat"], label="窗口 A",
                           env={"WFC_WORKER_CALLBACK": "fresh"})

    def call(self, op, *, state=None, dispatch="d-abc12345", environ=None, **data):
        result = subprocess.run([WFC, "--tmux-backend"], text=True, capture_output=True,
                                input=json.dumps(dict(op=op, dispatch=dispatch, **data)),
                                env={**self.env, "WFC_STATE": str(state or self.state), **(environ or {})},
                                timeout=40)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def launch_composer(self, mode, op="create", expected="accepted", **data):
        log = self.root / (mode + ".log")
        argv = [sys.executable, str(Path(__file__).resolve()), "--composer", mode, str(log), "参数 ; $HOME"]
        launch = {**self.launch, "argv": argv, "env": {"WFC_WORKER_CALLBACK": "fresh", "CHECK_ENV": "中文"}}
        result = self.call(op, **launch, **data)
        self.assertEqual(result["result"], expected, result)
        self.assertEqual(Path(result["receipt"]).stat().st_mode & 0o777, 0o600)
        self.assertFalse(list(Path(result["receipt"]).parent.glob("*.spec")))
        deadline = time.monotonic() + 5
        while "model · cwd" not in self.call("read", state=data.get("state"))["screen"]:
            self.assertLess(time.monotonic(), deadline, "controlled worker did not draw its prompt")
            time.sleep(0.02)
        return log

    def events(self, log, minimum):
        deadline = time.monotonic() + 3
        while True:
            lines = log.read_text().splitlines()[1:]
            if len(lines) >= minimum or time.monotonic() >= deadline:
                return [json.loads(line) for line in lines]
            time.sleep(0.02)

    def attach_detach(self, argv):
        master, slave = pty.openpty()
        try:
            with subprocess.Popen(argv, env=self.env, stdin=slave, stdout=slave, stderr=slave) as client:
                try:
                    deadline = time.monotonic() + 5
                    output = b""
                    while b"model" not in output:
                        self.assertGreater(deadline - time.monotonic(), 0, output)
                        if select.select([master], [], [], 0.1)[0]:
                            output += os.read(master, 65536)
                    subprocess.run(self.tmux + ["detach-client", "-t", os.ttyname(slave)], env=self.env, check=True)
                    self.assertEqual(client.wait(timeout=5), 0)
                finally:
                    if client.poll() is None:
                        client.kill()
        finally:
            os.close(master)
            os.close(slave)

    def test_launch_attach_and_two_states(self):
        self.state = self.root / "state #,} A"
        log = self.launch_composer("ready")
        record = json.loads(log.read_text().splitlines()[0])
        self.assertEqual(record["cwd"], str(self.work))
        self.assertEqual(record["worker"], ["fresh", None, str(self.state), "d-abc12345", "中文"])
        self.assertEqual(record["label_arg"], "参数 ; $HOME")
        label = subprocess.check_output(self.tmux + ["display-message", "-p", "-t", "=wfc-d-abc12345:",
                                                     "#{window_name}"], env=self.env, text=True)
        self.assertEqual(label.strip(), "窗口 A")
        self.attach_detach(self.call("inspect")["attach"])
        self.assertTrue(self.call("inspect", backend=None)["owned"])
        self.assertEqual(self.call("create", state=self.other, dispatch="d-abc123456", **self.launch)["result"], "accepted")
        self.assertEqual(self.call("close", dispatch="d-abc123456")["result"], "refused")
        self.assertEqual(self.call("interrupt")["result"], "accepted")
        self.assertEqual(self.events(log, 1)[0][0], "03")
        self.assertEqual(self.call("close")["result"], "completed")
        self.assertTrue(self.call("inspect", state=self.other, dispatch="d-abc123456")["owned"])
        self.assertFalse(self.call("inspect")["alive"])  # no prefix fallback
        self.assertEqual(self.call("close", state=self.other, dispatch="d-abc123456")["result"], "completed")

    def test_input_counts_on_terminal(self):
        for mode, host, count, result in [
            ("empty", "codex", 1, "accepted"), ("drop", "codex", 2, "accepted"),
            ("always-drop", "codex", 3, "uncertain"), ("change", "codex", 1, "accepted"),
            ("drop", "claude", 1, "accepted"), ("drop", "opencode", 1, "accepted"),
            ("readonly", "codex", 0, "refused"), ("refusal", "codex", 1, "refused"),
            ("retry-refusal", "codex", 2, "refused"),
        ]:
            with self.subTest(mode=mode, host=host):
                log = self.launch_composer(mode)
                text = "[router] hello '世界' ; \\ $HOME #{pid} %1"
                reply = self.call("submit", text=text, host=host)
                self.assertEqual(reply["result"], result, reply)
                events = self.events(log, len(text.encode()) + count if count else 0)
                raw = b"".join(bytes.fromhex(byte) for byte, _ in events)
                self.assertEqual(raw, text.encode() + b"\r" * count if count else b"")
                if count:
                    first = len(text.encode())
                    self.assertGreaterEqual(events[first][1] - events[first-1][1], 0.45)
                self.assertEqual(self.call("close")["result"], "completed")
                log.unlink()

    def test_missing_tmux_and_lost_confirmations(self):
        missing = {"PATH": str(self.work)}
        self.assertEqual(self.call("create", environ=missing, **self.launch)["result"], "definitely-not-sent")
        reply = self.call("submit", environ=missing, text="abc")
        self.assertEqual((reply["result"], reply["category"]), ("definitely-not-sent", "input_not_delivered"))
        self.assertEqual(self.call("close", environ=missing)["result"], "definitely-not-sent")
        log = self.launch_composer("lost")
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, sys
a = sys.argv[1:]
if any("send-keys -t " in x and " Enter ; " in x for x in a):
    sys.exit(1)
os.execv({TMUX!r}, [{TMUX!r}, *a])
""")
        shim.chmod(0o700)
        reply = self.call("submit", text="once", environ=missing)
        self.assertEqual((reply["result"], reply["category"]), ("uncertain", "input_uncertain"))
        self.assertIn("Enter", reply["diagnostic"])
        events = self.events(log, 4)
        self.assertEqual(bytes.fromhex("".join(e[0] for e in events)), b"once")
        self.call("close")
        for argv, category in [(["/no/such/wfc-program"], "executable_missing"),
                               (["/bin/sh", "-c", "exit 23"], "process_exited")]:
            reply = self.call("create", **{**self.launch, "argv": argv})
            expected = "uncertain" if category == "process_exited" else "definitely-not-sent"
            self.assertEqual((reply["result"], reply["category"]), (expected, category))
            if category == "process_exited":
                self.assertEqual(reply["exitCode"], 23)

    def test_resume_host_inputs(self):
        log = self.launch_composer("resumed", op="resume", host="opencode", text="resume brief")
        events = self.events(log, len(b"resume brief\r"))
        self.assertEqual(bytes.fromhex("".join(e[0] for e in events)), b"resume brief\r")
        self.assertEqual(self.call("close")["result"], "completed")
        log = self.launch_composer("readonly", op="resume", host="codex", expected="refused",
                                   probe_read_only=True, environ={"WFC_RESUME_CHECK_SECONDS": "1.5"})
        self.assertEqual(self.events(log, 0), [])
        self.assertTrue(self.call("inspect")["owned"])

    def test_linked_window_refuses_keys(self):
        log = self.launch_composer("linked")
        subprocess.run(self.tmux + ["link-window", "-s", "=wfc-d-abc12345:", "-t", "=sibling:9"],
                       env=self.env, check=True)
        self.assertEqual(self.call("submit", text="foreign visible")["result"], "refused")
        self.assertEqual(self.call("interrupt")["result"], "refused")
        self.assertEqual(self.events(log, 0), [])
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], env=self.env, check=True)
        self.assertEqual(self.call("close")["result"], "refused")
        self.assertEqual(self.call("resume", **self.launch)["result"], "refused")
        subprocess.run(self.tmux + ["unlink-window", "-t", "=sibling:9"], env=self.env, check=True)
        self.assertEqual(self.call("close")["result"], "completed")

    def test_grouped_window_is_shared(self):
        log = self.launch_composer("grouped")
        subprocess.run(self.tmux + ["new-session", "-d", "-t", "wfc-d-abc12345", "-s", "group-member"],
                       env=self.env, check=True)
        self.attach_detach(self.tmux + ["attach-session", "-t", "=wfc-d-abc12345"])
        proof = subprocess.check_output(self.tmux + ["display-message", "-p", "-t", "=wfc-d-abc12345:",
                                        "#{window_linked} #{window_linked_sessions}"], env=self.env, text=True)
        self.assertEqual(proof.strip(), "0 2")
        for op in ["read", "submit", "interrupt"]:
            data = {"text": "shared"} if op == "submit" else {}
            self.assertEqual(self.call(op, **data)["result"], "refused", op)
        self.assertEqual(self.events(log, 0), [])
        self.assertEqual(self.call("close")["result"], "refused")
        subprocess.run(self.tmux + ["kill-session", "-t", "=wfc-d-abc12345"], env=self.env, check=True)
        self.assertEqual(self.call("resume", **self.launch)["result"], "uncertain")
        subprocess.run(self.tmux + ["has-session", "-t", "=sibling"], env=self.env, check=True)

    def test_lost_socket_and_original_server_exit(self):
        self.launch_composer("original")
        parked = self.root / "original-socket"
        os.rename(self.socket, parked)
        try:
            for op in ["inspect", "close", "create", "resume"]:
                data = self.launch if op in ("create", "resume") else {}
                self.assertEqual(self.call(op, **data)["result"], "uncertain", op)
                self.assertFalse(Path(self.socket).exists(), op)
            replacement = self.call("create", state=self.other, dispatch="d-other", **self.launch)
            self.assertEqual(replacement["result"], "accepted")
            self.assertEqual(self.call("resume", **self.launch)["result"], "uncertain")
        finally:
            if Path(self.socket).is_socket():
                self.stop_server()
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect")["owned"])
        subprocess.run(self.tmux + ["kill-server"], env=self.env, check=True)
        deadline = time.monotonic() + 3
        while (inspection := self.call("inspect"))["result"] == "uncertain":
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.03)
        self.assertFalse(inspection["alive"])
        self.assertEqual(self.call("resume", **self.launch)["result"], "accepted")

    def test_fresh_adoption_expires_command_saved_before_close(self):
        self.launch_composer("old")
        old = self.call("inspect")["attach"]
        token = subprocess.check_output(self.tmux + ["show-environment", "-t", "=wfc-d-abc12345",
                                                     "WFC_TMUX_LAUNCH"], env=self.env, text=True).strip()
        self.assertEqual(self.call("close")["result"], "completed")
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-d-abc12345", "-e",
                                    f"WFC_STATE={self.state}", "-e", token, "/bin/cat"], env=self.env, check=True)
        self.assert_attach_refused(old)
        current_token = subprocess.check_output(self.tmux + ["show-environment", "-t", "=wfc-d-abc12345",
                                                             "WFC_TMUX_LAUNCH"], env=self.env, text=True).strip()
        self.assertEqual(current_token, token)
        new = self.call("inspect")["attach"]
        self.assertNotEqual(old, new)
        self.assert_attach_refused(old)
        self.assertEqual(self.call("submit", text="new")["result"], "accepted")
        self.assertIn("new", self.call("read")["screen"])

    def test_restored_environment_does_not_replace_the_original_server(self):
        self.launch_composer("origin")
        original = self.call("inspect")
        token = subprocess.check_output(self.tmux + ["show-environment", "-t", "=wfc-d-abc12345",
                                                     "WFC_TMUX_LAUNCH"], env=self.env, text=True).strip()
        parked = self.root / "original"
        os.rename(self.socket, parked)
        try:
            for name in ["sibling", "wfc-d-abc12345"]:
                subprocess.run(self.tmux + ["new-session", "-d", "-s", name, "-e", f"WFC_STATE={self.state}",
                                            "-e", token, "/bin/cat"], env=self.env, check=True)
            for op in ["inspect", "close", "resume"]:
                self.assertEqual(self.call(op, **(self.launch if op == "resume" else {}))["result"], "uncertain")
            self.assert_attach_refused(original["attach"])
            subprocess.run(self.tmux + ["has-session", "-t", "=wfc-d-abc12345"], env=self.env, check=True)
        finally:
            if Path(self.socket).is_socket():
                self.stop_server()
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect")["owned"])

    def test_unlinked_socket_restored_by_original_server(self):
        log = self.launch_composer("restored")
        pid = int(subprocess.check_output(self.tmux + ["display-message", "-p", "#{pid}"], env=self.env))
        os.unlink(self.socket)
        try:
            self.assertEqual(self.call("inspect")["result"], "uncertain")
            self.assertEqual(self.call("resume", **self.launch)["result"], "uncertain")
            self.assertFalse(Path(self.socket).exists())
        finally:
            os.kill(pid, signal.SIGUSR1)
            deadline = time.monotonic() + 3
            while not Path(self.socket).exists():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("submit", text="once")["result"], "accepted")
        self.assertEqual(bytes.fromhex("".join(e[0] for e in self.events(log, 5))), b"once\r")

    def test_legacy_session_is_tracked_on_first_owned_observation(self):
        self.state.mkdir()
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-d-abc12345", "-e",
                                    f"WFC_STATE={self.state}", "/bin/cat"], env=self.env, check=True)
        self.assertTrue(self.call("inspect")["owned"])
        parked = self.root / "legacy-socket"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("close")["result"], "uncertain")
        finally:
            os.rename(parked, self.socket)
        subprocess.run(self.tmux + ["kill-session", "-t", "=wfc-d-abc12345"], env=self.env, check=True)
        self.assertFalse(self.call("inspect")["alive"])
        self.assertEqual(self.call("resume", **self.launch)["result"], "accepted")

    def stop_server(self):
        pid = int(subprocess.check_output(self.tmux + ["display-message", "-p", "#{pid}"], env=self.env))
        subprocess.run(self.tmux + ["kill-server"], env=self.env, check=True)
        deadline = time.monotonic() + 5
        stat = Path(f"/proc/{pid}/stat")
        while stat.exists():
            try:
                if stat.read_text().rsplit(") ", 1)[1].startswith("Z "):
                    break
            except FileNotFoundError:
                break
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)

    def test_old_attach_rechecks_ownership(self):
        self.launch_composer("original")
        argv = self.call("inspect")["attach"]
        subprocess.run(self.tmux + ["set-environment", "-t", "=wfc-d-abc12345", "WFC_STATE", str(self.other)],
                       env=self.env, check=True)
        self.assert_attach_refused(argv)
        subprocess.run(self.tmux + ["set-environment", "-g", "WFC_STATE", str(self.state)], env=self.env, check=True)
        subprocess.run(self.tmux + ["set-environment", "-u", "-t", "=wfc-d-abc12345", "WFC_STATE"],
                       env=self.env, check=True)
        self.assert_attach_refused(argv)
        self.stop_server()
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "sibling", "/bin/cat"],
                       env=self.env, check=True)
        self.launch_composer("replacement", state=self.other)
        self.assert_attach_refused(argv)

    def assert_attach_refused(self, argv):
        master, slave = pty.openpty()
        try:
            result = subprocess.run(argv, stdin=slave, stdout=slave, stderr=slave, env=self.env, timeout=3)
            self.assertNotEqual(result.returncode, 0)
        finally:
            os.close(slave)
            os.close(master)

    def test_resume_keeps_started_receipt_when_brief_could_not_be_sent(self):
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, sys
if "show-environment" in sys.argv:
    os.unlink(__file__)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        reply = self.call("resume", host="opencode", text="brief", environ={"PATH": str(self.work)}, **self.launch)
        self.assertEqual(json.loads(Path(reply["receipt"]).read_text())["category"], "started")
        self.assertEqual(reply["result"], "uncertain", reply)
        self.assertEqual(reply["launchResult"], "accepted")
        self.assertEqual(reply["inputResult"], "definitely-not-sent")
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("resume", **self.launch)["result"], "refused")

    def test_unknown_create_and_literal_response(self):
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys, time
a = sys.argv[1:]
if "new-session" in a or any("send-keys -t " in x and " -l -- " in x for x in a):
    subprocess.run([{TMUX!r}, *a], stdout=subprocess.DEVNULL, check=True)
    time.sleep(0.2)
    sys.exit(1)
os.execv({TMUX!r}, [{TMUX!r}, *a])
""")
        shim.chmod(0o700)
        lost = {"PATH": str(self.work)}
        reply = self.call("create", environ=lost, **self.launch)
        self.assertEqual(reply["result"], "uncertain")
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("create", **self.launch)["result"], "refused")
        reply = self.call("submit", text="literal once", environ=lost)
        self.assertEqual((reply["result"], reply["category"]), ("uncertain", "input_uncertain"))
        self.assertEqual(self.call("read")["screen"].count("literal once"), 1)
        self.assertEqual(self.call("close")["result"], "completed")

    def test_single_session_on_new_server(self):
        self.env["WFC_TMUX_SOCKET"] = str(self.root / "brand-new-server")
        self.addCleanup(subprocess.run, [TMUX, "-S", self.env["WFC_TMUX_SOCKET"], "kill-server"],
                        env=self.env, capture_output=True)
        created = self.call("create", **self.launch)
        self.assertEqual(created["result"], "accepted", created)
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("close")["result"], "completed")
        self.assertFalse(self.call("inspect")["alive"])

    def test_first_disconnected_legacy_execution(self):
        self.state.mkdir()
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-d-abc12345", "-e",
                                    f"WFC_STATE={self.state}", "/bin/cat"], env=self.env, check=True)
        parked = self.root / "legacy-alive"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("inspect", legacyExecutionId="old-attempt")["result"], "uncertain")
            for op in ("inspect", "close", "resume", "create"):
                result = self.call(op, **(self.launch if op in {"create", "resume"} else {}))
                self.assertEqual(result["result"], "uncertain", op)
                self.assertFalse(Path(self.socket).exists())
            subprocess.run([TMUX, "-S", parked, "has-session", "-t", "=wfc-d-abc12345"],
                           env=self.env, check=True)
        finally:
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect", legacyExecutionId="old-attempt")["owned"])
        self.assertEqual(self.call("close", legacyExecutionId="old-attempt")["result"], "completed")
        self.assertEqual(self.call("resume", legacyExecutionId="old-attempt", **self.launch)["result"], "accepted")

    def test_new_legacy_execution_after_previous_close(self):
        self.state.mkdir()
        args = ["new-session", "-d", "-s", "wfc-d-abc12345", "-e", f"WFC_STATE={self.state}", "/bin/cat"]
        subprocess.run(self.tmux + args, env=self.env, check=True)
        self.assertTrue(self.call("inspect", legacyExecutionId="prior-attempt")["owned"])
        self.assertEqual(self.call("close", legacyExecutionId="prior-attempt")["result"], "completed")
        subprocess.run(self.tmux + args, env=self.env, check=True)
        self.assertEqual(self.call("close", legacyExecutionId="prior-attempt")["result"], "uncertain")
        parked = self.root / "next-generation"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("close", legacyExecutionId="next-attempt")["result"], "uncertain")
            self.assertEqual(self.call("create", **self.launch)["result"], "uncertain")
        finally:
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("close", legacyExecutionId="next-attempt")["result"], "completed")
        self.assertEqual(self.call("close", legacyExecutionId="next-attempt")["result"], "completed")

    def test_prior_server_exit_cannot_release_a_different_legacy_execution(self):
        self.assertEqual(self.call("create", **self.launch)["result"], "accepted")
        other = "d-older-observation"
        def legacy(dispatch):
            subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-" + dispatch, "-e",
                                        f"WFC_STATE={self.state}", "/bin/cat"], env=self.env, check=True)
        legacy(other)
        self.assertTrue(self.call("inspect", dispatch=other)["owned"])
        record = self.state / "tmux" / (other + ".json")
        observed = json.loads(record.read_text())
        del observed["Observed"]["execution"]  # Exact schema written by R2.
        record.write_text(json.dumps(observed))
        for dispatch in ["d-abc12345", other]:
            self.assertEqual(self.call("close", dispatch=dispatch, legacyExecutionId="different")["result"],
                             "uncertain")
            self.assertTrue(self.call("inspect", dispatch=dispatch)["owned"])
        self.stop_server()
        for dispatch in ["d-abc12345", other]:
            legacy(dispatch)
        parked = self.root / "later-server"
        os.rename(self.socket, parked)
        try:
            for dispatch in ["d-abc12345", other]:
                self.assertEqual(self.call("inspect", dispatch=dispatch, legacyExecutionId="different")["result"],
                                 "uncertain")
                self.assertEqual(self.call("close", dispatch=dispatch)["result"], "uncertain")
                self.assertEqual(self.call("resume", dispatch=dispatch, **self.launch)["result"], "uncertain")
        finally:
            os.rename(parked, self.socket)
        for dispatch in ["d-abc12345", other]:
            self.assertTrue(self.call("inspect", dispatch=dispatch)["owned"])
            self.assertEqual(self.call("close", dispatch=dispatch)["result"], "completed")

    def test_business_input_stays_on_original_worker(self):
        log = self.launch_composer("worker")
        other = self.root / "second.log"
        target = "=wfc-d-abc12345:"
        first = subprocess.check_output(self.tmux + ["display-message", "-p", "-t", target,
                                                    "#{pane_id}"], env=self.env, text=True).strip()
        second = subprocess.check_output(self.tmux + ["new-window", "-P", "-F", "#{pane_id}", "-t", target,
            sys.executable, str(Path(__file__).resolve()), "--composer", "readonly", str(other), "other"],
            env=self.env, text=True).strip()
        self.assertNotEqual(first, second)
        deadline = time.monotonic() + 4
        while "this chat is read-only" not in self.call("read")["screen"]:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)
        self.assertEqual(self.call("submit", host="codex", text="once")["result"], "accepted")
        self.assertEqual(self.call("interrupt")["result"], "accepted")
        self.assertEqual(bytes.fromhex("".join(e[0] for e in self.events(log, 6))), b"once\r\x03")
        self.assertEqual(self.events(other, 0), [])
        subprocess.run(self.tmux + ["link-window", "-s", first, "-t", "=sibling:9"], env=self.env, check=True)
        self.assertIn(self.call("submit", text="shared")["result"], {"refused", "uncertain"})
        self.assertIn(self.call("interrupt")["result"], {"refused", "uncertain"})
        self.assertEqual(len(self.events(log, 0)), 6)
        self.assertEqual(self.events(other, 0), [])

        # Move the worker between literal text and Enter. Both processes stay live.
        subprocess.run(self.tmux + ["unlink-window", "-t", "=sibling:9"], env=self.env, check=True)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys
if any("send-keys -t " in x and " Enter ; " in x for x in sys.argv):
    subprocess.run({self.tmux!r} + ["move-window", "-s", {first!r}, "-t", "=sibling:9"], check=True)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        result = self.call("submit", text="late", environ={"PATH": str(self.work)})
        self.assertEqual((result["result"], result["category"]), ("uncertain", "input_uncertain"))
        for op in ["submit", "interrupt", "close", "resume"]:
            data = {"text": "moved"} if op == "submit" else self.launch if op == "resume" else {}
            self.assertIn(self.call(op, **data)["result"], {"refused", "uncertain"}, op)
        self.assertEqual(bytes.fromhex("".join(e[0] for e in self.events(log, 10))), b"once\r\x03late")
        self.assertEqual(self.events(other, 0), [])

    def test_legacy_worker_hint_survives_first_disconnected_observation(self):
        target = "=wfc-d-abc12345:"
        self.state.mkdir()
        worker = subprocess.check_output(self.tmux + ["new-session", "-d", "-P", "-F", "#{pane_id}",
            "-s", "wfc-d-abc12345", "-e", f"WFC_STATE={self.state}", "/bin/cat"], text=True, env=self.env).strip()
        subprocess.run(self.tmux + ["new-window", "-t", target, "/bin/cat"], env=self.env, check=True)
        self.assertEqual(self.call("inspect", legacyExecutionId="old-attempt")["result"], "uncertain")
        parked = self.root / "parked"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("inspect", legacyExecutionId="old-attempt", legacyWorkerPane=worker)["result"], "uncertain")
            self.assertEqual(self.call("inspect", environ={"WFC_TMUX_SOCKET": str(parked)})["result"], "uncertain")
        finally:
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("submit", text="original")["result"], "accepted")
        self.assertNotIn("original", self.call("read")["screen"])
        self.assertIn("original", subprocess.check_output(self.tmux + ["capture-pane", "-p", "-t", worker],
                                                          text=True, env=self.env))

    def test_abandoned_reservation_distinguishes_consumed_spec(self):
        # Crash boundary between the host taking the spec and publishing its server record.
        launches = self.state / "launches"
        claims = self.state / "tmux"
        launches.mkdir(parents=True)
        claims.mkdir()
        launch_id = "ab" * 16
        (claims / "d-abc12345.json").write_text(json.dumps({"Launch": launch_id}))
        receipt = launches / (launch_id + ".json")
        receipt.write_text(json.dumps({"category": "unknown"}))
        for op in ["inspect", "close", "create", "resume"]:
            self.assertEqual(self.call(op, **(self.launch if op in {"create", "resume"} else {}))["result"],
                             "uncertain", op)
        # In a different abandoned launch the host has not taken its spec.
        other_id = "cd" * 16
        (claims / "d-other.json").write_text(json.dumps({"Launch": other_id}))
        (launches / (other_id + ".spec")).write_text(json.dumps(self.launch))
        self.assertEqual(self.call("inspect", dispatch="d-other")["result"], "uncertain")
        self.assertEqual(self.call("close", dispatch="d-other")["result"], "completed")
        self.assertEqual(json.loads((launches / (other_id + ".json")).read_text())["category"], "not_started")
        late = subprocess.run([WFC, "--tmux-host", other_id], capture_output=True,
                              env={**self.env, "WFC_STATE": str(self.state)})
        self.assertNotEqual(late.returncode, 0)
        self.assertEqual(self.call("create", dispatch="d-other", **self.launch)["result"], "accepted")
        self.assertEqual(self.call("inspect")["result"], "uncertain")

    def test_cancelled_launch_survives_unwritable_receipt(self):
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, sys
from pathlib import Path
if "new-session" in sys.argv:
    receipt = Path(os.environ.get("WFC_TEST_LAUNCHES")) / (sys.argv[-1] + ".json")
    receipt.unlink()
    receipt.mkdir()
    sys.exit(1)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        env = {"PATH": str(self.work), "WFC_TEST_LAUNCHES": str(self.state / "launches")}
        reply = self.call("create", **self.launch, environ=env)
        self.assertEqual((reply["result"], reply["category"]), ("definitely-not-sent", "not_started"))
        self.assertNotIn("receipt", reply)
        marker = Path(reply["cancellationEvidence"])
        self.assertEqual(marker.read_bytes(), b"null")
        self.assertFalse(marker.with_suffix(".spec").exists())
        self.assertFalse(self.call("inspect")["alive"])
        self.assertEqual(self.call("create", **self.launch)["result"], "accepted")

    def test_retry_same_dispatch_after_launcher_could_not_start(self):
        parent = self.root / "new-directory"
        endpoint = {"WFC_TMUX_SOCKET": str(parent / "socket")}
        # Cleanup is scoped to this test even if a later assertion fails.
        self.addCleanup(subprocess.run, [TMUX, "-S", endpoint["WFC_TMUX_SOCKET"], "kill-server"],
                        capture_output=True, env=self.env)
        reply = self.call("create", **self.launch, environ=endpoint)
        self.assertEqual(reply["result"], "definitely-not-sent", reply)
        parent.mkdir()
        self.assertFalse(self.call("inspect", environ=endpoint)["alive"])
        log = self.root / "starts"
        argv = [sys.executable, str(Path(__file__).resolve()), "--composer", "retry", str(log), "retry"]
        self.assertEqual(self.call("create", **{**self.launch, "argv": argv}, environ=endpoint)["result"], "accepted")
        self.assertEqual(self.call("create", **self.launch, environ=endpoint)["result"], "refused")
        self.assertEqual(len(log.read_text().splitlines()), 1)
        self.assertEqual(self.call("close", environ=endpoint)["result"], "completed")

    def gated_exit(self, environ=None):
        trigger = self.root / "exit"
        argv = [sys.executable, "-c",
                "import sys,time,pathlib\np=pathlib.Path(sys.argv[1])\nwhile not p.exists(): time.sleep(.01)\nsys.exit(37)",
                str(trigger)]
        reply = self.call("create", **{**self.launch, "argv": argv}, environ=environ)
        self.assertEqual(reply["result"], "accepted", reply)
        return trigger, Path(reply["receipt"])

    def test_close_observes_exit_in_the_inspection_gap(self):
        trigger, receipt = self.gated_exit()
        self.assertTrue(self.call("inspect")["alive"])
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys, time
from pathlib import Path
if "#{{pane_active}}" in sys.argv:
    Path({str(trigger)!r}).touch()
    deadline = time.monotonic() + 5
    while subprocess.run({self.tmux!r} + ["has-session", "-t", "=wfc-d-abc12345"], capture_output=True).returncode == 0:
        assert time.monotonic() < deadline
        time.sleep(.01)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        closed = self.call("close", environ={"PATH": str(self.work)})
        self.assertEqual((closed["result"], closed.get("closeEffect")), ("completed", "already_absent"))
        self.assertEqual(json.loads(receipt.read_bytes()), {"category": "process_exited", "exitCode": 37})
        # Same dispatch, new live worker; its previous receipt still exists.
        self.assertEqual(self.call("resume", **self.launch)["result"], "accepted")
        closed = self.call("close")
        self.assertEqual((closed["result"], closed.get("closeEffect")), ("completed", "session_closed"))
        self.assertEqual(self.call("close").get("closeEffect"), "already_absent")

    def test_close_does_not_guess_exit_order_at_kill(self):
        trigger, receipt = self.gated_exit()
        subprocess.run(self.tmux + ["set-option", "-w", "-t", "=wfc-d-abc12345:", "remain-on-exit", "on"],
                       env=self.env, check=True)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys, time
from pathlib import Path
if any("kill-session -t " in arg for arg in sys.argv):
    Path({str(trigger)!r}).touch()
    deadline = time.monotonic() + 5
    while subprocess.check_output({self.tmux!r} + ["display-message", "-pt", "=wfc-d-abc12345:", "#{{pane_dead}}"]).strip() != b"1":
        assert time.monotonic() < deadline
        time.sleep(.01)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        closed = self.call("close", environ={"PATH": str(self.work)})
        self.assertEqual(closed["result"], "uncertain", closed)
        self.assertNotIn("closeEffect", closed)
        self.assertEqual(json.loads(receipt.read_bytes())["exitCode"], 37)
        self.assertEqual(self.call("close").get("closeEffect"), "worker_exited")

    def test_close_waits_for_an_unpublished_exit(self):
        # tmux continues a stopped direct child. Put a waiting parent between
        # tmux and the real receipt writer so only the latter can be paused.
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, sys
args = sys.argv[1:]
if "new-session" in args:
    i = args.index("--") + 1
    args[i:i] = [{sys.executable!r}, "-c", "import subprocess,sys; sys.exit(subprocess.call(sys.argv[1:]))"]
os.execv({TMUX!r}, [{TMUX!r}, *args])
""")
        shim.chmod(0o700)
        trigger, receipt = self.gated_exit(environ={"PATH": str(self.work)})
        pid = json.loads(receipt.read_bytes())["pid"]
        host = int(Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[1])
        self.assertIn(b"--tmux-host", Path(f"/proc/{host}/cmdline").read_bytes().split(b"\0"))
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], env=self.env, check=True)
        try:
            os.kill(host, signal.SIGSTOP)
            deadline = time.monotonic() + 4
            while Path(f"/proc/{host}/stat").read_text().rsplit(") ", 1)[1][0] != "T":
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
            trigger.touch()
            while Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1][0] != "Z":
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
            self.assertEqual(json.loads(receipt.read_bytes())["category"], "started")
            closed = self.call("close")
            self.assertEqual(closed["result"], "uncertain", closed)
            self.assertNotIn("closeEffect", closed)
        finally:
            os.kill(host, signal.SIGCONT)
        deadline = time.monotonic() + 4
        while json.loads(receipt.read_bytes()).get("exitCode") != 37:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        self.assertEqual(self.call("close").get("closeEffect"), "worker_exited")

    def test_lost_close_confirmation(self):
        self.assertEqual(self.call("create", **self.launch)["result"], "accepted")
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys
if any("kill-session -t " in arg for arg in sys.argv):
    subprocess.run([{TMUX!r}, *sys.argv[1:]], stdout=subprocess.DEVNULL, check=True)
    sys.exit(1)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        closed = self.call("close", environ={"PATH": str(self.work)})
        self.assertEqual(closed["result"], "uncertain", closed)
        self.assertNotIn("closeEffect", closed)
        self.assertEqual(self.call("close").get("closeEffect"), "already_absent")

    def test_close_after_normal_exit_with_remaining_window(self):
        trigger, receipt = self.gated_exit()
        worker = subprocess.check_output(self.tmux + ["display-message", "-p", "-t", "=wfc-d-abc12345:",
                                                     "#{pane_id}"], env=self.env, text=True).strip()
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], env=self.env, check=True)
        trigger.touch()
        deadline = time.monotonic() + 4
        while json.loads(receipt.read_text())["category"] != "process_exited" or worker in subprocess.check_output(
                self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"], text=True, env=self.env).splitlines():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        evidence = receipt.read_bytes()
        receipt.unlink()
        self.assertIn(self.call("close")["result"], {"refused", "uncertain"})
        receipt.write_bytes(evidence)
        closed = self.call("close")
        self.assertEqual((closed["result"], closed.get("closeEffect")), ("completed", "worker_exited"))
        self.assertEqual(json.loads(receipt.read_bytes())["exitCode"], 37)
        self.assertEqual(self.call("resume", **self.launch)["result"], "accepted")
        subprocess.run(self.tmux + ["has-session", "-t", "=sibling"], env=self.env, check=True)

    def test_lifecycle(self):
        launched = self.call("create", **self.launch)
        self.assertEqual(launched["result"], "accepted", launched)
        seen = self.call("inspect", backend="tmux")
        self.assertTrue(seen["owned"] and seen["alive"])
        self.assertEqual(self.call("inspect", state=self.other)["owned"], False)
        for op in ["read", "submit", "interrupt", "close", "resume", "create"]:
            data = self.launch if op in {"resume", "create"} else {"text": "foreign"} if op == "submit" else {}
            self.assertEqual(self.call(op, state=self.other, **data)["result"], "refused", op)
        self.assertEqual(self.call("submit", text="你好 'quoted' $HOME ;")["result"], "accepted")
        self.assertIn("你好 'quoted' $HOME ;", self.call("read")["screen"])
        self.assertEqual(self.call("interrupt")["result"], "accepted")
        self.assertEqual(self.call("close")["result"], "completed")
        self.assertEqual(self.call("close")["result"], "completed")
        self.assertFalse(self.call("inspect")["alive"])
        self.assertEqual(self.call("resume", **self.launch)["result"], "accepted")
        self.assertEqual(self.call("close")["result"], "completed")
        subprocess.run(self.tmux + ["has-session", "-t", "=sibling"], check=True)


if __name__ == "__main__":
    unittest.main()
