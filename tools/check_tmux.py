"""Exercise the public CLI and private backend with a fresh tmux server and state roots."""
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tty
import unittest


@contextmanager
def read_only_tree(root):
    modes = [(path, path.stat().st_mode & 0o777) for path in [root, *root.rglob("*")]]
    try:
        for path, mode in reversed(modes):
            path.chmod(mode & ~0o222)
        yield
    finally:
        for path, mode in modes:
            path.chmod(mode)


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
LEGACY_WFC = os.getenv("WFC_TEST_R1_BINARY")


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
                    "WFC_OWNER_TOKEN": "controller-only-value"}
        self.env.pop("TMUX", None)
        self.tmux = [TMUX, "-f", "/dev/null", "-S", self.socket]
        self.addCleanup(subprocess.run, self.tmux + ["kill-server"], env=self.env, capture_output=True)
        # The sibling keeps the same isolated server alive across close and resume.
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "sibling", "/bin/cat"], env=self.env, check=True)
        self.launch = dict(cwd=str(self.work), argv=["/bin/cat"], label="窗口 A",
                           env={"WFC_WORKER_CALLBACK": "fresh"})

    def call(self, op, *, state=None, dispatch="d-abc12345", environ=None, binary=None, **data):
        result = subprocess.run([binary or WFC, "--tmux-backend"], text=True, capture_output=True,
                                input=json.dumps(dict(op=op, dispatch=dispatch, **data)),
                                env={**self.env, "WFC_WORKER_CALLBACK": "stale",
                                     "WFC_STATE": str(state or self.state), **(environ or {})},
                                timeout=40)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def prepare_submission(self, launch_id, execution_id, dispatch="d-abc12345"):
        path = self.state / "launches" / (launch_id + ".submission")
        path.write_text(json.dumps(dict(launchId=launch_id, executionId=execution_id, dispatch=dispatch,
                                        socket=self.socket, origin=None, phase="Prepared")))

    def active_pane(self, dispatch="d-abc12345"):
        return subprocess.check_output(self.tmux + ["display-message", "-pt", f"=wfc-{dispatch}:", "#{pane_id}"],
                                       env=self.env, text=True).strip()

    def test_read_only_unowned_close_replays_confirmed_archive(self):
        self.assertNotEqual(os.geteuid(), 0, "read-only fixture requires an ordinary user")
        scope = "archive"

        def cli(*args, state=None):
            return subprocess.run([WFC, *args], cwd=self.work,
                                  env={**self.env, "WFC_STATE": str(state or self.state)},
                                  text=True, capture_output=True, timeout=15)

        def data(*args, state=None):
            result = cli(*args, state=state)
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(result.stdout)

        (self.work / "task").write_text("controlled worker")
        config = self.work / "launch.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [
            sys.executable, "-c", "import time; time.sleep(30)",
        ]}}}))
        worker = data("dispatch", "--scope", scope, "--host", "codex", "--cwd", ".",
                      "--task", "task", "--report", "report", "--launch-config", str(config))
        close = ("close", worker["dispatch"])
        with read_only_tree(self.state):
            first = cli(*close)
            self.assertEqual((first.returncode, first.stdout), (1, ""), first.stderr)
            self.assertIn("Permission denied", first.stderr)
        self.active_pane(worker["dispatch"])
        first = data(*close)
        self.assertEqual((first["result"], first["closeEffect"]), ("completed", "session_closed"))
        self.assertTrue(list(self.state.glob("relay-*.lock")))

        history = self.root / "history"
        (history / "dispatches").mkdir(parents=True)
        (history / "launches").mkdir()
        assignment = f"{worker['dispatch']}.json"
        archive_name = f"{worker['dispatch']}.{worker['attempt']}.closed"
        for name in (assignment, archive_name):
            shutil.copyfile(self.state / "dispatches" / name, history / "dispatches" / name)
        launch = Path(worker["create"]["receipt"])
        shutil.copyfile(launch, history / "launches" / launch.name)
        self.assertFalse((history / "controller-owners.json").exists())
        archive = history / "dispatches" / archive_name
        saved = archive.read_bytes()

        def assert_read_only_replay():
            with read_only_tree(history):
                replay = data(*close, state=history)
                for field in ("dispatch", "round", "attempt", "result", "closeOperationId", "closeEffect"):
                    self.assertEqual(replay[field], first[field])
                self.assertEqual(replay["diagnostic"], "session was already gone")
                self.assertEqual(archive.read_bytes(), saved)
                self.assertFalse(list(history.glob("relay-*.lock")))
                return replay

        replay = assert_read_only_replay()
        # R2 could close this execution with an unresolved interrupt audit.
        # Its same-execution archive still permits a read-only replay.
        assignment_path = history / "dispatches" / assignment
        record = json.loads(assignment_path.read_text())
        record["audit"] = [dict(id=1, operation="interrupt", dispatch=worker["dispatch"], round=worker["round"],
                                attempt=worker["attempt"], result="uncertain", scopeFence=True)]
        legacy = json.dumps(record)
        assignment_path.write_text(legacy)
        self.assertEqual(assert_read_only_replay(), replay)
        self.assertEqual(assignment_path.read_text(), legacy)

        ambiguous = json.loads(saved)
        ambiguous["round"] += 1
        archive.write_text(json.dumps(ambiguous))
        with read_only_tree(history):
            refused = cli(*close, state=history)
            self.assertEqual((refused.returncode, refused.stdout), (1, ""), refused.stderr)
            self.assertIn("Permission denied", refused.stderr)

        a = data("acquire", "--scope", scope, "--owner", "A")
        b = data("takeover", "--scope", scope, "--owner", "B")
        stale = cli(*close, "--owner-token", a["token"])
        self.assertEqual((stale.returncode, stale.stdout), (1, ""), stale.stderr)
        self.assertEqual(json.loads(stale.stderr)["error"], "stale_controller_owner")
        with read_only_tree(self.state):
            locked = cli(*close, "--owner-token", b["token"])
            self.assertEqual((locked.returncode, locked.stdout), (1, ""), locked.stderr)
            self.assertIn("Permission denied", locked.stderr)
        self.assertEqual(data(*close, "--owner-token", b["token"])["closeOperationId"],
                         replay["closeOperationId"])
        print(json.dumps({"readOnlyClose": replay, "unlockedFirstCloseRefused": True,
                          "legacyUnresolvedArchiveReplayed": True, "ambiguousArchiveRefused": True,
                          "managedReplayRequiresLock": True,
                          "staleTokenRefused": True, "euid": os.geteuid()}), flush=True)

    def test_managed_dispatch_callback_survives_takeover(self):
        env = {**self.env, "WFC_STATE": str(self.state)}

        def wfc(*args):
            return json.loads(subprocess.check_output([WFC, *args], cwd=self.work, env=env,
                                                      text=True, timeout=15))

        script = """
import os, subprocess, time
from pathlib import Path
assert os.environ.get("WFC_WORKER_CALLBACK") not in (None, "", "configured-stale")
assert not any(key.upper() in ("WFC_CONTROLLER_TOKEN", "WFC_OWNER_TOKEN") for key in os.environ)
Path("ready").touch()
deadline = time.monotonic() + 15
while not Path("release").exists():
    assert time.monotonic() < deadline
    time.sleep(.02)
Path("report").write_text("controlled tmux completion")
subprocess.run([os.environ["WFC_BIN"], "done", "--report", "report", "--attempt",
                os.environ["WFC_ATTEMPT"]], check=True, timeout=5)
"""
        (self.work / "task").write_text("controlled worker")
        config = self.work / "launch.json"
        config.write_text(json.dumps({"env": {"WFC_WORKER_CALLBACK": "configured-stale"},
                                      "hosts": {"codex": {"command": [sys.executable, "-c", script]}}}))
        a = wfc("acquire", "--scope", "managed", "--owner", "A")
        dispatch = wfc("dispatch", "--scope", "managed", "--owner-token", a["token"],
                       "--host", "codex", "--cwd", ".", "--task", "task", "--report", "report",
                       "--launch-config", str(config))
        deadline = time.monotonic() + 5
        while not (self.work / "ready").exists():
            self.assertLess(time.monotonic(), deadline, "worker did not start with its own credential")
            time.sleep(.02)
        b = wfc("takeover", "--scope", "managed", "--owner", "B")
        (self.work / "release").touch()
        message, = wfc("wait", "--scope", "managed", "--owner-token", b["token"],
                       "--timeout", "5")["messages"]
        self.assertEqual((message["dispatch"], message["attempt"]),
                         (dispatch["dispatch"], dispatch["attempt"]))
        ack = wfc("ack", message["messageId"], "--owner-token", b["token"])
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 3))
        self.assertEqual(wfc("close", dispatch["dispatch"], "--owner-token", b["token"])["result"], "completed")
        print(json.dumps({"tmuxCallbackReceipt": ack, "attempt": message["attempt"]}), flush=True)

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
        self.state = self.root / "state #,} A's"
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
        closed = self.call("close")
        self.assertEqual(closed["result"], "completed", closed)
        for argv, category in [(["/no/such/wfc-program"], "executable_missing"),
                               (["/bin/sh", "-c", "exit 23"], "process_exited")]:
            reply = self.call("create", **{**self.launch, "argv": argv})
            expected = "uncertain" if category == "process_exited" else "definitely-not-sent"
            self.assertEqual((reply["result"], reply.get("category")), (expected, category), (argv, reply))
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
        new = self.call("inspect", legacyExecutionId="replacement", legacyWorkerPane=self.active_pane())["attach"]
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
        self.assertTrue(self.call("inspect", legacyExecutionId="legacy", legacyWorkerPane=self.active_pane())["owned"])
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

    def test_session_identity_is_recorded_before_a_fast_worker_runs(self):
        argv = [sys.executable, "-c", """import json, os, pathlib, sys
launches = pathlib.Path(os.environ["WFC_STATE"]) / "launches"
anchors = [json.loads(path.read_bytes()) for path in launches.glob("*.session")]
sys.exit(23 if len(anchors) == 1 and anchors[0]["pane"] == os.environ["TMUX_PANE"] else 71)
"""]
        attempt = self.call("create", executionId="fast", **{**self.launch, "argv": argv})
        self.assertEqual((attempt.get("category"), attempt.get("exitCode")), ("process_exited", 23), attempt)

    def test_same_launch_worker_receipt_cannot_substitute_for_session_identity(self):
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import json, os, sys
from pathlib import Path
args=sys.argv[1:]
if "new-session" in args:
    launch_id=args[args.index("--tmux-host")+1]
    record=Path({str(self.state)!r})/"launches"/(launch_id+".session")
    record.write_text(json.dumps(dict(category="unknown", launchId=launch_id)))
    args=args[:args.index(";")]
os.execv({TMUX!r},[{TMUX!r},*args])
""")
        shim.chmod(0o700)
        ran = self.root / "worker-ran"
        result = self.call("create", executionId="fast", environ={"PATH": str(self.work)},
                           **{**self.launch, "argv": ["/usr/bin/touch", str(ran)]})
        self.assertEqual(result.get("category"), "not_started", result)
        self.assertFalse(ran.exists())

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
        original = self.active_pane()
        parked = self.root / "legacy-alive"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("inspect", legacyExecutionId="old-attempt", legacyWorkerPane=original)["result"], "uncertain")
            self.assertEqual(self.call("inspect", freshDispatch=True)["result"], "uncertain")
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
        time.sleep(2 / os.sysconf("SC_CLK_TCK"))  # Give the original pane a distinct Linux start tick.
        self.assertTrue(self.call("inspect", legacyExecutionId="prior-attempt", legacyWorkerPane=self.active_pane())["owned"])
        self.assertEqual(self.call("close", legacyExecutionId="prior-attempt")["result"], "completed")
        subprocess.run(self.tmux + args, env=self.env, check=True)
        time.sleep(2 / os.sysconf("SC_CLK_TCK"))  # Give the original pane a distinct Linux start tick.
        self.assertEqual(self.call("close", legacyExecutionId="prior-attempt")["closeEffect"], "already_absent")
        next_pane = self.active_pane()
        parked = self.root / "next-generation"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("close", legacyExecutionId="next-attempt", legacyWorkerPane=next_pane)["result"], "uncertain")
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
        panes = {d: self.active_pane(d) for d in ["d-abc12345", other]}
        parked = self.root / "later-server"
        os.rename(self.socket, parked)
        try:
            for dispatch in ["d-abc12345", other]:
                self.assertEqual(self.call("inspect", dispatch=dispatch, legacyExecutionId="different", legacyWorkerPane=panes[dispatch])["result"],
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
        self.assertTrue(self.call("inspect", legacyExecutionId="old-attempt")["owned"])
        self.assertEqual(self.call("interrupt")["result"], "uncertain")
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
        (launches / (other_id + ".spec")).write_text(json.dumps({
            **self.launch, "env": {"WFC_STATE": str(self.state), "WFC_DISPATCH": "d-other"}}))
        (launches / (other_id + ".json")).write_text(json.dumps({"category": "unknown"}))
        self.prepare_submission(other_id, None, "d-other")
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
    receipt = Path(os.environ.get("WFC_TEST_LAUNCHES")) / (sys.argv[sys.argv.index("--tmux-host")+1] + ".json")
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
        self.assertEqual(json.loads(marker.read_bytes()), dict(
            category="not_started", dispatch="d-abc12345", executionId=None,
            closeOperationId=None, launchId=marker.stem))
        self.assertFalse(marker.with_suffix(".spec").exists())
        self.assertEqual(self.call("inspect")["result"], "uncertain")
        self.assertEqual(self.call("create", **self.launch)["result"], "uncertain")

    def test_native_socket_alias_preserves_server_identity(self):
        alias = self.root / "socket-alias"
        alias.symlink_to(self.socket)
        environ = {"WFC_TMUX_SOCKET": str(alias)}
        args = {"executionId": "A"}
        started = self.call("create", environ=environ, **args, **self.launch)
        self.assertEqual(started["result"], "accepted", started)
        receipt = Path(started["receipt"])
        self.assertEqual(json.loads(receipt.with_suffix(".session").read_text())["socket"], self.socket)
        self.assertTrue(self.call("inspect", environ=environ, **args)["workerBound"])
        self.assertTrue(self.call("inspect", **args)["workerBound"])
        foreign = self.root / "foreign-socket"
        foreign_tmux = [TMUX, "-S", str(foreign)]
        subprocess.run(foreign_tmux + ["new-session", "-d", "-s", "foreign", "/bin/cat"], check=True)
        self.addCleanup(subprocess.run, foreign_tmux + ["kill-server"], capture_output=True)
        alias.unlink()
        alias.symlink_to(foreign)
        key = dict(**args, closeOperationId="end")
        self.assertEqual(self.call("close", environ=environ, **key)["result"], "uncertain")
        self.assertTrue(self.call("inspect", **args)["workerBound"])
        alias.unlink()
        alias.symlink_to(self.socket)
        self.assertEqual(self.call("close", environ=environ, **key)["closeEffect"], "session_closed")
        subprocess.run(foreign_tmux + ["has-session", "-t", "=foreign"], check=True)
        subprocess.run(self.tmux + ["has-session", "-t", "=sibling"], check=True)

    def test_legacy_socket_alias_can_identify_original_pane(self):
        self.state.mkdir()
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-d-abc12345", "-e",
                                    f"WFC_STATE={self.state}", "/bin/cat"], env=self.env, check=True)
        time.sleep(2 / os.sysconf("SC_CLK_TCK"))
        alias = self.root / "alias"
        alias.symlink_to(self.socket)
        selector = {"legacyExecutionId": "old", "legacyWorkerPane": self.active_pane()}
        reply = self.call("inspect", environ={"WFC_TMUX_SOCKET": str(alias)}, **selector)
        self.assertTrue(reply["workerBound"], reply)
        self.assertEqual(self.call("close", legacyExecutionId="old")["closeEffect"], "session_closed")

    def test_legacy_cancelled_reservation_requires_empty_original_endpoint(self):
        launch = "af" * 16
        launches = self.state / "launches"
        claims = self.state / "tmux"
        launches.mkdir(parents=True)
        claims.mkdir()
        pointer = claims / "d-abc12345.json"
        pointer.write_text(json.dumps({"Launch": launch}))
        receipt = launches / (launch + ".json")
        receipt.write_text('{"category":"not_started"}')
        recovery = {
            "bootId": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            "pidNamespace": Path("/proc/self/ns/pid").stat().st_ino,
            "networkNamespace": Path("/proc/self/ns/net").stat().st_ino,
            "userNamespace": Path("/proc/self/ns/user").stat().st_ino,
            "effectiveUid": os.geteuid(),
        }
        self.assertEqual(self.call("inspect", legacyRecovery=recovery)["result"], "uncertain")
        parked = self.root / "moved-original"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("inspect", legacyRecovery=recovery)["result"], "uncertain")
            self.assertEqual(self.call("create", **self.launch)["result"], "uncertain")
        finally:
            os.rename(parked, self.socket)
        endpoint = Path(self.socket)
        original_socket = endpoint.lstat()
        self.stop_server()
        try:
            leftover_socket = endpoint.lstat()
        except FileNotFoundError:
            pass
        else:
            # Some tmux servers leave the socket entry behind after exiting.
            self.assertTrue(stat.S_ISSOCK(leftover_socket.st_mode))
            self.assertTrue(os.path.samestat(original_socket, leftover_socket))
            rows = Path("/proc/net/unix").read_bytes().split(b"\n")
            address = os.fsencode(self.socket)
            self.assertFalse(any(row.split(maxsplit=7)[7:] == [address] for row in rows))
            self.assertEqual(self.call("inspect", legacyRecovery=recovery)["result"], "uncertain")
            endpoint.unlink()
        self.assertEqual(self.call("inspect")["result"], "uncertain")
        for field in ("pidNamespace", "networkNamespace", "userNamespace", "effectiveUid"):
            wrong_scope = {**recovery, field: recovery[field] + 1}
            self.assertEqual(self.call("inspect", legacyRecovery=wrong_scope)["result"], "uncertain")
        restored = self.call("inspect", legacyRecovery=recovery)
        self.assertEqual((restored.get("result"), restored.get("alive")), ("accepted", False), restored)

        other_id = "bf" * 16
        pointer.write_text(json.dumps({"Registered": {"id": other_id, "execution": "A"}}))
        registered = launches / (other_id + ".json")
        registered.write_bytes(receipt.read_bytes())
        absent = self.call("inspect", executionId="A", legacyRecovery=recovery)
        self.assertEqual(absent, dict(result="accepted", alive=False, owned=False))
        self.assertEqual(self.call("inspect", executionId="A"), absent)
        self.assertEqual(json.loads(registered.read_text()), {"category": "not_started"})
        self.assertEqual(self.call("create", executionId="A", **self.launch)["result"], "accepted")

    def test_keyed_close_can_confirm_prepared_launch_without_claiming_revocation(self):
        endpoint = self.root / "absent-parent" / "socket"
        environ = {"WFC_TMUX_SOCKET": str(endpoint)}
        args = dict(executionId="A", closeOperationId="close:A")
        failed = self.call("create", environ=environ, executionId="A", **self.launch)
        self.assertEqual(failed.get("category"), "not_started", failed)
        receipt = Path(failed["receipt"])
        self.assertIsNone(json.loads(receipt.read_text())["closeOperationId"])
        endpoint.parent.mkdir()
        expected = dict(result="completed", closeEffect="already_absent", closeOperationId="close:A")
        self.assertEqual(self.call("revoke", environ=environ, **args)["result"], "uncertain")
        self.assertEqual(self.call("close", environ=environ, **args), expected)
        self.assertEqual(self.call("close", environ=environ, **args), expected)
        self.assertEqual(self.call("revoke", environ=environ, **args)["result"], "uncertain")
        self.assertIsNone(json.loads(receipt.read_text())["closeOperationId"])

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
        deadline = time.monotonic() + 5
        while not log.exists() or log.stat().st_size == 0:
            self.assertLess(time.monotonic(), deadline, "worker did not record its start")
            time.sleep(0.02)
        self.assertEqual(len(log.read_text().splitlines()), 1)
        self.assertEqual(self.call("close", environ=endpoint)["result"], "completed")

    def gated_exit(self, environ=None, binary=None):
        trigger = self.root / "exit"
        argv = [sys.executable, "-c",
                "import sys,time,pathlib\np=pathlib.Path(sys.argv[1])\nwhile not p.exists(): time.sleep(.01)\nsys.exit(37)",
                str(trigger)]
        reply = self.call("create", **{**self.launch, "argv": argv}, environ=environ, binary=binary)
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
        self.assertEqual(json.loads(receipt.read_bytes()), {"category": "process_exited", "exitCode": 37, "launchId": receipt.stem})
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
        self.assertEqual(self.call("create", executionId="lost", **self.launch)["result"], "accepted")
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys
if any("kill-session -t " in arg for arg in sys.argv):
    subprocess.run([{TMUX!r}, *sys.argv[1:]], stdout=subprocess.DEVNULL, check=True)
    sys.exit(1)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        args = dict(executionId="lost", closeOperationId="close:lost")
        closed = self.call("close", environ={"PATH": str(self.work)}, **args)
        self.assertEqual(closed["result"], "uncertain", closed)
        self.assertNotIn("closeEffect", closed)
        self.assertEqual(self.call("close", **args)["result"], "uncertain")
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

    def test_closed_execution_history_never_adopts_a_later_session(self):
        self.state.mkdir()
        def start():
            if LEGACY_WFC:
                self.assertEqual(self.call("create", binary=LEGACY_WFC, **self.launch)["result"], "accepted")
                return self.active_pane()
            return subprocess.check_output(self.tmux + ["new-session", "-d", "-P", "-F", "#{pane_id}",
                "-s", "wfc-d-abc12345", "-e", f"WFC_STATE={self.state}", "/bin/cat"], env=self.env, text=True).strip()
        for gen in ["A", "B"]:
            pane = start()
            self.assertTrue(self.call("inspect", legacyExecutionId=gen, legacyWorkerPane=pane)["owned"])
            self.assertEqual(self.call("close", legacyExecutionId=gen)["result"], "completed")
        self.assertFalse(self.call("inspect", legacyExecutionId="A")["alive"])
        pane = start()
        pointer = self.state / "tmux/d-abc12345.json"
        before = pointer.read_bytes()
        for op in ["inspect", "close", "submit", "interrupt", "read", "resume", "create"]:
            data = self.launch if op in {"create", "resume"} else {"text": "late"} if op == "submit" else {}
            result = self.call(op, legacyExecutionId="A", **data)
            if op == "inspect":
                self.assertFalse(result["alive"])
            elif op == "close":
                self.assertEqual(result["closeEffect"], "already_absent")
            else:
                self.assertNotEqual(result["result"], "accepted")
            self.assertEqual(pointer.read_bytes(), before)
        token = subprocess.run(self.tmux + ["show-environment", "-t", "=wfc-d-abc12345", "WFC_TMUX_LAUNCH"],
                               env=self.env, capture_output=True)
        self.assertNotEqual(token.returncode, 0)
        self.assertTrue(self.call("inspect", legacyExecutionId="C", legacyWorkerPane=pane)["owned"])
        self.assertEqual(self.call("submit", text="current")["result"], "accepted")

    def test_remaining_tool_pane_is_not_the_legacy_worker(self):
        self.state.mkdir()
        target = "=wfc-d-abc12345:"
        if LEGACY_WFC:
            trigger, receipt = self.gated_exit(binary=LEGACY_WFC)
            original = self.active_pane()
        else:
            original = subprocess.check_output(self.tmux + ["new-session", "-d", "-P", "-F", "#{pane_id}",
                "-s", "wfc-d-abc12345", "-e", f"WFC_STATE={self.state}", "/bin/cat"], env=self.env, text=True).strip()
        tool_log = self.root / "tool.log"
        subprocess.run(self.tmux + ["new-window", "-t", target, sys.executable, str(Path(__file__).resolve()),
            "--composer", "ready", str(tool_log), "tool"], env=self.env, check=True)
        if LEGACY_WFC:
            trigger.touch()
            deadline = time.monotonic() + 4
            while json.loads(receipt.read_bytes()).get("exitCode") != 37 or original in subprocess.check_output(
                    self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"], text=True, env=self.env).splitlines():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
        else:
            subprocess.run(self.tmux + ["kill-pane", "-t", original], env=self.env, check=True)
        inspection = self.call("inspect", legacyExecutionId="A")
        self.assertTrue(inspection["owned"])
        self.assertFalse(inspection["workerBound"])
        # A pre-R5 Observed record could contain this guessed tool pane.
        path = self.state / "tmux/d-abc12345.json"
        saved = json.loads(path.read_bytes())
        for field in ("pane", "launch", "receipt"):
            saved["Observed"].pop(field, None)
        saved["Observed"]["server"]["pane"] = self.active_pane()
        path.write_text(json.dumps(saved))
        deadline = time.monotonic() + 4
        while "model" not in self.call("read")["screen"]:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        for op in ("submit", "interrupt", "close"):
            result = self.call(op, **({"text": "wrong"} if op == "submit" else {}))
            self.assertEqual(result["result"], "uncertain", result)
        self.assertEqual(self.events(tool_log, 0), [])

    def test_new_dispatch_preflight_on_empty_socket(self):
        sock = str(self.root / "new.sock")
        env = {"WFC_TMUX_SOCKET": sock}
        self.addCleanup(subprocess.run, [TMUX, "-S", sock, "kill-server"], capture_output=True)
        self.assertEqual(self.call("inspect", environ=env)["result"], "uncertain")
        claim = self.state / "tmux/d-abc12345.json"
        self.assertFalse(claim.exists())
        claim.write_text(json.dumps({"LegacyPending": {"execution": None, "socket": sock, "pane": None}}))
        self.assertFalse(self.call("inspect", environ=env, freshDispatch=True)["alive"])
        self.assertFalse(Path(sock).exists())
        self.assertEqual(self.call("create", environ=env, **self.launch)["result"], "accepted")
        self.assertEqual(self.call("close", environ=env)["closeEffect"], "session_closed")

    def test_registered_input_could_not_start_tmux(self):
        log = self.launch_composer("missing-tmux")
        env = {"PATH": str(self.work)}
        self.assertEqual(self.call("inspect", environ=env)["result"], "uncertain")
        for op in ["submit", "interrupt"]:
            reply = self.call(op, environ=env, **({"text": "once"} if op == "submit" else {}))
            self.assertEqual((reply["result"], reply.get("category")), ("definitely-not-sent", "input_not_delivered"))
            self.assertEqual(reply.get("inspectionResult"), "uncertain")
        self.assertEqual(self.events(log, 0), [])
        self.assertEqual(self.call("submit", text="once")["result"], "accepted")
        self.assertEqual(bytes.fromhex("".join(e[0] for e in self.events(log, 5))), b"once\r")

    @unittest.skipUnless(LEGACY_WFC, "set WFC_TEST_R1_BINARY to the R1 executable for the upgrade test")
    def test_adopted_r1_receipt_recovers_remaining_window(self):
        trigger = self.root / "finish"
        argv = [sys.executable, "-c", "import sys,time,pathlib\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nsys.exit(37)",
                str(trigger)]
        created = self.call("create", binary=LEGACY_WFC, **{**self.launch, "argv": argv})
        self.assertEqual(created["result"], "accepted", created)
        receipt = Path(created["receipt"])
        original = self.active_pane()
        self.assertFalse(self.call("inspect")["workerBound"])
        self.assertTrue(self.call("inspect", legacyExecutionId=receipt.stem, legacyWorkerPane=original)["workerBound"])
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], check=True, env=self.env)
        trigger.touch()
        deadline = time.monotonic() + 5
        while json.loads(receipt.read_bytes()).get("exitCode") != 37 or original in subprocess.check_output(
                self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"], env=self.env, text=True).splitlines():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        self.assertEqual(self.call("close", legacyExecutionId=receipt.stem).get("closeEffect"), "worker_exited")
        self.assertEqual(self.call("resume", legacyExecutionId=receipt.stem, **self.launch)["result"], "accepted")
        self.assertTrue(self.call("inspect")["owned"])
        self.assertEqual(self.call("close", legacyExecutionId=receipt.stem)["closeEffect"], "already_absent")
        self.assertTrue(self.call("inspect")["owned"])

    @unittest.skipUnless(LEGACY_WFC, "set WFC_TEST_R1_BINARY to the R1 executable for the upgrade test")
    def test_legacy_inspect_replays_verified_receipt(self):
        trigger, receipt = self.gated_exit(binary=LEGACY_WFC)
        sibling = self.call("create", binary=LEGACY_WFC, dispatch="d-sibling", **self.launch)
        self.assertEqual(sibling["result"], "accepted")
        other_receipt = Path(sibling["receipt"])
        # A real receipt from the same state cannot identify a different worker.
        wrong = self.call("inspect", dispatch="d-sibling", legacyExecutionId="old-B",
                          legacyLaunchId=receipt.stem, receipt=str(receipt))
        self.assertNotIn("launchId", wrong)
        self.assertNotIn("receipt", wrong)
        args = dict(legacyExecutionId="old-A")
        original = self.active_pane()

        def check_association(reply):
            self.assertEqual(reply["result"], "accepted", reply)
            self.assertEqual((reply.get("legacyExecutionId"), reply.get("launchId"), reply.get("receipt")),
                             ("old-A", receipt.stem, str(receipt)))

        check_association(self.call("inspect", **args, legacyWorkerPane=original))
        check_association(self.call("inspect", **args))
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], check=True, env=self.env)
        trigger.touch()
        deadline = time.monotonic() + 5
        while json.loads(receipt.read_bytes()).get("exitCode") != 37 or original in subprocess.check_output(
                self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"], env=self.env, text=True).splitlines():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        check_association(self.call("inspect", **args))
        self.assertEqual(self.call("close", **args)["closeEffect"], "worker_exited")
        self.assertEqual(self.call("create", executionId="new-A", **self.launch)["result"], "accepted")
        replay = self.call("inspect", **args)
        self.assertFalse(replay["alive"])
        check_association(replay)
        self.assertEqual(json.loads(Path(replay["receipt"]).read_bytes())["exitCode"], 37)
        spoof = self.call("inspect", **args, legacyLaunchId=other_receipt.stem, receipt=str(other_receipt))
        self.assertNotEqual(spoof.get("receipt"), str(other_receipt))
        saved = receipt.read_bytes()
        receipt.unlink()
        missing = self.call("inspect", **args, receipt=str(other_receipt))
        self.assertNotIn("receipt", missing)
        self.assertNotIn("launchId", missing)
        receipt.write_bytes(saved)
        check_association(self.call("inspect", **args))
        self.assertTrue(self.call("inspect", executionId="new-A")["owned"])

    @unittest.skipUnless(LEGACY_WFC, "set WFC_TEST_R1_BINARY to the R1 executable for the upgrade test")
    def test_r1_origin_and_anonymous_adoption_token(self):
        first = self.call("create", binary=LEGACY_WFC, **self.launch)
        sibling = self.call("create", dispatch="d-sibling", binary=LEGACY_WFC, **self.launch)
        self.assertEqual((first["result"], sibling["result"]), ("accepted", "accepted"))
        receipt = Path(first["receipt"])
        original = self.active_pane()
        token = self.call("inspect")["attach"][-1]
        args = dict(legacyExecutionId=token, legacyLaunchId=receipt.stem)
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], check=True, env=self.env)
        subprocess.run(self.tmux + ["move-window", "-s", original, "-t", "=wfc-d-sibling:"], check=True, env=self.env)
        wrong = self.call("inspect", dispatch="d-sibling", **args)
        self.assertNotIn("receipt", wrong)
        self.assertFalse(wrong.get("workerBound", False))
        subprocess.run(self.tmux + ["move-window", "-s", original, "-t", "=wfc-d-abc12345:"], check=True, env=self.env)
        live = self.call("inspect", **args)
        self.assertEqual((live.get("legacyExecutionId"), live.get("receipt")), (token, str(receipt)))
        self.assertEqual(self.call("close", legacyExecutionId=token)["closeEffect"], "session_closed")
        closed = self.call("inspect", legacyExecutionId=token)
        self.assertFalse(closed["alive"])
        self.assertEqual((closed.get("launchId"), closed.get("receipt")), (receipt.stem, str(receipt)))

    def test_lost_create_recovers_only_the_registered_execution(self):
        trigger = self.root / "exit-41"
        launch = {**self.launch, "argv": [sys.executable, "-c",
            "import pathlib,sys,time\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nsys.exit(41)", str(trigger)]}
        # The public caller committed its attempt before sending create and lost stdout.
        proc = subprocess.run([WFC, "--tmux-backend"], input=json.dumps(dict(
            op="create", dispatch="d-abc12345", executionId="attempt-A", **launch)), text=True,
            env={**self.env, "WFC_STATE": str(self.state)}, stdout=subprocess.DEVNULL, check=True)
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(self.call("inspect", executionId="attempt-B")["result"], "uncertain")
        inspected = self.call("inspect", executionId="attempt-A")
        self.assertEqual((inspected["executionId"], inspected["owned"]), ("attempt-A", True))
        receipt = self.state / "launches" / (inspected["launchId"] + ".json")
        self.assertEqual(Path(inspected["receipt"]), receipt)
        trigger.touch()
        deadline = time.monotonic() + 4
        while json.loads(receipt.read_bytes()).get("exitCode") != 41:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        while self.call("inspect", executionId="attempt-A").get("alive") is not False:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        args = dict(executionId="attempt-A", closeOperationId="close:A")
        for _ in range(2):
            closed = self.call("close", **args)
            self.assertEqual(closed["closeEffect"], "already_absent")
            self.assertNotIn("category", closed)
            self.assertNotIn("cancellationEvidence", closed)
        self.assertEqual(self.call("create", executionId="attempt-B", **self.launch)["result"], "accepted")
        old = self.call("inspect", executionId="attempt-A")
        self.assertFalse(old["alive"])
        self.assertEqual(json.loads(Path(old["receipt"]).read_bytes())["exitCode"], 41)
        self.assertNotEqual(self.call("inspect", executionId="attempt-B")["launchId"], inspected["launchId"])

    def test_close_replays_first_action_after_caller_archive_failure(self):
        self.assertEqual(self.call("create", executionId="attempt-A", **self.launch)["result"], "accepted")
        args = dict(executionId="attempt-A", closeOperationId="close:round1")
        first = self.call("close", **args)
        self.assertEqual((first["result"], first["closeEffect"], first["closeOperationId"]),
                         ("completed", "session_closed", "close:round1"))
        # The caller failed to save the reply. A new process may only read the original action.
        self.assertEqual(self.call("close", **args, environ={"PATH": str(self.work)}), first)
        self.assertEqual(self.call("close", executionId="attempt-A", closeOperationId="new-operation")["closeEffect"],
                         "already_absent")
        self.assertEqual(self.call("create", executionId="attempt-B", **self.launch)["result"], "accepted")
        self.assertEqual(self.call("close", **args), first)
        self.assertTrue(self.call("inspect", executionId="attempt-B")["owned"])
        self.assertEqual(self.call("close", executionId="attempt-B", closeOperationId="close:round1")["closeEffect"],
                         "session_closed")

    def test_recover_atomic_pre_host_reservation_by_public_id(self):
        # Exact crash cut after publishing the claim, before starting tmux.
        state = self.state / "tmux"
        launches = self.state / "launches"
        state.mkdir(parents=True)
        launches.mkdir()
        launch = "ac" * 16
        pointer = state / "d-abc12345.json"
        pointer.write_text(json.dumps({"Registered": {"id": launch, "execution": "attempt-A"}}))
        (launches / (launch + ".spec")).write_text(json.dumps({
            **self.launch, "env": {"WFC_STATE": str(self.state), "WFC_DISPATCH": "d-abc12345"}}))
        (launches / (launch + ".json")).write_text('{"category":"unknown"}')
        args = dict(executionId="attempt-A", closeOperationId="close:A")
        self.prepare_submission(launch, "attempt-A")
        self.assertEqual(self.call("inspect", executionId="attempt-A")["result"], "uncertain")
        self.assertEqual(self.call("close", **args)["closeEffect"], "already_absent")
        self.assertEqual(json.loads((launches / (launch + ".json")).read_bytes())["category"], "not_started")
        self.assertEqual(self.call("create", executionId="attempt-A", **self.launch)["result"], "refused")
        self.assertEqual(self.call("create", executionId="attempt-B", **self.launch)["result"], "accepted")
        self.assertEqual(self.call("close", **args)["closeEffect"], "already_absent")
        self.assertTrue(self.call("inspect", executionId="attempt-B")["owned"])

    def test_registered_create_interrupted_before_tmux_start(self):
        dispatch = "d-abc12345"
        args = dict(executionId="attempt-A", closeOperationId="cancel-A")
        # The real create has already reserved both IDs when it invokes new-session.
        # Interrupt that caller before tmux consumes the spec, then exit the shim.
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, signal, sys
if "new-session" in sys.argv:
    os.kill(os.getppid(), signal.SIGKILL)
    sys.exit(0)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        result = subprocess.run([WFC, "--tmux-backend"], capture_output=True, text=True, timeout=15,
                                input=json.dumps(dict(op="create", dispatch=dispatch, executionId="attempt-A", **self.launch)),
                                env={**self.env, "PATH": str(self.work), "WFC_STATE": str(self.state)})
        self.assertEqual(result.returncode, -signal.SIGKILL, result.stderr)
        spec, = (self.state / "launches").glob("*.spec")
        offline = {"PATH": str(self.root / "unavailable")}
        def unproven(**data):
            response = self.call("revoke", environ=offline, **data)
            self.assertNotEqual(response.get("category"), "not_started", response)
            self.assertNotIn("cancellationEvidence", response)
            return response
        self.assertEqual(unproven(executionId="unregistered", closeOperationId="cancel-A")["result"], "uncertain")
        self.assertEqual(unproven(dispatch="d-other", **args)["result"], "uncertain")
        self.assertTrue(spec.is_file())
        original = spec.read_bytes()
        spec.unlink()  # A host could already have taken it.
        self.assertEqual(unproven(**args)["result"], "uncertain")
        spec.write_bytes(original)
        server = spec.with_suffix(".server")
        server.touch()
        self.assertEqual(unproven(**args)["result"], "uncertain")
        server.unlink()
        spec.write_text(json.dumps({**json.loads(original), "launchId": "00" * 16}))
        self.assertEqual(unproven(**args)["result"], "uncertain")
        host_env = {**self.env, "WFC_STATE": str(self.state), "WFC_DISPATCH": dispatch}
        def rejected_host():
            host = subprocess.run([WFC, "--tmux-host", spec.stem], capture_output=True, env=host_env, timeout=5)
            self.assertNotEqual(host.returncode, 0)
            self.assertTrue(spec.exists())
            self.assertFalse(server.exists())
        rejected_host()
        untagged = json.loads(original)
        untagged.pop("launchId")
        spec.write_text(json.dumps(untagged))
        self.assertEqual(unproven(**args)["result"], "uncertain")
        rejected_host()
        moved = self.root / "displaced"
        moved.write_bytes(original)
        spec.unlink()
        spec.symlink_to(moved)
        self.assertEqual(unproven(**args)["result"], "uncertain")
        rejected_host()
        spec.unlink()
        spec.write_bytes(original)

        revoked = self.call("revoke", environ=offline, **args)
        record = dict(category="not_started", dispatch=dispatch, launchId=spec.stem, **args)
        receipt = spec.with_suffix(".json")
        self.assertEqual(revoked["result"], "uncertain", revoked)
        self.assertNotIn("closeEffect", revoked)
        self.assertEqual(Path(revoked["cancellationEvidence"]), receipt)
        self.assertFalse(spec.exists())
        self.assertEqual(json.loads(receipt.read_bytes()), record)
        self.assertEqual(self.call("revoke", environ=offline, **args), revoked)
        self.assertEqual(unproven(executionId="attempt-A", closeOperationId="different")["result"], "uncertain")
        self.assertEqual(self.call("create", executionId="attempt-B", **self.launch)["result"], "uncertain")
        self.assertEqual(self.call("close", **args)["result"], "uncertain")

    def test_recover_close_after_atomic_spec_take(self):
        dispatch = "d-abc12345"
        launch = "ab" * 16
        args = dict(executionId="A", closeOperationId="close-A")
        owner = dict(dispatch=dispatch, launchId=launch, **args)
        registry = self.state / "tmux"
        registry.mkdir(parents=True)
        (registry / (dispatch + ".json")).write_text(json.dumps({"Registered": {"id": launch, "execution": "A"}}))
        (registry / (dispatch + ".executions.json")).write_text(json.dumps({
            "launches": {}, "closed": [], "close_operations": {launch: {"close-A": "Pending"}}}))
        prefix = self.state / "launches" / launch
        prefix.parent.mkdir()
        self.prepare_submission(launch, "A", dispatch)
        receipt = prefix.with_suffix(".json")
        receipt.write_text(json.dumps(dict(category="unknown", launchId=launch)))
        # Durable cut immediately after the atomic rename and before publishing the proof.
        held = prefix.with_suffix(".held")
        held.write_text(json.dumps({**self.launch, "launchId": launch,
                                    "env": {"WFC_STATE": str(self.state), "WFC_DISPATCH": dispatch}}))
        intent = prefix.with_suffix(".intent")
        intent.write_text(json.dumps(owner))
        offline = {"PATH": str(self.work)}
        self.assertEqual(self.call("revoke", executionId="A", closeOperationId="other",
                                   environ=offline)["result"], "uncertain")
        self.assertEqual(self.call("close", environ=offline)["result"], "uncertain")
        self.assertEqual(self.call("create", **self.launch, executionId="A")["result"], "refused")
        self.assertTrue(held.exists())
        closed = self.call("revoke", environ=offline, **args)
        proof = dict(category="not_started", **owner)
        self.assertEqual(closed, dict(result="completed", closeEffect="already_absent",
                                      cancellationEvidence=str(receipt), **proof))
        self.assertEqual(json.loads(receipt.read_bytes()), proof)
        self.assertFalse(held.exists())
        self.assertFalse(intent.exists())
        self.assertEqual(self.call("revoke", environ=offline, **args), closed)

    def gated_retained_session(self):
        gate = self.root / "allow-host"
        ran = self.root / "worker-ran"
        helper = self.root / "gate.py"
        helper.write_text("import os,sys,time\nfrom pathlib import Path\n"
                          "gate,*argv=sys.argv[1:]\n"
                          "while not Path(gate).exists(): time.sleep(.02)\n"
                          "os.execv(argv[0],argv)\n")
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os,signal,subprocess,sys
args=sys.argv[1:]
if "new-session" in args:
    i=args.index("--")+1
    args[i:i]=[{sys.executable!r},{str(helper)!r},{str(gate)!r}]
    subprocess.run([{TMUX!r},*args],check=True)
    os.kill(os.getppid(),signal.SIGKILL)
    sys.exit(0)
os.execv({TMUX!r},[{TMUX!r},*args])
""")
        shim.chmod(0o700)
        launch = {**self.launch, "argv": [sys.executable, "-c",
            "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(ran)]}
        args = dict(executionId="A", closeOperationId="close:A")
        proc = subprocess.run([WFC, "--tmux-backend"], capture_output=True, text=True, timeout=15,
                              input=json.dumps(dict(op="create", dispatch="d-abc12345", executionId="A", **launch)),
                              env={**self.env, "PATH": str(self.work), "WFC_STATE": str(self.state)})
        self.assertEqual(proc.returncode, -signal.SIGKILL, proc.stderr)
        pane = self.active_pane()
        subprocess.run(self.tmux + ["set-option", "-w", "-t", pane, "remain-on-exit", "on"], check=True)
        return gate, ran, pane, args

    def release_retained_host(self, gate, pane):
        gate.touch()
        deadline = time.monotonic() + 8
        while subprocess.check_output(self.tmux + ["display-message", "-pt", pane, "#{pane_dead}"]).strip() != b"1":
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)

    def test_revoke_and_close_a_retained_session_after_prestart_cancellation(self):
        gate, ran, pane, args = self.gated_retained_session()
        revoked = self.call("revoke", **args)
        self.assertEqual((revoked["result"], revoked["category"]), ("uncertain", "not_started"), revoked)
        self.assertNotIn("closeEffect", revoked)
        self.assertEqual(self.active_pane(), pane)
        self.assertNotEqual(self.call("close", **args)["result"], "completed")
        self.assertNotEqual(self.call("create", executionId="B", **self.launch)["result"], "accepted")
        self.release_retained_host(gate, pane)
        name = "wfc-d-abc12345"
        launch_id = revoked["launchId"]
        subprocess.run(self.tmux + ["rename-session", "-t", f"={name}:", "renamed-original"], check=True)
        subprocess.run(self.tmux + ["set-environment", "-ut", "=renamed-original:", "WFC_TMUX_LAUNCH"], check=True)
        self.assertEqual(self.call("close", **args)["result"], "uncertain")
        self.assertEqual(self.call("create", executionId="B", **self.launch)["result"], "uncertain")
        subprocess.run(self.tmux + ["rename-session", "-t", "=renamed-original:", name], check=True)
        subprocess.run(self.tmux + ["set-environment", "-t", f"={name}:", "WFC_TMUX_LAUNCH", launch_id], check=True)
        self.assertEqual(self.active_pane(), pane)
        self.assertFalse(ran.exists())
        self.assertEqual(self.call("revoke", **args), revoked)
        closed = self.call("close", **args)
        self.assertEqual((closed["result"], closed["closeEffect"], closed["category"]),
                         ("completed", "unstarted_session_closed", "not_started"), closed)
        self.assertEqual(closed["cancellationEvidence"], revoked["cancellationEvidence"])
        self.assertEqual(self.call("create", executionId="B", **self.launch)["result"], "accepted")
        self.assertEqual(self.call("close", **args), closed)
        self.assertEqual(self.call("revoke", **args), closed)
        self.assertTrue(self.call("inspect", executionId="B")["workerBound"])
        self.assertIn("sibling", subprocess.check_output(self.tmux + ["list-sessions", "-F", "#{session_name}"],
                                                         text=True).splitlines())

    def test_retained_session_cleanup_confirmation_may_not_be_reclassified(self):
        gate, ran, pane, args = self.gated_retained_session()
        revoked = self.call("revoke", **args)
        self.assertEqual(revoked["category"], "not_started", revoked)
        self.release_retained_host(gate, pane)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os,subprocess,sys
args=sys.argv[1:]
if any("kill-session" in part for part in args):
    subprocess.run([{TMUX!r},*args],check=True,stdout=subprocess.DEVNULL)
    sys.exit(1)
os.execv({TMUX!r},[{TMUX!r},*args])
""")
        shim.chmod(0o700)
        lost = self.call("close", environ={"PATH":str(self.work)}, **args)
        self.assertEqual((lost["result"], lost["category"]), ("uncertain", "not_started"), lost)
        self.assertFalse(ran.exists())
        sessions = subprocess.check_output(self.tmux + ["list-sessions", "-F", "#{session_name}"],
                                           text=True).splitlines()
        self.assertEqual(sessions, ["sibling"])
        for op in ("close", "revoke"):
            replay = self.call(op, **args)
            self.assertEqual((replay["result"], replay["category"]), ("uncertain", "not_started"), replay)
            self.assertNotIn("closeEffect", replay)

    def test_r7_archived_revocation_does_not_prove_session_absence(self):
        dispatch = "d-abc12345"
        launch = "ab" * 16
        operation = "close:A"
        self.state.mkdir()
        claim = self.state / "tmux"
        claim.mkdir()
        (claim / (dispatch + ".json")).write_text(json.dumps({"Released": launch}))
        (claim / (dispatch + ".executions.json")).write_text(json.dumps({
            "launches": {"A": launch}, "closed": [launch],
            "close_operations": {launch: {operation: {"Confirmed": "already_absent"}}}}))
        launches = self.state / "launches"
        launches.mkdir()
        (launches / (launch + ".json")).write_text(json.dumps(dict(
            category="not_started", dispatch=dispatch, executionId="A",
            launchId=launch, closeOperationId=operation)))
        for op, fields in [("inspect", {}), ("inspect", {"executionId": "A"}), ("close", {}),
                           ("close", {"executionId": "A", "closeOperationId": "other"}),
                           ("inspect", {"legacyExecutionId": launch}),
                           ("close", {"legacyExecutionId": launch}),
                           ("close", {"legacyExecutionId": launch, "closeOperationId": operation}),
                           ("create", {**self.launch, "executionId": "B"})]:
            result = self.call(op, **fields)
            self.assertEqual(result["result"], "uncertain", result)
            self.assertNotIn("closeEffect", result)
        for op in ("revoke", "close"):
            result = self.call(op, executionId="A", closeOperationId=operation)
            self.assertEqual((result["result"], result["category"]), ("uncertain", "not_started"), result)
            self.assertNotIn("closeEffect", result)

        (launches / (launch + ".json")).unlink()
        server = launches / (launch + ".server")
        for invalid in (None, {}, {"category": "started", "launchId": launch}):
            if invalid is not None:
                server.write_text(json.dumps(invalid))
            for op in ("inspect", "close", "revoke"):
                fields = {"closeOperationId": operation} if op != "inspect" else {}
                result = self.call(op, executionId="A", **fields)
                self.assertEqual(result["result"], "uncertain", result)
                self.assertNotIn("category", result)
                self.assertNotIn("closeEffect", result)

    def test_plain_absence_rejects_conflicting_cancellation(self):
        opened = self.call("create", executionId="A", **self.launch)
        self.assertEqual(opened["result"], "accepted", opened)
        receipt = Path(opened["receipt"])
        marker = receipt.with_suffix(".revoked")
        parked = self.root / "parked"
        os.rename(self.socket, parked)
        try:
            for record in [None, dict(category="not_started", launchId=receipt.stem,
                                      dispatch="d-other", executionId="other", closeOperationId="foreign")]:
                marker.write_text(json.dumps(record))
                for op, fields in [("inspect", {}), ("close", {}), ("close", dict(executionId="A", closeOperationId="end"))]:
                    result = self.call(op, **fields)
                    self.assertEqual(result["result"], "uncertain", result)
                    self.assertNotIn("closeEffect", result)
        finally:
            marker.unlink(missing_ok=True)
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect", executionId="A")["workerBound"])

    def test_revoke_leaves_running_and_closed_executions_alone(self):
        args = dict(executionId="attempt-A", closeOperationId="close:A")
        self.assertEqual(self.call("revoke", **args)["result"], "uncertain")
        self.assertEqual(self.call("create", executionId="attempt-A", **self.launch)["result"], "accepted")
        parked = self.root / "parked"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("inspect", executionId="attempt-A")["result"], "uncertain")
            self.assertEqual(self.call("revoke", **args)["result"], "uncertain")
        finally:
            os.rename(parked, self.socket)
        pane = self.active_pane()
        for execution in ("other", "attempt-A"):
            result = self.call("revoke", **{**args, "executionId": execution})
            self.assertEqual(result["result"], "uncertain", result)
            self.assertNotIn("cancellationEvidence", result)
        self.assertEqual(self.active_pane(), pane)
        self.assertTrue(self.call("inspect", executionId="attempt-A")["workerBound"])
        ordinary = self.call("close", **args)
        self.assertEqual(ordinary["closeEffect"], "session_closed")
        self.assertEqual(self.call("revoke", **args)["result"], "uncertain")
        self.assertEqual(self.call("close", **args), ordinary)
        self.assertEqual(subprocess.check_output(self.tmux + ["list-sessions", "-F", "#{session_name}"],
                                                text=True, env=self.env).splitlines(), ["sibling"])

    def test_revoke_requires_native_execution_and_operation_selectors(self):
        valid = dict(executionId="A", closeOperationId="cancel-A")
        for invalid in [{}, {"executionId": "A"}, {"closeOperationId": "cancel-A"},
                        dict(legacyExecutionId="A", closeOperationId="cancel-A"),
                        {**valid, "legacyExecutionId": "A"}, {**valid, "executionId": ""},
                        {**valid, "closeOperationId": ""}, {**valid, "freshDispatch": True}]:
            with self.subTest(request=invalid):
                result = subprocess.run([WFC, "--tmux-backend"], capture_output=True, text=True, timeout=5,
                                        env={**self.env, "WFC_STATE": str(self.state)},
                                        input=json.dumps(dict(op="revoke", dispatch="d-abc12345", **invalid)))
                self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertFalse(self.state.exists())

    def test_native_inputs_require_the_live_original_worker(self):
        trigger, receipt = self.gated_exit()
        pane = self.active_pane()
        subprocess.run(self.tmux + ["set-option", "-w", "-t", pane, "remain-on-exit", "on"], check=True)
        self.assertTrue(self.call("inspect")["workerBound"])
        trigger.touch()
        deadline = time.monotonic() + 5
        while subprocess.check_output(self.tmux + ["display-message", "-pt", pane, "#{pane_dead}"]).strip() != b"1":
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        self.assertEqual(json.loads(receipt.read_bytes())["exitCode"], 37)
        log = self.root / "replacement.log"
        subprocess.run(self.tmux + ["respawn-pane", "-t", pane, sys.executable, str(Path(__file__).resolve()),
                                    "--composer", "ready", str(log), "replacement"], check=True)
        deadline = time.monotonic() + 5
        while "model" not in self.call("read")["screen"]:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        self.assertFalse(self.call("inspect")["workerBound"])
        for op, fields in [("submit", {"text": "boundary"}), ("interrupt", {})]:
            result = self.call(op, **fields)
            self.assertNotEqual(result["result"], "accepted", result)
            self.assertEqual(result["inputResult"], "definitely-not-sent", result)
        self.assertEqual(self.call("close")["result"], "refused")
        self.assertIn("model", self.call("read")["screen"])
        self.assertEqual(self.events(log, 0), [])

    def switch_worker_during_command(self, operation):
        trigger, receipt = self.gated_exit()
        pane = self.active_pane()
        subprocess.run(self.tmux + ["set-option", "-w", "-t", pane, "remain-on-exit", "on"], check=True)
        after_enter = self.root / "enter"
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys, time
from pathlib import Path
args = sys.argv[1:]
entered = Path({str(after_enter)!r})
if any("send-keys -t " in x and " Enter ; " in x for x in args):
    subprocess.run([{TMUX!r}, *args], check=True)
    entered.touch()
    sys.exit(0)
close = any("kill-session -t " in x for x in args)
observe = entered.exists() and any("capture-pane -p" in x for x in args)
if close or observe:
    Path({str(trigger)!r}).touch()
    deadline = time.monotonic() + 5
    while subprocess.check_output({self.tmux!r}+["display-message","-pt",{pane!r},"#{{pane_dead}}"]).strip() != b"1":
        assert time.monotonic() < deadline
        time.sleep(.02)
    subprocess.run({self.tmux!r}+["respawn-pane","-t",{pane!r},"/bin/cat"], check=True)
os.execv({TMUX!r}, [{TMUX!r}, *args])
""")
        shim.chmod(0o700)
        reply = self.call(operation, environ={"PATH": str(self.work)}, **({"text": "check"} if operation == "submit" else {}))
        self.assertEqual(json.loads(receipt.read_bytes()).get("exitCode"), 37)
        self.assertEqual(self.active_pane(), pane)
        self.assertFalse(self.call("inspect")["workerBound"])
        return reply

    def test_close_preserves_replacement_during_final_guard(self):
        self.assertEqual(self.switch_worker_during_command("close")["result"], "refused")

    def test_submit_does_not_confirm_from_replacement_screen(self):
        reply = self.switch_worker_during_command("submit")
        self.assertEqual((reply["result"], reply.get("category")), ("uncertain", "input_uncertain"), reply)

    def test_refused_close_operation_may_retry_after_exit(self):
        trigger = self.root / "go"
        argv = [sys.executable, "-c", "import pathlib,sys,time\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nsys.exit(41)", str(trigger)]
        reply = self.call("create", **{**self.launch, "argv": argv}, executionId="original")
        self.assertEqual(reply["result"], "accepted", reply)
        pane = self.active_pane()
        subprocess.run(self.tmux + ["link-window", "-s", pane, "-t", "=sibling:"], check=True)
        args = dict(executionId="original", closeOperationId="close-original")
        self.assertEqual(self.call("close", **args)["result"], "refused")
        self.assertTrue(self.call("inspect", executionId="original")["workerBound"])
        trigger.touch()
        deadline = time.monotonic() + 5
        while self.call("inspect", executionId="original").get("alive") is not False:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        self.assertEqual(json.loads(Path(reply["receipt"]).read_bytes())["exitCode"], 41)
        for _ in range(2):
            result = self.call("close", **args)
            self.assertEqual((result.get("closeEffect"), result.get("closeOperationId")), ("already_absent", "close-original"), result)

    @unittest.skipUnless(LEGACY_WFC, "set WFC_TEST_R1_BINARY to the R1 executable for the upgrade test")
    def test_r1_equal_launch_hint_and_offline_terminal_receipt(self):
        trigger, receipt = self.gated_exit(binary=LEGACY_WFC)
        pane = self.active_pane()
        args = dict(legacyExecutionId=receipt.stem, legacyWorkerPane=pane)
        hold = receipt.with_suffix(".hold")
        receipt.rename(hold)
        try:
            for extra in [{}, {"legacyLaunchId": receipt.stem}]:
                seen = self.call("inspect", **args, **extra)
                self.assertFalse(seen["workerBound"], seen)
                self.assertNotIn("receipt", seen)
                for op, fields in [("submit", {"text": "unverified"}), ("interrupt", {})]:
                    self.assertNotEqual(self.call(op, **args, **extra, **fields)["result"], "accepted")
        finally:
            hold.rename(receipt)
        seen = self.call("inspect", **args)
        self.assertTrue(seen["workerBound"], seen)
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], check=True)
        parked = self.root / "parked"
        os.rename(self.socket, parked)
        try:
            live = self.call("inspect", **args)
            self.assertEqual(live["result"], "uncertain")
            self.assertNotIn("receipt", live)
            trigger.touch()
            deadline = time.monotonic() + 5
            while json.loads(receipt.read_bytes()).get("exitCode") != 37:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            offline = self.call("inspect", **args)
            self.assertEqual(offline["result"], "uncertain", offline)
            self.assertNotIn("alive", offline)
            for key in ("legacyExecutionId", "launchId", "receipt"):
                self.assertEqual(offline.get(key), seen[key], offline)
            self.assertEqual(self.call("close", **args)["result"], "uncertain")
            self.assertNotIn("receipt", self.call("inspect", **args, legacyLaunchId="b"*32))
            receipt.rename(hold)
            self.assertNotIn("receipt", self.call("inspect", **args))
            receipt.symlink_to(hold)
            self.assertNotIn("receipt", self.call("inspect", **args))
            receipt.unlink()
            hold.rename(receipt)
        finally:
            os.rename(parked, self.socket)
        self.assertEqual(self.call("close", **args)["closeEffect"], "worker_exited")

    @unittest.skipUnless(LEGACY_WFC, "set WFC_TEST_R1_BINARY to the R1 executable for the upgrade test")
    def test_correct_unverified_r1_launch_hint(self):
        launches = {}
        for dispatch in ("left", "right"):
            made = self.call("create", dispatch=dispatch, binary=LEGACY_WFC, **self.launch)
            self.assertEqual(made["result"], "accepted", made)
            launches[dispatch] = Path(made["receipt"])
        for dispatch, other in [("left", "right"), ("right", "left")]:
            args = dict(dispatch=dispatch, legacyExecutionId="legacy-"+dispatch)
            wrong = self.call("inspect", **args, legacyLaunchId=launches[other].stem)
            self.assertNotIn("receipt", wrong)
            hint = dict(legacyWorkerPane=self.active_pane(dispatch))
            if dispatch == "left":
                hint["legacyLaunchId"] = launches[dispatch].stem
            verified = self.call("inspect", **args, **hint)
            self.assertEqual((verified.get("legacyExecutionId"), verified.get("launchId"), verified.get("receipt")),
                             (args["legacyExecutionId"], launches[dispatch].stem, str(launches[dispatch])), verified)
            self.assertNotIn("receipt", self.call("inspect", **args, legacyLaunchId=launches[other].stem))
            self.assertEqual(self.call("inspect", **args).get("receipt"), str(launches[dispatch]))

    def test_terminal_receipt_during_start_verification(self):
        trigger = self.root / "go"
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import json, os, sys, time
from pathlib import Path
directory = Path({str(self.state / "launches")!r})
if "#{{pane_active}}" in sys.argv and directory.exists():
    receipt, = directory.glob("*.json")
    if json.loads(receipt.read_bytes()).get("category") == "started":
        Path({str(trigger)!r}).touch()
        deadline = time.monotonic() + 5
        while json.loads(receipt.read_bytes()).get("exitCode") != 23:
            assert time.monotonic() < deadline
            time.sleep(.01)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        argv = [sys.executable, "-c", "import pathlib,sys,time\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nsys.exit(23)", str(trigger)]
        reply = self.call("create", **{**self.launch, "argv": argv}, environ={"PATH": str(self.work)})
        self.assertEqual((reply["result"], reply.get("category"), reply.get("exitCode")),
                         ("uncertain", "process_exited", 23), reply)

    def check_legacy_pane_process(self, r1):
        self.state.mkdir()
        session = "wfc-d-abc12345"
        original_log = self.root / "original.log"
        argv = [sys.executable, str(Path(__file__).resolve()), "--composer", "ready", str(original_log), "original"]
        args = dict(legacyExecutionId="import-A")
        if r1:
            created = self.call("create", binary=LEGACY_WFC, **{**self.launch, "argv": argv})
            self.assertEqual(created["result"], "accepted", created)
            receipt = Path(created["receipt"])
            hold = receipt.with_suffix(".hold")
            receipt.rename(hold)
        else:
            subprocess.run(self.tmux + ["new-session", "-d", "-s", session, "-e", f"WFC_STATE={self.state}", *argv], check=True)
        pane = self.active_pane()
        inspected = self.call("inspect", **args, legacyWorkerPane=pane)
        self.assertNotIn("receipt", inspected)
        if r1:
            hold.rename(receipt)
        else:
            self.assertTrue(inspected["workerBound"], inspected)
            deadline = time.monotonic() + 5
            while "model" not in self.call("read")["screen"]:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            self.assertEqual(self.call("submit", **args, text="hi")["result"], "accepted")
            self.assertEqual(self.call("interrupt", **args)["result"], "accepted")
            self.assertEqual(bytes.fromhex("".join(e[0] for e in self.events(original_log, 4))), b"hi\r\x03")
        replacement = self.root / "replacement.log"
        argv[-2:] = [str(replacement), "replacement"]
        subprocess.run(self.tmux + ["respawn-pane", "-k", "-t", pane, *argv], check=True)
        self.assertEqual(self.active_pane(), pane)
        deadline = time.monotonic()+5
        while "model" not in self.call("read")["screen"]:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        for fields in [args, {**args, "legacyWorkerPane": pane}]:
            with self.subTest(fields=fields):
                seen = self.call("inspect", **fields)
                self.assertFalse(seen["workerBound"], seen)
                self.assertNotIn("receipt", seen)
                for op in ("submit", "interrupt"):
                    result = self.call(op, **fields, **({"text": "boundary"} if op == "submit" else {}))
                    self.assertEqual(result.get("inputResult"), "definitely-not-sent", result)
                self.assertIn(self.call("close", **fields)["result"], {"refused", "uncertain"})
        self.assertEqual(self.events(replacement, 0), [])
        self.assertIn("model", self.call("read")["screen"])
        self.assertEqual(subprocess.check_output(self.tmux + ["list-sessions", "-F", "#{session_name}"],
                                               text=True).splitlines(), ["sibling", session])

    def test_plain_legacy_pane_process(self):
        self.check_legacy_pane_process(False)

    @unittest.skipUnless(LEGACY_WFC, "set WFC_TEST_R1_BINARY to the R1 executable for the upgrade test")
    def test_r1_pane_process_when_receipt_was_unavailable(self):
        self.check_legacy_pane_process(True)

    def test_disconnected_pane_hint_cannot_bind_a_new_process(self):
        self.state.mkdir()
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-d-abc12345", "-e",
                                    f"WFC_STATE={self.state}", "/bin/cat"], check=True)
        pane = self.active_pane()
        args = dict(legacyExecutionId="import", legacyWorkerPane=pane)
        parked = self.root / "parked"
        os.rename(self.socket, parked)
        try:
            self.assertEqual(self.call("inspect", **args)["result"], "uncertain")
            subprocess.run([TMUX, "-S", str(parked), "respawn-pane", "-k", "-t", pane, "/bin/cat"], check=True)
        finally:
            os.rename(parked, self.socket)
        self.assertFalse(self.call("inspect", **args)["workerBound"])
        self.assertNotEqual(self.call("close", **args)["result"], "completed")
        self.assertEqual(self.active_pane(), pane)

    def test_first_hint_precedes_wait_for_dispatch_lock(self):
        (self.state/"tmux").mkdir(parents=True)
        subprocess.run(self.tmux + ["new-session", "-d", "-s", "wfc-d-abc12345", "-e",
                                    f"WFC_STATE={self.state}", "/bin/cat"], check=True)
        pane = self.active_pane()
        request = dict(op="inspect", dispatch="d-abc12345", legacyExecutionId="earlier", legacyWorkerPane=pane)
        with (self.state/"tmux/d-abc12345.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with subprocess.Popen([WFC, "--tmux-backend"], env={**self.env, "WFC_STATE": str(self.state)},
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
                try:
                    child.stdin.write(json.dumps(request))
                    child.stdin.close()
                    subprocess.run(self.tmux + ["respawn-pane", "-k", "-t", pane, "/bin/cat"], check=True)
                finally:
                    fcntl.flock(lock, fcntl.LOCK_UN)
                self.assertEqual(child.wait(timeout=10), 0)
                self.assertFalse(json.load(child.stdout)["workerBound"])
        self.assertEqual(self.active_pane(), pane)

    def test_offline_pane_hint_requires_the_original_server(self):
        self.state.mkdir()
        alternate = self.root / "alternate"
        alternate_tmux = [TMUX, "-f", "/dev/null", "-S", str(alternate)]
        self.addCleanup(subprocess.run, alternate_tmux + ["kill-server"], capture_output=True, env=self.env)
        subprocess.run(alternate_tmux + ["new-session", "-d", "-s", "peer", "/bin/cat"], check=True)
        launch = ["new-session", "-d", "-s", "wfc-d-abc12345", "-e", f"WFC_STATE={self.state}", "/bin/cat"]
        for server in [self.tmux, alternate_tmux]:
            subprocess.run(server + launch, check=True)
        pane = self.active_pane()
        parked = self.root / "original"
        os.rename(self.socket, parked)
        try:
            args = dict(legacyExecutionId="old", legacyWorkerPane=pane)
            self.assertEqual(self.call("inspect", **args)["result"], "uncertain")
            os.rename(alternate, self.socket)
            try:
                seen = self.call("inspect", **args)
                self.assertFalse(seen.get("workerBound", False), seen)
                self.assertNotEqual(self.call("close", **args)["result"], "completed")
                self.assertEqual(self.active_pane(), pane)
            finally:
                os.rename(self.socket, alternate)
        finally:
            os.rename(parked, self.socket)
        self.assertTrue(self.call("inspect", **args)["workerBound"])
        self.assertEqual(self.call("close", **args)["result"], "completed")

    def test_native_close_uses_the_same_terminal_proof_as_inspect(self):
        trigger = self.root / "done"
        argv = [sys.executable, "-c", "import pathlib,sys,time\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nsys.exit(41)", str(trigger)]
        args = dict(executionId="A")
        made = self.call("create", **args, **{**self.launch, "argv": argv})
        receipt = Path(made["receipt"])
        original = self.active_pane()
        subprocess.run(self.tmux + ["new-window", "-t", "=wfc-d-abc12345:", "/bin/cat"], check=True)
        trigger.touch()
        deadline = time.monotonic() + 5
        while json.loads(receipt.read_bytes()).get("exitCode") != 41 or original in subprocess.check_output(
                self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"], text=True).splitlines():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        original_receipt = receipt.read_bytes()
        receipt.write_text(json.dumps(dict(category="process_exited", exitCode=17)))
        self.assertNotIn("receipt", self.call("inspect", **args))
        close = self.call("close", **args, closeOperationId="end")
        self.assertNotIn("closeEffect", close)
        self.assertNotEqual(close["result"], "completed")
        subprocess.run(self.tmux + ["has-session", "-t", "=wfc-d-abc12345"], check=True)
        receipt.write_bytes(original_receipt)
        self.assertEqual(self.call("close", **args, closeOperationId="end")["closeEffect"], "worker_exited")

    def test_first_native_lookup_recovers_offline_exit(self):
        trigger = self.root / "go"
        argv = [sys.executable, "-c", "import pathlib,sys,time\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nsys.exit(41)", str(trigger)]
        request = dict(op="create", dispatch="d-abc12345", executionId="A", **{**self.launch, "argv": argv})
        subprocess.run([WFC, "--tmux-backend"], input=json.dumps(request), text=True, stdout=subprocess.DEVNULL,
                       env={**self.env, "WFC_STATE": str(self.state)}, check=True)
        receipt, = (self.state/"launches").glob("*.json")
        sibling = self.call("create", dispatch="other", executionId="B", **self.launch)
        self.assertEqual(sibling["result"], "accepted")
        held = self.root/"parked"
        os.rename(self.socket, held)
        try:
            trigger.touch()
            deadline = time.monotonic()+5
            while json.loads(receipt.read_bytes()).get("exitCode") != 41:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            # This is the first client lookup. No online inspect primed a cache.
            got = self.call("inspect", executionId="A")
            self.assertEqual((got["result"], got.get("executionId"), got.get("launchId"), got.get("receipt")),
                             ("uncertain", "A", receipt.stem, str(receipt)), got)
            self.assertNotIn("alive", got)
            self.assertEqual(json.loads(Path(got["receipt"]).read_bytes())["exitCode"], 41)
            self.assertEqual(self.call("close", executionId="A")["result"], "uncertain")
            self.assertNotIn("receipt", self.call("inspect", executionId="B"))
            self.assertNotIn("receipt", self.call("inspect", dispatch="other", executionId="B"))
            original = receipt.read_bytes()
            receipt.unlink()
            self.assertNotIn("receipt", self.call("inspect", executionId="A"))
            for wrong in [dict(category="process_exited", exitCode=17, launchId=Path(sibling["receipt"]).stem),
                          dict(category="process_exited", exitCode=17)]:
                receipt.write_text(json.dumps(wrong))
                self.assertNotIn("receipt", self.call("inspect", executionId="A"))
            receipt.write_bytes(original)
        finally:
            os.rename(held, self.socket)
        self.assertEqual(self.call("inspect", executionId="A").get("receipt"), str(receipt))
        self.assertTrue(self.call("inspect", dispatch="other", executionId="B")["workerBound"])

    def test_public_question_reuses_terminal_evidence(self):
        env = {**self.env, "WFC_STATE": str(self.state)}
        for key in ("WFC_WORKER_CALLBACK", "WFC_OWNER_TOKEN"):
            env.pop(key, None)

        def cli(*args, expected=0, **overrides):
            result = subprocess.run([WFC, *args], env={**env, **overrides}, cwd=self.work,
                                    text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, expected, (args, result.stdout, result.stderr))
            if expected != 0:
                self.assertFalse(result.stdout)
                return result.stderr
            return json.loads(result.stdout)

        worker = self.work / "worker.py"
        worker.write_text("""import os, subprocess, sys, time
from pathlib import Path
secret = os.environ.get("WFC_WORKER_CALLBACK")
assert secret and all(secret not in arg for arg in sys.argv)
first = subprocess.check_output([os.environ["WFC_BIN"], "ask", "First question?",
                                 "--attempt", os.environ["WFC_ATTEMPT"],
                                 "--message-id", "first"])
Path("question.tmp").write_bytes(first)
Path("question.tmp").rename("question.json")
deadline = time.monotonic() + 20
while not Path("exit-now").exists():
    if time.monotonic() >= deadline:
        sys.exit(2)
    time.sleep(.02)
sys.exit(41)
""")
        (self.work / "task").write_text("ask the controller")
        config = self.work / "launch.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, str(worker)]}}}))
        record = cli("dispatch", "--host", "codex", "--task", "task", "--cwd", ".",
                     "--report", "report", "--scope", "approval", "--launch-config", str(config))
        dispatch, attempt = record["dispatch"], record["attempt"]
        self.assertNotIn("workerCallbackHash", record)

        def status():
            return cli("status", "--dispatch", dispatch)["dispatches"][0]

        def ask(text, key, expected=0):
            return cli("ask", text, "--attempt", attempt, "--message-id", key,
                       expected=expected, WFC_DISPATCH=dispatch)

        question = self.work / "question.json"
        deadline = time.monotonic() + 5
        while not question.exists():
            self.assertLess(time.monotonic(), deadline, "public worker did not publish its question")
            time.sleep(.025)
        first = json.loads(question.read_bytes())
        self.assertEqual(first["state"], "unsent")
        live = status()
        self.assertNotIn("workerCallbackHash", live)
        self.assertEqual(live["status"], "waiting-human")
        archive = self.state / "dispatches" / f"{dispatch}.{attempt}.closed"
        archive.write_text(json.dumps({"dispatch": dispatch, "attempt": attempt, "round": 1,
                                       "worker": {"state": "cancelled", "exit_code": None, "signal": None}}))
        self.assertEqual(status()["worker"]["state"], "running")
        second = ask("Question while online?", "second")
        self.assertNotEqual(second["messageId"], first["messageId"])
        archive.unlink()

        (self.work / "exit-now").touch()
        deadline = time.monotonic() + 5
        while status()["worker"].get("exit_code") != 41:
            self.assertLess(time.monotonic(), deadline, "same-launch exit receipt was not observed")
            time.sleep(.025)
        self.assertFalse(archive.exists())
        self.assertIn("round is not running", ask("Too late?", "late", expected=1))
        messages = cli("wait", "--scope", "approval", "--timeout", "0")["messages"]
        self.assertEqual([m["messageId"] for m in messages], [first["messageId"], second["messageId"]])
        confirmed = cli("ack", first["messageId"])
        self.assertEqual(ask("First question?", "first"), {**confirmed, "duplicate": True})
        self.assertEqual(status()["state"], "exited")

    def test_public_question_and_close_share_attempt_lock(self):
        env = {**self.env, "WFC_STATE": str(self.state)}
        for key in ("WFC_WORKER_CALLBACK", "WFC_OWNER_TOKEN"):
            env.pop(key, None)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys, time
from pathlib import Path
args = sys.argv[1:]
mode = os.environ.get("QUESTION_TEST_GATE")
root = Path({str(self.root)!r})
ready = root / (str(mode) + "-ready")
closing = mode == "close" and any("kill-session -t " in arg for arg in args)
probe = ["list-panes", "-t", os.environ.get("QUESTION_TEST_TARGET")]
inspecting = mode == "ask" and any(args[i:i + len(probe)] == probe for i in range(len(args)))
if (closing or inspecting) and not ready.exists():
    result = subprocess.run([{TMUX!r}, *args], capture_output=True) if inspecting else None
    ready.touch()
    deadline = time.monotonic() + 10
    while not (root / (mode + "-release")).exists():
        if time.monotonic() >= deadline:
            sys.exit(2)
        time.sleep(.02)
    if result is not None:
        sys.stdout.buffer.write(result.stdout)
        sys.stderr.buffer.write(result.stderr)
        sys.exit(result.returncode)
os.execv({TMUX!r}, [{TMUX!r}, *args])
""")
        shim.chmod(0o700)
        worker = self.work / "worker.py"
        worker.write_text("""import os, subprocess, time
from pathlib import Path
assert os.environ.get("WFC_WORKER_CALLBACK")
first = subprocess.check_output([os.environ["WFC_BIN"], "ask", "First question?",
                                 "--attempt", os.environ["WFC_ATTEMPT"], "--message-id", "original"])
Path("question.tmp").write_bytes(first)
Path("question.tmp").rename("question.json")
time.sleep(45)
""")
        (self.work / "task").write_text("ask the controller")
        config = self.work / "launch.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, str(worker)]}}}))

        processes = []

        def cleanup():
            for mode in ("ask", "close"):
                (self.root / (mode + "-release")).touch()
            for proc in processes:
                if proc.poll() is None:
                    try:
                        proc.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.communicate(timeout=5)

        self.addCleanup(cleanup)

        def start(*args, cwd=None, dispatch=None, gate=None):
            child_env = env.copy()
            if dispatch:
                child_env["WFC_DISPATCH"] = dispatch
            if gate:
                child_env.update(PATH=str(self.work) + ":" + env["PATH"], QUESTION_TEST_GATE=gate)
            if gate == "ask":
                child_env["QUESTION_TEST_TARGET"] = f"=wfc-{dispatch}:"
            proc = subprocess.Popen([WFC, *args], cwd=cwd or self.work, env=child_env,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            processes.append(proc)
            return proc

        def finish(proc, expected=0):
            out, err = proc.communicate(timeout=10)
            self.assertEqual(proc.returncode, expected, (proc.args, out, err))
            if expected:
                self.assertFalse(out)
                return err
            return json.loads(out)

        def until(predicate, message):
            deadline = time.monotonic() + 5
            while not predicate():
                self.assertLess(time.monotonic(), deadline, message)
                time.sleep(.02)

        def opened_by(proc, path):
            for fd in Path(f"/proc/{proc.pid}/fd").glob("*"):
                try:
                    if os.readlink(fd) == str(path):
                        return True
                except FileNotFoundError:
                    pass
            return False

        def locked(path):
            with path.open("rb") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return False
                except BlockingIOError:
                    return True

        def dispatch(scope):
            cwd = self.work / scope
            cwd.mkdir()
            row = finish(start("dispatch", "--host", "codex", "--task", str(self.work / "task"),
                               "--cwd", ".", "--report", "report", "--scope", scope,
                               "--launch-config", str(config), cwd=cwd))
            question = cwd / "question.json"
            until(question.exists, "public worker did not ask")
            return row, json.loads(question.read_bytes())

        other, other_question = dispatch("unrelated")
        for order in ("close", "ask"):
            with self.subTest(first=order):
                row, first = dispatch(order)
                dispatch_id, attempt = row["dispatch"], row["attempt"]
                lock = self.state / "close-locks" / f"{dispatch_id}.{attempt}"
                archive = self.state / "dispatches" / f"{dispatch_id}.{attempt}.closed"

                def ask(text, key, gate=None):
                    return start("ask", text, "--attempt", attempt, "--message-id", key,
                                 dispatch=dispatch_id, gate=gate)

                if order == "close":
                    close = start("close", dispatch_id, gate="close")
                    until((self.root / "close-ready").exists, "close did not reach the original session")
                    self.assertTrue(locked(lock))
                    pending = ask("Second question?", "second")
                    until(lambda: opened_by(pending, lock), "ask did not wait on the attempt lock")
                    self.assertEqual(finish(ask("First question?", "original"))["messageId"], first["messageId"])
                    self.assertFalse(archive.exists())
                    (self.root / "close-release").touch()
                    closed = finish(close)
                    self.assertIn("round is not running", finish(pending, expected=1))
                    accepted = []
                else:
                    pending = ask("Second question?", "second", gate="ask")
                    until((self.root / "ask-ready").exists, "ask did not sample the real tmux session")
                    self.assertTrue(locked(lock))
                    # The blocked tmux inspection leaves other scopes free to take messages.
                    answer = finish(start("wait", "--scope", "unrelated", "--timeout", "0.5"))
                    self.assertEqual([m["messageId"] for m in answer["messages"]],
                                     [other_question["messageId"]])
                    with (self.state / "messages.lock").open("rb") as inbox:
                        fcntl.flock(inbox, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        close = start("close", dispatch_id)
                        until(lambda: opened_by(close, lock), "close did not wait on the attempt lock")
                        (self.root / "ask-release").touch()
                        native = self.state / "tmux" / f"{dispatch_id}.lock"
                        until(lambda: not locked(native), "tmux inspection did not finish")
                        self.assertTrue(locked(lock))
                        self.assertIsNone(close.poll())
                        self.assertFalse(archive.exists())
                    accepted = [finish(pending)["messageId"]]
                    closed = finish(close)

                saved = json.loads(archive.read_bytes())
                self.assertEqual((saved["dispatch"], saved["attempt"], saved["round"]),
                                 (dispatch_id, attempt, row["round"]))
                self.assertEqual(saved["executionId"], attempt)
                self.assertEqual(saved["launchId"], Path(row["create"]["receipt"]).stem)
                self.assertEqual(closed["result"], "completed")
                for result in (saved, closed):
                    self.assertEqual(result["closeOperationId"], "close:" + attempt)
                    self.assertEqual(result["closeEffect"], "session_closed")
                    self.assertEqual(result["resourceCleanup"], "confirmed")
                batch = finish(start("wait", "--scope", order, "--timeout", "0"))
                self.assertEqual([m["messageId"] for m in batch["messages"]],
                                 [first["messageId"], *accepted])
                confirmed = finish(start("ack", first["messageId"]))
                self.assertEqual(finish(ask("First question?", "original")), {**confirmed, "duplicate": True})
                owner = self.call("inspect", dispatch=other["dispatch"], executionId=other["attempt"])
                self.assertTrue(owner["alive"] and owner["owned"] and owner["workerBound"], owner)

    def public_interrupt_fixture(self):
        worker = self.work / "interrupt-worker.py"
        worker.write_text(r"""import json, os, time, tty
from pathlib import Path

tty.setraw(0)
data = {key: os.environ[key] for key in ("WFC_DISPATCH", "WFC_ROUND", "WFC_ATTEMPT",
                                        "WFC_WORKER_CALLBACK", "WFC_REPORT")}
assert "WFC_OWNER_TOKEN" not in os.environ and "WFC_CONTROLLER_TOKEN" not in os.environ
attempt = data["WFC_ATTEMPT"]
temp = Path(attempt + ".tmp")
temp.write_text(json.dumps(data))
temp.rename(attempt + ".ready")
with open(attempt + ".keys", "wb", buffering=0) as keys:
    while byte := os.read(0, 1):
        keys.write(byte)
""")
        config = self.work / "interrupt-launch.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [sys.executable, str(worker)]}}}))
        (self.work / "task").write_text("interrupt this controlled worker")
        env = {**self.env, "WFC_STATE": str(self.state)}
        env.pop("WFC_OWNER_TOKEN", None)

        def cli(*args, code=0, **overrides):
            proc = subprocess.run([WFC, *args], cwd=self.work, env={**env, **overrides},
                                  capture_output=True, text=True, timeout=15)
            self.assertEqual(proc.returncode, code, (args, proc.stdout, proc.stderr))
            if code:
                self.assertFalse(proc.stdout)
                return proc.stderr
            return json.loads(proc.stdout)

        def dispatch(name, **credentials):
            record = cli("dispatch", "--host", "codex", "--scope", name, "--cwd", ".",
                         "--task", "task", "--report", name + ".md",
                         "--launch-config", str(config), **credentials)
            ready = self.work / (record["attempt"] + ".ready")
            keys = self.work / (record["attempt"] + ".keys")
            self.wait_interrupt_fixture(lambda: ready.exists() and keys.exists())
            return record, json.loads(ready.read_text()), keys

        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import json, os, subprocess, sys, time
from pathlib import Path

args = sys.argv[1:]
real = {TMUX!r}
work = Path({str(self.work)!r})
mode = os.environ.get("INTERRUPT_TEST_MODE")
def hold():
    (work / "at-interrupt-key").touch()
    deadline = time.monotonic() + 25
    while not (work / "allow-interrupt-key").exists():
        if time.monotonic() >= deadline:
            sys.exit(2)
        time.sleep(.02)

if mode == "before-backend-start" and "list-panes" in args:
    hold()
if mode == "fail-ordered-preflight" and "list-panes" in args:
    parent = (Path("/proc") / str(os.getppid()) / "cmdline").read_bytes()
    if b"--lifecycle-lock-fds" in parent.split(bytes([0])):
        (work / "ordered-preflight-refused").touch()
        sys.exit(7)
if mode and "if-shell" in args and any("send-keys -t " in a and " C-c" in a for a in args):
    if mode == "hold":
        links = []
        for fd in Path("/proc/self/fd").iterdir():
            try:
                links.append(os.readlink(fd))
            except FileNotFoundError:
                pass
        (work / "interrupt-client-fds").write_text(json.dumps(links))
        hold()
    elif mode == "lose-response":
        result = subprocess.run([real, *args], stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, close_fds=False)
        if result.returncode == 0:
            (work / "real-key-command-succeeded").touch()
        sys.exit(1)
    elif mode == "pause-server-queue":
        # This wait occurs inside the real server's guarded command queue.
        # Leave the real client connected when the private backend is killed.
        assert " ; send-keys -t " in args[-2]
        args[-2] = args[-2].replace(
            " ; send-keys -t ",
            " ; set-option -g @wfc-r20-queued 1 ; wait-for wfc-r20-release ; send-keys -t ",
            1,
        )
        (work / "interrupt-backend-pid").write_text(str(os.getppid()))
os.execv(real, [real, *args])
""")
        shim.chmod(0o700)
        return cli, dispatch, env

    def wait_interrupt_fixture(self, ready, timeout=6):
        deadline = time.monotonic() + timeout
        while not ready():
            self.assertLess(time.monotonic(), deadline, "interrupt fixture did not reach its boundary")
            time.sleep(.025)

    @contextmanager
    def gated_interrupt(self, env, dispatch, *, mode="hold", binary=WFC):
        env = {**env, "PATH": str(self.work) + ":" + env["PATH"], "INTERRUPT_TEST_MODE": mode}
        with subprocess.Popen([binary, "interrupt", dispatch], cwd=self.work, env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, start_new_session=True) as process:
            try:
                self.wait_interrupt_fixture((self.work / "at-interrupt-key").exists)
                yield process
            finally:
                (self.work / "allow-interrupt-key").touch()
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    raise

    def test_public_interrupt_fences_before_tmux_key(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        owner = cli("acquire", "--scope", "managed-interrupt", "--owner", "A")["token"]
        credentials = {"WFC_OWNER_TOKEN": owner}
        target, callback, keys = dispatch("managed-interrupt", **credentials)
        neighbor, _, other_keys = dispatch("neighbor-interrupt")
        identity = target["dispatch"], target["attempt"]
        pane = self.active_pane(identity[0])
        self.assertEqual(json.loads(cli("interrupt", identity[0], code=1))["error"], "stale_controller_owner")
        self.assertEqual(json.loads(cli(
            "interrupt", identity[0], code=1, **credentials,
            WFC_WORKER_CALLBACK=callback["WFC_WORKER_CALLBACK"],
        ))["error"], "worker_callback_only")

        bad_target = self.call("interrupt", dispatch=identity[0], executionId=identity[1],
                               launchId="f" * 32)
        self.assertEqual(bad_target["result"], "refused")
        self.assertEqual(keys.read_bytes(), b"")

        path = self.state / "dispatches" / f"{identity[0]}.json"
        Path(callback["WFC_REPORT"]).write_text("old writer's report")
        launch = {**env, **credentials, "INTERRUPT_TEST_MODE": "hold",
                  "PATH": str(self.work) + ":" + env["PATH"]}
        with subprocess.Popen([WFC, "interrupt", identity[0]], cwd=self.work, env=launch,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as process:
            try:
                self.wait_interrupt_fixture(lambda: (self.work / "at-interrupt-key").exists())
                saved = json.loads(path.read_text())
                audit, = saved["audit"]
                self.assertEqual((audit["result"], audit["ownerGeneration"], audit["scopeFence"]),
                                 ("pending", 1, True))
                self.assertEqual((audit["round"], audit["attempt"], audit["target"]["executionId"]),
                                 (1, identity[1], identity[1]))
                self.assertEqual(audit["target"]["receipt"], target["create"]["receipt"])
                self.assertEqual(saved["rounds"][0]["state"], "unknown")
                self.assertEqual(keys.read_bytes(), b"")
                callback_env = {key: value for key, value in callback.items() if key != "WFC_REPORT"}
                self.assertIn("execution was interrupted", cli(
                    "ask", "late", "--attempt", identity[1], code=1, **callback_env))
                self.assertIn("execution was interrupted", cli(
                    "done", "--report", callback["WFC_REPORT"], "--attempt", identity[1],
                    code=1, **callback_env))
            finally:
                (self.work / "allow-interrupt-key").touch()
                out, err = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, (out, err))
        result = json.loads(out)
        self.assertEqual((result["result"], result["audit"], result["attempt"]), ("accepted", 1, identity[1]))
        self.wait_interrupt_fixture(lambda: keys.read_bytes() == b"\x03")
        status = cli("status", "--dispatch", identity[0])["dispatches"][0]
        self.assertEqual((status["status"], status["rounds"][0]["state"], status["backendAlive"]),
                         ("unknown", "unknown", True))
        self.assertEqual((status["audit"][0]["result"], status["messages"]), ("accepted", []))
        self.assertEqual(self.active_pane(identity[0]), pane)
        repeat = json.loads(cli("interrupt", identity[0], code=1, **credentials))
        self.assertEqual((repeat["error"], repeat["audit"]), ("lifecycle_reconcile_required", 1))
        self.assertEqual(keys.read_bytes(), b"\x03")
        self.assertEqual(other_keys.read_bytes(), b"")
        closed = cli("close", identity[0], **credentials)
        self.assertEqual((closed["result"], closed["resourceCleanup"]), ("completed", "confirmed"))
        self.assertTrue(cli("status", "--dispatch", neighbor["dispatch"])["dispatches"][0]["backendAlive"])

    def test_public_interrupt_lost_confirmation_keeps_fence_and_refuses_foreign_writer(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        target, _, keys = dispatch("unclaimed-interrupt")
        foreign, _, foreign_keys = dispatch("foreign-interrupt")
        foreign_session = f"=wfc-{foreign['dispatch']}"
        subprocess.run(self.tmux + ["set-environment", "-t", foreign_session, "WFC_STATE", str(self.other)],
                       env=self.env, check=True)
        error = cli("interrupt", foreign["dispatch"], code=1)
        self.assertIn("live original tmux writer", error)
        foreign_record = self.state / "dispatches" / (foreign["dispatch"] + ".json")
        self.assertFalse(json.loads(foreign_record.read_text()).get("audit"))
        subprocess.run(self.tmux + ["has-session", "-t", foreign_session], env=self.env, check=True)
        self.assertEqual(foreign_keys.read_bytes(), b"")

        identity = target["dispatch"], target["attempt"]
        result = cli("interrupt", identity[0], INTERRUPT_TEST_MODE="lose-response",
                     PATH=str(self.work) + ":" + env["PATH"])
        self.assertTrue((self.work / "real-key-command-succeeded").exists())
        self.assertEqual((result["result"], result["audit"], result["attempt"]), ("uncertain", 1, identity[1]))
        self.wait_interrupt_fixture(lambda: keys.read_bytes() == b"\x03")
        repeated = json.loads(cli("interrupt", identity[0], code=1))
        self.assertEqual((repeated["error"], repeated["result"], repeated["audit"]),
                         ("lifecycle_reconcile_required", "uncertain", 1))
        status = cli("status", "--dispatch", identity[0])["dispatches"][0]
        self.assertEqual((status["status"], status["backendAlive"]), ("unknown", True))
        audit, = status["audit"]
        self.assertEqual((audit["scopeFence"], audit["ownerGeneration"], audit["result"]),
                         (True, None, "uncertain"))
        refused = json.loads(cli("close", identity[0], code=1))
        self.assertEqual((refused["error"], refused["audit"], refused["result"]),
                         ("lifecycle_reconcile_required", 1, "uncertain"))
        self.assertFalse((self.state / "dispatches" / f"{identity[0]}.{identity[1]}.closed").exists())
        self.active_pane(identity[0])
        self.assertEqual(keys.read_bytes(), b"\x03")
        self.assertEqual(foreign_keys.read_bytes(), b"")
        subprocess.run(self.tmux + ["has-session", "-t", foreign_session], env=self.env, check=True)

    def test_public_interrupt_holds_both_locks_after_controller_exit(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        scope = "interrupt-controller-exit"
        owner = cli("acquire", "--scope", scope, "--owner", "A")
        target, _, keys = dispatch(scope, WFC_OWNER_TOKEN=owner["token"])
        neighbor, _, other_keys = dispatch("interrupt-neighbor")
        dispatch_id, attempt = target["dispatch"], target["attempt"]
        scope_lock = self.state / ("relay-" + hashlib.sha256(scope.encode()).hexdigest() + ".lock")
        attempt_lock = self.state / "close-locks" / f"{dispatch_id}.{attempt}"
        path = self.state / "dispatches" / f"{dispatch_id}.json"
        pending = None
        try:
            with self.gated_interrupt({**env, "WFC_OWNER_TOKEN": owner["token"]}, dispatch_id) as process:
                os.killpg(process.pid, signal.SIGKILL)
                self.assertEqual(process.wait(timeout=5), -signal.SIGKILL)
                saved = path.read_bytes()
                audit, = json.loads(saved)["audit"]
                self.assertEqual((audit["id"], audit["result"], audit["ownerGeneration"]), (1, "pending", 1))
                links = json.loads((self.work / "interrupt-client-fds").read_text())
                for lock_path in (scope_lock, attempt_lock):
                    self.assertNotIn(str(lock_path), links)
                    with lock_path.open("rb") as lock:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                pending = subprocess.Popen([WFC, "takeover", "--scope", scope, "--owner", "B"],
                                           cwd=self.work, env=env, text=True,
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)

                def opened_scope():
                    for fd in Path(f"/proc/{pending.pid}/fd").iterdir():
                        try:
                            if os.readlink(fd) == str(scope_lock):
                                return True
                        except FileNotFoundError:
                            pass
                    return False

                self.wait_interrupt_fixture(opened_scope)
                self.assertIsNone(pending.poll())
                self.assertEqual(keys.read_bytes(), b"")
            out, err = pending.communicate(timeout=10)
            self.assertEqual(pending.returncode, 0, (out, err))
            current = json.loads(out)
        finally:
            if pending is not None:
                if pending.poll() is None:
                    pending.kill()
                pending.communicate(timeout=5)
        self.wait_interrupt_fixture(lambda: keys.read_bytes() == b"\x03")
        self.assertEqual((current["generation"], path.read_bytes()), (2, saved))
        proof_path = self.state / "dispatches" / f"{dispatch_id}.{attempt}.1.interrupt-order"
        proof = json.loads(proof_path.read_text())
        self.assertEqual((proof["result"], proof["order"]["ownerGeneration"],
                          proof["order"]["audit"], proof["order"]["target"]),
                         ("accepted", owner["generation"], 1, audit["target"]))
        self.assertEqual(proof["order"]["server"]["pane"], self.active_pane(dispatch_id))
        for operation in ("close", "interrupt"):
            blocked = json.loads(cli(operation, dispatch_id, code=1, WFC_OWNER_TOKEN=current["token"]))
            self.assertEqual((blocked["error"], blocked["audit"], blocked["result"]),
                             ("lifecycle_reconcile_required", 1, "pending"))
        self.assertFalse((self.state / "dispatches" / f"{dispatch_id}.{attempt}.closed").exists())
        self.active_pane(dispatch_id)
        self.active_pane(neighbor["dispatch"])
        self.assertEqual((keys.read_bytes(), other_keys.read_bytes()), (b"\x03", b""))
        print(json.dumps({"controllerExited": True, "bothLocksHeld": True, "tmuxInheritedLocks": False,
                          "takeoverGeneration": current["generation"], "audit": audit,
                          "orderingReceipt": proof}), flush=True)

    def test_public_interrupt_leaves_pending_when_owner_expires(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        scope = "interrupt-expiry"
        owner = cli("acquire", "--scope", scope, "--owner", "A")
        credentials = {"WFC_OWNER_TOKEN": owner["token"]}
        target, _, keys = dispatch(scope, **credentials)
        neighbor, _, other_keys = dispatch("expiry-neighbor")
        dispatch_id = target["dispatch"]
        path = self.state / "dispatches" / f"{dispatch_id}.json"

        cli("renew", "--scope", scope, "--lease", "5", **credentials)
        expires = json.loads((self.state / "controller-owners.json").read_text())[scope]["until"]
        with self.gated_interrupt({**env, **credentials}, dispatch_id) as process:
            saved = path.read_bytes()
            audit, = json.loads(saved)["audit"]
            self.assertEqual((audit["result"], audit["ownerGeneration"]), ("pending", owner["generation"]))
            self.assertEqual(keys.read_bytes(), b"")
            self.wait_interrupt_fixture(lambda: time.time() > expires, timeout=10)
            (self.work / "allow-interrupt-key").touch()
            out, err = process.communicate(timeout=10)
        self.assertEqual((process.returncode, out), (1, ""), err)
        stale = json.loads(err)
        self.assertEqual((stale["error"], stale["generation"], stale["active"]),
                         ("stale_controller_owner", owner["generation"], False))
        self.wait_interrupt_fixture(lambda: keys.read_bytes() == b"\x03")
        self.assertEqual(path.read_bytes(), saved)
        current = cli("takeover", "--scope", scope, "--owner", "B")
        again = json.loads(cli("interrupt", dispatch_id, code=1, WFC_OWNER_TOKEN=current["token"]))
        self.assertEqual((again["error"], again["audit"], again["result"]),
                         ("lifecycle_reconcile_required", 1, "pending"))
        self.active_pane(dispatch_id)
        self.active_pane(neighbor["dispatch"])
        self.assertEqual((keys.read_bytes(), other_keys.read_bytes()), (b"\x03", b""))
        print(json.dumps({"staleOwner": stale, "pendingUnchanged": True, "nextOwner": current["generation"]}),
              flush=True)

    def test_public_interrupt_backend_spawn_failure_is_definitely_not_sent(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        target, _, keys = dispatch("interrupt-no-spawn")
        neighbor, _, other_keys = dispatch("no-spawn-neighbor")
        dispatch_id, attempt = target["dispatch"], target["attempt"]
        binary = self.work / "controller-copy"
        shutil.copy2(WFC, binary)
        with self.gated_interrupt(env, dispatch_id, mode="before-backend-start", binary=binary) as process:
            # The original inspect backend is already running. Only the next
            # backend exec on the fixture copy can fail with EACCES.
            binary.chmod(0o600)
            (self.work / "allow-interrupt-key").touch()
            out, err = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, (out, err))
        result = json.loads(out)
        self.assertEqual((result["result"], result["attempt"], result["audit"]),
                         ("definitely-not-sent", attempt, 1))
        self.assertIn("start tmux backend", result["diagnostic"])
        record = self.state / "dispatches" / f"{dispatch_id}.json"
        audit, = json.loads(record.read_text())["audit"]
        self.assertEqual((audit["result"], audit["scopeFence"]), ("definitely-not-sent", True))
        self.assertTrue(audit["settledAt"])
        blocked = json.loads(cli("interrupt", dispatch_id, code=1))
        self.assertEqual((blocked["error"], blocked["audit"], blocked["result"]),
                         ("lifecycle_reconcile_required", 1, "definitely-not-sent"))
        self.active_pane(dispatch_id)
        self.active_pane(neighbor["dispatch"])
        self.assertEqual((keys.read_bytes(), other_keys.read_bytes()), (b"", b""))
        self.assertEqual(cli("close", dispatch_id)["closeEffect"], "session_closed")
        self.active_pane(neighbor["dispatch"])
        print(json.dumps({"definiteFailure": result, "targetKeys": 0, "neighborKeys": 0}), flush=True)

    def test_public_interrupt_preflight_zero_send_allows_takeover(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        scope = "interrupt-preflight"
        owner = cli("acquire", "--scope", scope, "--owner", "A")
        credentials = {"WFC_OWNER_TOKEN": owner["token"]}
        target, _, keys = dispatch(scope, **credentials)
        neighbor, _, other_keys = dispatch("interrupt-preflight-neighbor")
        result = cli("interrupt", target["dispatch"], **credentials,
                     PATH=str(self.work) + ":" + env["PATH"],
                     INTERRUPT_TEST_MODE="fail-ordered-preflight")
        self.assertTrue((self.work / "ordered-preflight-refused").exists())
        self.assertEqual((result["result"], result["audit"]), ("definitely-not-sent", 1))
        self.assertIn("tmux could not inspect", result["diagnostic"])
        self.assertFalse(list(self.state.glob("interrupt-order-*.json")))
        current = cli("takeover", "--scope", scope, "--owner", "B")
        self.assertEqual(current["generation"], 2)
        self.active_pane(target["dispatch"])
        self.active_pane(neighbor["dispatch"])
        self.assertEqual((keys.read_bytes(), other_keys.read_bytes()), (b"", b""))
        print(json.dumps({"backendPreflightFailed": True, "result": result,
                          "takeoverGeneration": current["generation"], "targetKeys": 0,
                          "neighborKeys": 0}), flush=True)

    def test_public_interrupt_blocks_owner_change_after_backend_dies_with_queued_key(self):
        cli, dispatch, env = self.public_interrupt_fixture()
        scope = "interrupt-queued"
        owner = cli("acquire", "--scope", scope, "--owner", "A")
        credentials = {"WFC_OWNER_TOKEN": owner["token"]}
        target, _, keys = dispatch(scope, **credentials)
        neighbor, _, other_keys = dispatch("interrupt-queued-neighbor")
        dispatch_id, attempt = target["dispatch"], target["attempt"]
        draft = self.state / "dispatches" / f"{dispatch_id}.json"
        pane = self.active_pane(dispatch_id)
        order_path = self.state / ("interrupt-order-" + hashlib.sha256(scope.encode()).hexdigest() + ".json")
        receipt = self.state / "dispatches" / f"{dispatch_id}.{attempt}.1.interrupt-order"
        owners = self.state / "controller-owners.json"
        original = owners.read_bytes()
        launch = {**env, **credentials, "PATH": str(self.work) + ":" + env["PATH"],
                  "INTERRUPT_TEST_MODE": "pause-server-queue"}
        blocked = []

        def assert_owner_blocked(action):
            refused = json.loads(cli(action, "--scope", scope, "--owner", "B", code=1))
            self.assertEqual((refused["error"], refused["operation"], refused["scope"],
                              refused["dispatch"], refused["attempt"], refused["audit"],
                              refused["requiresDecision"]),
                             ("lifecycle_reconcile_required", "interrupt", scope,
                              dispatch_id, attempt, 1, True))
            self.assertFalse(receipt.exists())
            self.assertEqual(owners.read_bytes(), original)
            blocked.append(refused)

        with subprocess.Popen([WFC, "interrupt", dispatch_id], cwd=self.work, env=launch,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as process:
            try:
                def server_has_queued_key():
                    result = subprocess.run(self.tmux + ["show-options", "-gqv", "@wfc-r20-queued"],
                                            env=self.env, check=True, text=True, capture_output=True, timeout=5)
                    return result.stdout.strip() == "1"

                self.wait_interrupt_fixture(server_has_queued_key)
                self.assertEqual(keys.read_bytes(), b"")
                event = json.loads(draft.read_text())["audit"][-1]
                self.assertEqual(event["result"], "pending")
                order = json.loads(order_path.read_text())
                self.assertEqual((order["audit"], order["ownerGeneration"], order["target"], order["server"]["pane"]),
                                 (event["id"], owner["generation"], event["target"], pane))
                backend_pid = int((self.work / "interrupt-backend-pid").read_text())
                os.kill(backend_pid, signal.SIGKILL)
                out, err = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 0, (out, err))
                response = json.loads(out)
                self.assertEqual((response["attempt"], response["audit"], response["result"]),
                                 (attempt, 1, "uncertain"))
                assert_owner_blocked("takeover")
                self.assertEqual(keys.read_bytes(), b"")
            finally:
                subprocess.run(self.tmux + ["wait-for", "-S", "wfc-r20-release"],
                               env=self.env, check=True, timeout=5)
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)

        self.wait_interrupt_fixture(lambda: keys.read_bytes() == b"\x03")
        assert_owner_blocked("takeover")
        same = cli("renew", "--scope", scope, **credentials)
        self.assertEqual(same["generation"], owner["generation"])
        self.assertEqual(cli("takeover", "--scope", neighbor["scope"], "--owner", "C")["generation"], 1)
        repeated = json.loads(cli("interrupt", dispatch_id, code=1, **credentials))
        self.assertEqual((repeated["error"], repeated["audit"], repeated["result"]),
                         ("lifecycle_reconcile_required", 1, "uncertain"))
        self.active_pane(dispatch_id)
        self.active_pane(neighbor["dispatch"])
        self.assertEqual((keys.read_bytes(), other_keys.read_bytes()), (b"\x03", b""))
        print(json.dumps({"realServerQueue": True, "backendKilled": True, "ownerBlocked": blocked,
                          "confirmationReceipts": 0, "targetKeys": 1, "neighborKeys": 0,
                          "retainedGeneration": same["generation"]}), flush=True)

    def test_lifecycle(self):
        # Keep the worker alive across Ctrl-C so close tests the active-session path.
        argv = ["/bin/sh", "-c", "trap '' INT; exec /bin/cat"]
        launched = self.call("create", **{**self.launch, "argv": argv})
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
