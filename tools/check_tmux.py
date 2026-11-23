"""Exercise the public CLI and private backend with a fresh tmux server and state roots."""
from contextlib import contextmanager
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import resource
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


def composer(mode, logfile, reply_worker=False):
    """Controlled terminal host: log actual input bytes and optionally drop Enter."""
    tty.setraw(0)
    draft = {"existing-scroll": "original draft ↑".encode(),
             "upstream-reply": b"original draft"}.get(mode, b"")
    unknown_footer = False
    enters = 0
    banner = "This chat uses A, but Codex now uses B, so this chat is read-only."
    refused = "This conversation is read-only or unavailable; no operation was sent."
    notice = banner if mode == "readonly" else refused if mode == "retry-refusal" else ""

    def draw():
        placeholder = "Ask Codex to do anything"
        shown = draft.decode("utf-8", errors="replace")
        prompt = "\x1b[1m›\x1b[0m" if reply_worker else "›"
        if not shown:
            shown = f"\x1b[2m{placeholder}\x1b[0m" if reply_worker else placeholder
        if unknown_footer:
            footer = "  unknown editor mode"
        elif mode == "upstream-reply":
            hint = "? for shortcuts" if not draft or draft == placeholder.encode() else ""
            footer = f"  {hint:<46}100% context left"
        else:
            footer = "  model · cwd"
        print(f"\x1b[2J\x1b[H{notice}\r\n{prompt} {shown}\r\n\r\n{footer}", end="", flush=True)

    wfc = os.getenv("WFC_BIN")
    attempt = os.getenv("WFC_ATTEMPT")
    accepted = False
    if reply_worker:
        receipt = subprocess.check_output([wfc, "ask", "One controlled question?",
                                           "--attempt", attempt, "--message-id", "reply-question"])
        Path("question.tmp").write_bytes(receipt)
        Path("question.tmp").rename("question.json")
    with open(logfile, "a", buffering=1) as log:
        print(json.dumps({"cwd": os.getcwd(), "label_arg": sys.argv[-1],
                          "worker": [os.getenv(k) for k in ("WFC_WORKER_CALLBACK", "WFC_OWNER_TOKEN",
                                                            "WFC_STATE", "WFC_DISPATCH", "CHECK_ENV")]}), file=log)
        draw()
        while True:
            if reply_worker:
                if Path("clear-draft").exists():
                    Path("clear-draft").unlink()
                    draft = b""
                    unknown_footer = False
                    draw()
                if mode == "upstream-reply" and Path("show-placeholder-draft").exists():
                    Path("show-placeholder-draft").unlink()
                    draft = b"Ask Codex to do anything"
                    draw()
                if mode == "upstream-reply" and Path("show-unknown-footer").exists():
                    Path("show-unknown-footer").unlink()
                    unknown_footer = True
                    draw()
                if notice == banner and Path("enable-input").exists():
                    notice = ""
                    draw()
                if (accepted or mode == "finish-alone") and Path("finish").exists():
                    report = Path(os.environ["WFC_REPORT"])
                    Path("finish").unlink()
                    report.write_text("original worker received the answer\n")
                    subprocess.run([wfc, "done", "--report", report, "--attempt", attempt,
                                    "--message-id", "reply-done"], check=True, stdout=subprocess.DEVNULL)
                if not select.select([0], [], [], .025)[0]:
                    continue
            b = os.read(0, 1)
            if not b:
                return
            print(json.dumps([b.hex(), time.monotonic()]), file=log)
            if b == b"\r" or (reply_worker and b == b"\n"):
                enters += 1
                if mode == "refusal":
                    notice = refused
                if mode == "retry-refusal":
                    notice = "" if enters == 1 else refused
                    if enters > 1:
                        draft = b""
                elif mode == "change":
                    draft = b"someone else's draft"
                elif mode == "changed-reply":
                    draft = draft.replace(b": a b", b": ab")
                elif mode == "scroll":
                    draft = "visible slice ↑".encode()
                elif mode != "always-drop" and (mode != "drop" or enters > 1):
                    if reply_worker:
                        with Path("accepted").open("ab") as delivered:
                            delivered.write(draft + b"\n")
                        accepted = True
                    draft = b""
            elif b != b"\x03":
                draft += b
            draw()


if sys.argv[1:2] in (["--composer"], ["--reply-worker"]):
    composer(sys.argv[2], sys.argv[3], reply_worker=sys.argv[1] == "--reply-worker")
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

    def resume_cli(self, *args, code=0, **env):
        result = subprocess.run([WFC, *args], cwd=self.work, text=True, capture_output=True,
                                env={**self.env, "WFC_STATE": str(self.state),
                                     "WFC_RESUME_CHECK_SECONDS": "0.25", **env}, timeout=20)
        if code is None:
            return result
        self.assertEqual(result.returncode, code, (args, result.stdout, result.stderr))
        if code:
            self.assertFalse(result.stdout)
            return result.stderr
        return json.loads(result.stdout)

    def resume_fixture(self, home_setting=None):
        if home_setting is None:
            self.codex_home = self.root / "codex home"
            self.codex_home.mkdir(exist_ok=True)
            home_setting = str(self.codex_home)
        else:
            self.codex_home = (self.work / home_setting).resolve()
        self.resume_config = self.work / "launch.json"
        worker = self.work / "resume-host.py"
        worker.write_text("""import json, os, sys, time
from pathlib import Path
attempt = os.environ['WFC_ATTEMPT']
data = {k: os.environ.get(k) for k in ('WFC_ATTEMPT', 'WFC_ROUND', 'WFC_TASK', 'WFC_REPORT',
                                      'WFC_DISPATCH', 'WFC_WORKER_CALLBACK', 'WFC_OWNER_TOKEN',
                                      'CODEX_HOME', 'VERSION')}
home = Path(os.environ['CODEX_HOME'])
data['codexHomeExisted'] = home.is_dir()
home.mkdir(parents=True, exist_ok=True)
data['argv'] = sys.argv[1:]
Path(attempt + '.tmp').write_text(json.dumps(data))
Path(attempt + '.tmp').rename(attempt + '.json')
print('› ready', flush=True)
deadline = time.monotonic() + 30
while time.monotonic() < deadline and not Path(attempt + '.quit').exists():
    time.sleep(.02)
""")
        self.resume_settings = {"env": {"CODEX_HOME": home_setting, "VERSION": "first"},
                                "hosts": {"codex": {"command": [sys.executable, str(worker)]}}}
        self.resume_config.write_text(json.dumps(self.resume_settings))
        (self.work / "task1").write_text("controlled task 1")
        (self.work / "task2").write_text("controlled task 2")
        owner = self.resume_cli("owner", "acquire", "--scope", "resume", "--owner", "one",
                                "--token", "owner-one", "--lease", "180")
        self.assertEqual(owner["generation"], 1)
        row = self.resume_cli("dispatch", "--host", "codex", "--scope", "resume", "--role", "worker",
                              "--cwd", ".", "--task", "task1", "--report", "reports/r1.md", "--model",
                              "test-model", "--effort", "high", "--network", "--launch-config",
                              str(self.resume_config), WFC_OWNER_TOKEN="owner-one")
        return row, self.resumed_host(row["attempt"])

    def resumed_host(self, attempt):
        path = self.work / f"{attempt}.json"
        deadline = time.monotonic() + 5
        while not path.is_file():
            self.assertLess(time.monotonic(), deadline, "worker launch was not observed")
            time.sleep(.025)
        return json.loads(path.read_text())

    def resume_status(self, dispatch):
        return self.resume_cli("status", "--dispatch", dispatch)["dispatches"][0]

    def end_resume_host(self, dispatch, attempt):
        (self.work / f"{attempt}.quit").touch()
        deadline = time.monotonic() + 5
        while True:
            row = self.resume_status(dispatch)
            if row["backendAlive"] is False and row["worker"]["state"] == "exited":
                return
            self.assertLess(time.monotonic(), deadline, "original worker exit was not verified")
            time.sleep(.025)

    def rollout(self, session, text, source="cli", *, folder="2026/11/23"):
        directory = self.codex_home / "sessions" / folder
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / f"rollout-2026-11-23T10-00-01-{session}.jsonl"
        events = [
            {"type": "session_meta", "payload": {"id": session, "cwd": str(self.work.resolve()), "source": source}},
            {"type": "response_item", "payload": {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "<environment_context>fixture</environment_context>"}]}},
            {"type": "event_msg", "payload": {"type": "user_message", "message": text}},
        ]
        path.write_text("".join(json.dumps(event) + "\n" for event in events))
        return path

    def resume_callback(self, worker, command, *args, code=0, exact=True):
        env = {key: worker[key] for key in ("WFC_DISPATCH", "WFC_ROUND", "WFC_ATTEMPT", "WFC_WORKER_CALLBACK")}
        if exact:
            args += ("--attempt", env["WFC_ATTEMPT"])
        return self.resume_cli(command, *args, code=code, **env)

    def test_public_resume_preserves_conversation_rounds_and_callback_fences(self):
        original, first = self.resume_fixture()
        dispatch, attempt = original["dispatch"], original["attempt"]
        request = ("resume", dispatch)
        session = "11111111-2222-4333-8444-555555555555"
        brief = first["argv"][-1]
        self.rollout("21111111-2222-4333-8444-555555555555", brief, {"subagent": {"thread_spawn": {}}})
        self.rollout("31111111-2222-4333-8444-555555555555", "quoted: " + brief)
        self.assertIn("still running", self.resume_cli(*request, code=1, WFC_OWNER_TOKEN="owner-one"))
        self.end_resume_host(dispatch, attempt)
        self.assertIn("cannot verify the original Codex conversation",
                      self.resume_cli(*request, code=1, WFC_OWNER_TOKEN="owner-one"))
        self.rollout(session, brief)
        duplicate = self.rollout(session, "unrelated input", folder="2026/11/22")
        self.assertIn("more than one rollout uses the original session ID",
                      self.resume_cli(*request, code=1, WFC_OWNER_TOKEN="owner-one"))
        duplicate.unlink()
        unchanged = self.resume_status(dispatch)
        self.assertEqual(unchanged["attempt"], attempt)
        self.assertFalse(unchanged.get("audit"))
        wrong_home = self.resume_settings["env"].copy()
        wrong_home["CODEX_HOME"] = str(self.root / "elsewhere")
        self.resume_config.write_text(json.dumps({**self.resume_settings, "env": wrong_home}))
        self.assertIn("differs from the original dispatch",
                      self.resume_cli(*request, code=1, WFC_OWNER_TOKEN="owner-one"))
        self.resume_settings["env"]["VERSION"] = "reloaded"
        self.resume_config.write_text(json.dumps(self.resume_settings))
        self.resume_cli("owner", "takeover", "--scope", "resume", "--owner", "two",
                        "--token", "owner-two", "--lease", "180")
        self.assertIn("stale_controller_owner",
                      self.resume_cli(*request, code=1, WFC_OWNER_TOKEN="owner-one"))
        self.assertIn("unfinished", self.resume_cli(*request, "--task", "task2", "--report",
                                                     "reports/r2.md", code=1, WFC_OWNER_TOKEN="owner-two"))
        commands = [WFC, *request]
        env = {**self.env, "WFC_STATE": str(self.state), "WFC_OWNER_TOKEN": "owner-two",
               "WFC_RESUME_CHECK_SECONDS": "0.25"}
        with subprocess.Popen(commands, cwd=self.work, env=env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE) as one:
            two = subprocess.run(commands, cwd=self.work, env=env, text=True, capture_output=True, timeout=20)
            out, err = one.communicate(timeout=20)
        self.assertEqual(sorted((one.returncode, two.returncode)), [0, 1], (err, two.stderr))
        resumed = json.loads(out if one.returncode == 0 else two.stdout)
        self.assertEqual((resumed["session"], resumed["round"], resumed["launchResult"]),
                         (session, 1, "accepted"))
        self.assertNotEqual(resumed["attempt"], attempt)
        second = self.resumed_host(resumed["attempt"])
        self.assertEqual(second["argv"][0], "resume")
        self.assertEqual(second["argv"][-2], session)
        self.assertIn("test-model", second["argv"])
        self.assertIn("model_reasoning_effort=high", second["argv"])
        self.assertIn("sandbox_workspace_write.network_access=true", second["argv"])
        self.assertEqual(second["VERSION"], "reloaded")
        self.assertIsNone(second["WFC_OWNER_TOKEN"])
        self.assertTrue(second["WFC_WORKER_CALLBACK"] and first["WFC_WORKER_CALLBACK"] != second["WFC_WORKER_CALLBACK"])
        for key in ("WFC_TASK", "WFC_REPORT", "WFC_ROUND", "WFC_DISPATCH"):
            self.assertEqual(second[key], first[key])
        report = Path(second["WFC_REPORT"])
        report.write_text("round 1")
        self.assertIn("attempt is no longer accepted",
                      self.resume_callback(first, "done", "--report", str(report), code=1))
        self.assertIn("attempt is no longer accepted", self.resume_callback(first, "ask", "old", code=1))
        self.assertIn("ambiguous report", self.resume_callback(second, "done", "--report", str(report), code=1, exact=False))
        question = self.resume_callback(second, "ask", "current question")
        done = self.resume_callback(second, "done", "--report", str(report))
        self.end_resume_host(dispatch, resumed["attempt"])
        self.assertIn("current task is complete", self.resume_cli(*request, code=1, WFC_OWNER_TOKEN="owner-two"))
        self.assertIn("already used", self.resume_cli(*request, "--task", "task2", "--report", str(report),
                                                        code=1, WFC_OWNER_TOKEN="owner-two"))
        self.assertIn("original report directory", self.resume_cli(*request, "--task", "task2", "--report",
                                                                     "r2.md", code=1, WFC_OWNER_TOKEN="owner-two"))
        later = self.resume_cli(*request, "--task", "task2", "--report", "reports/r2.md", WFC_OWNER_TOKEN="owner-two")
        third = self.resumed_host(later["attempt"])
        self.assertEqual((later["session"], later["round"], third["WFC_ROUND"]), (session, 2, "2"))
        self.assertEqual(Path(third["WFC_TASK"]).name, "task2")
        self.assertEqual(Path(third["WFC_REPORT"]).name, "r2.md")
        row = self.resume_status(dispatch)
        self.assertNotIn("workerCallbackHash", row)
        self.assertEqual([(r["round"], r["state"]) for r in row["rounds"]],
                         [(1, "failed"), (1, "completed"), (2, "running")])
        self.assertEqual([m["messageId"] for m in row["messages"]], [question["messageId"], done["messageId"]])
        self.assertEqual([(a["fromAttempt"], a["toAttempt"], a["ownerGeneration"]) for a in row["audit"]],
                         [(attempt, resumed["attempt"], 2), (resumed["attempt"], later["attempt"], 2)])

    def test_public_resume_relative_codex_home_created_after_dispatch(self):
        hop = self.root / "home hop"
        hop.mkdir()
        relative = "../home hop/../codex home"
        original, first = self.resume_fixture(relative)
        dispatch, attempt = original["dispatch"], original["attempt"]
        session = "11111111-2222-4333-8444-555555555555"
        owner = {"WFC_OWNER_TOKEN": "owner-one"}
        self.assertEqual(first["CODEX_HOME"], relative)
        self.assertFalse(first["codexHomeExisted"])
        self.assertEqual(original["codexHome"], str(self.work / relative))
        self.end_resume_host(dispatch, attempt)

        self.codex_home.rmdir()
        missing = json.loads(self.resume_cli("resume", dispatch, code=1, **owner))
        self.assertEqual(missing["error"], "lifecycle_reconcile_required")
        self.rollout("21111111-2222-4333-8444-555555555555", "quoted: " + first["argv"][-1])
        self.assertIn("cannot verify the original Codex conversation",
                      self.resume_cli("resume", dispatch, code=1, **owner))
        self.rollout(session, first["argv"][-1])

        # The final directory existing is not proof if an earlier component
        # of the saved spelling disappeared before a "..".
        hop.rmdir()
        existing_home = {**self.resume_settings["env"], "CODEX_HOME": str(self.codex_home)}
        self.resume_config.write_text(json.dumps({**self.resume_settings, "env": existing_home}))
        refused = json.loads(self.resume_cli("resume", dispatch, code=1, **owner))
        self.assertEqual(refused["error"], "lifecycle_reconcile_required")
        self.assertIn("configured Codex home differs", refused["diagnostic"])
        self.resume_config.write_text(json.dumps(self.resume_settings))
        hop.mkdir()

        # A symlink in a component followed by ".." can change its meaning.
        redirect = self.root / "redirect"
        (redirect / hop.name).mkdir(parents=True)
        redirected_home = redirect / self.codex_home.name
        self.codex_home.rename(redirected_home)
        hop.rmdir()
        hop.symlink_to(redirect / hop.name, target_is_directory=True)
        refused = json.loads(self.resume_cli("resume", dispatch, code=1, **owner))
        self.assertIn("configured Codex home differs", refused["diagnostic"])
        self.assertEqual(refused["error"], "lifecycle_reconcile_required")
        row = self.resume_status(dispatch)
        self.assertEqual(row["attempt"], attempt)
        self.assertFalse(row.get("audit"))
        hop.unlink()
        hop.mkdir()
        redirected_home.rename(self.codex_home)

        resumed = self.resume_cli("resume", dispatch, **owner)
        self.assertEqual((resumed["session"], resumed["round"]), (session, 1))
        self.assertNotEqual(resumed["attempt"], attempt)
        second = self.resumed_host(resumed["attempt"])
        self.assertEqual(second["argv"][-2], session)
        self.assertEqual(second["CODEX_HOME"], str(self.codex_home))
        self.assertEqual(second["WFC_REPORT"], first["WFC_REPORT"])
        self.end_resume_host(dispatch, resumed["attempt"])

    def test_public_resume_dot_codex_home_created_after_dispatch(self):
        relative = "./codex-data"
        original, first = self.resume_fixture(relative)
        dispatch, attempt = original["dispatch"], original["attempt"]
        session = "11111111-2222-4333-8444-555555555555"
        owner = {"WFC_OWNER_TOKEN": "owner-one"}
        self.assertFalse(first["codexHomeExisted"])
        self.assertEqual(first["CODEX_HOME"], relative)
        self.assertEqual(original["codexHome"], f"{self.work}/{relative}")
        self.end_resume_host(dispatch, attempt)
        rollout = self.rollout(session, first["argv"][-1])

        # An identical rollout in a different, existing home does not prove
        # that the home belongs to the original dispatch.
        other = self.root / "other Codex home"
        copied = other / rollout.relative_to(self.codex_home)
        copied.parent.mkdir(parents=True)
        shutil.copyfile(rollout, copied)
        different_home = {**self.resume_settings["env"], "CODEX_HOME": str(other)}
        self.resume_config.write_text(json.dumps({**self.resume_settings, "env": different_home}))
        refused = json.loads(self.resume_cli("resume", dispatch, code=1, **owner))
        self.assertEqual(refused["error"], "lifecycle_reconcile_required")
        self.assertIn("configured Codex home differs", refused["diagnostic"])
        row = self.resume_status(dispatch)
        self.assertEqual(row["attempt"], attempt)
        self.assertFalse(row.get("audit"))

        self.resume_config.write_text(json.dumps(self.resume_settings))
        resumed = self.resume_cli("resume", dispatch, **owner)
        self.assertEqual((resumed["session"], resumed["round"], resumed["launchResult"]),
                         (session, 1, "accepted"))
        self.assertNotEqual(resumed["attempt"], attempt)
        second = self.resumed_host(resumed["attempt"])
        self.assertEqual((second["argv"][0], second["argv"][-2]), ("resume", session))
        self.assertEqual(second["CODEX_HOME"], str(self.codex_home))
        self.assertEqual(second["WFC_REPORT"], first["WFC_REPORT"])
        self.assertIn("attempt is no longer accepted",
                      self.resume_callback(first, "ask", "stale", code=1))
        wrong_callback = {**second, "WFC_WORKER_CALLBACK": first["WFC_WORKER_CALLBACK"]}
        self.assertIn("invalid_worker_callback",
                      self.resume_callback(wrong_callback, "ask", "wrong worker", code=1))
        self.end_resume_host(dispatch, resumed["attempt"])

    def test_public_resume_records_definite_spawn_failure(self):
        original, first = self.resume_fixture()
        dispatch, attempt = original["dispatch"], original["attempt"]
        session = "11111111-2222-4333-8444-555555555555"
        self.rollout(session, first["argv"][-1])
        self.end_resume_host(dispatch, attempt)
        command = self.resume_settings["hosts"]["codex"]["command"]
        self.resume_settings["hosts"]["codex"]["command"] = [str(self.work / "missing-executable")]
        self.resume_config.write_text(json.dumps(self.resume_settings))
        error = json.loads(self.resume_cli("resume", dispatch, code=1, WFC_OWNER_TOKEN="owner-one"))
        self.assertEqual((error["result"], error["launchResult"], error["category"]),
                         ("definitely-not-sent", "definitely-not-sent", "executable_missing"))
        row = self.resume_status(dispatch)
        self.assertNotEqual(row["attempt"], attempt)
        self.assertEqual(row["state"], "unknown")
        self.assertEqual(row["audit"][0]["launchResult"], "definitely-not-sent")
        self.resume_settings["hosts"]["codex"]["command"] = command
        self.resume_config.write_text(json.dumps(self.resume_settings))
        resumed = self.resume_cli("resume", dispatch, WFC_OWNER_TOKEN="owner-one")
        self.assertEqual((resumed["round"], resumed["session"]), (1, session))
        self.assertNotIn(resumed["attempt"], (attempt, row["attempt"]))
        self.assertEqual(self.resume_status(dispatch)["audit"][1]["fromAttempt"], row["attempt"])

    def test_public_resume_rechecks_completion_before_reserving(self):
        original, first = self.resume_fixture()
        dispatch, attempt = original["dispatch"], original["attempt"]
        self.rollout("11111111-2222-4333-8444-555555555555", first["argv"][-1])
        self.end_resume_host(dispatch, attempt)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, sys, time
from pathlib import Path
args = sys.argv[1:]
if "list-panes" in args and {f'=wfc-{dispatch}:'!r} in args:
    Path("inspecting").touch()
    deadline = time.monotonic() + 8
    while not Path("checked").exists() and time.monotonic() < deadline:
        time.sleep(.025)
os.execv({TMUX!r}, [{TMUX!r}, *args])
""")
        shim.chmod(0o700)
        env = {**self.env, "PATH": str(self.work), "WFC_OWNER_TOKEN": "owner-one",
               "WFC_STATE": str(self.state)}
        with subprocess.Popen([WFC, "resume", dispatch], cwd=self.work, env=env,
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as resume:
            try:
                deadline = time.monotonic() + 5
                while not (self.work / "inspecting").exists():
                    self.assertLess(time.monotonic(), deadline, "resume did not reach inspection")
                    time.sleep(.025)
                report = Path(first["WFC_REPORT"])
                report.write_text("done during resume")
                done = self.resume_callback(first, "done", "--report", str(report))
            finally:
                (self.work / "checked").touch()
            out, err = resume.communicate(timeout=15)
        self.assertEqual((resume.returncode, out), (1, ""), err)
        self.assertIn("current task is complete", err)
        row = self.resume_status(dispatch)
        self.assertEqual(row["attempt"], attempt)
        self.assertFalse(row.get("audit"))
        self.assertEqual(row["rounds"][0]["result"]["messageId"], done["messageId"])

    def test_public_resume_fences_pending_and_lost_launch_responses(self):
        for mode in ("pending", "uncertain"):
            with self.subTest(mode=mode):
                self.verify_resume_lost_launch(mode)

    def verify_resume_lost_launch(self, mode):
        self.state = self.root / mode
        original, first = self.resume_fixture()
        dispatch, attempt = original["dispatch"], original["attempt"]
        self.rollout("11111111-2222-4333-8444-555555555555", first["argv"][-1])
        self.end_resume_host(dispatch, attempt)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, signal, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
if "new-session" in args:
    subprocess.run([{TMUX!r}, *args], check=True)
    backend = os.getppid()
    if {mode!r} == "pending":
        status = Path(f"/proc/{{backend}}/status").read_text().splitlines()
        router = int(next(line.split()[1] for line in status if line.startswith("PPid:")))
        os.kill(router, signal.SIGKILL)
    os.kill(backend, signal.SIGKILL)
    sys.exit(0)
os.execv({TMUX!r}, [{TMUX!r}, *args])
""")
        shim.chmod(0o700)
        owner = {"WFC_OWNER_TOKEN": "owner-one"}
        failed = self.resume_cli("resume", dispatch, code=None, PATH=str(self.work), **owner)
        self.assertEqual(failed.returncode, -signal.SIGKILL if mode == "pending" else 1, failed.stderr)
        row = self.resume_status(dispatch)
        self.assertEqual(row["audit"][0]["result"], mode)
        self.assertEqual(row["state"], "unknown")
        self.assertNotEqual(row["attempt"], attempt)
        new = self.resumed_host(row["attempt"])
        self.assertIn("attempt is no longer accepted", self.resume_callback(first, "ask", "old", code=1))
        if mode == "pending":
            question = self.resume_callback(new, "ask", "new pending attempt")
            self.assertEqual(question["state"], "unsent")
            report = Path(new["WFC_REPORT"])
            report.write_text("done while the launch receipt was pending")
            self.resume_callback(new, "done", "--report", str(report))
            active = self.resume_cli("status", "--active")
            self.assertEqual([(r["dispatch"], r["state"]) for r in active["dispatches"]],
                             [(dispatch, "done")])
            self.assertEqual(active["reclaimable"], [])
        pane = self.active_pane(dispatch)
        diagnostic = "resume audit 1 is unresolved"
        self.assertIn(diagnostic, self.resume_cli("close", dispatch, code=1, **owner))
        reclaimed = self.resume_cli("reclaim", "--dispatch", dispatch, "--yes", code=None, **owner)
        self.assertEqual(reclaimed.returncode, 1, reclaimed.stderr)
        item, = json.loads(reclaimed.stdout)["sessions"]
        self.assertEqual((item["result"], item["reclaimable"]), ("failed", False))
        self.assertIn(diagnostic, item["error"])
        self.assertEqual(self.active_pane(dispatch), pane)
        self.assertFalse(any(m["type"] == "reclaimed" for m in self.resume_status(dispatch)["messages"]))
        self.end_resume_host(dispatch, row["attempt"])
        again = json.loads(self.resume_cli("resume", dispatch, code=1, **owner))
        self.assertEqual((again["error"], again["audit"], again["attempt"]),
                         ("lifecycle_reconcile_required", 1, row["attempt"]))
        self.assertEqual(len(self.resume_status(dispatch)["audit"]), 1)
        shim.unlink()

    def test_public_resume_after_reclaim_preserves_history(self):
        original, first = self.resume_fixture()
        dispatch, attempt = original["dispatch"], original["attempt"]
        owner = {"WFC_OWNER_TOKEN": "owner-one"}
        session = "11111111-2222-4333-8444-555555555555"
        self.rollout(session, first["argv"][-1])
        report = Path(first["WFC_REPORT"])
        report.write_text("original round report")
        done = self.resume_callback(first, "done", "--report", str(report))

        # Keep the session available for reclaim after the original worker exits.
        pane = self.active_pane(dispatch)
        subprocess.run(self.tmux + ["new-window", "-dt", f"=wfc-{dispatch}:", "/bin/cat"],
                       env=self.env, check=True)
        (self.work / f"{attempt}.quit").touch()
        deadline = time.monotonic() + 5
        while True:
            row = self.resume_status(dispatch)
            panes = subprocess.check_output(self.tmux + ["list-panes", "-a", "-F", "#{pane_id}"],
                                            env=self.env, text=True).splitlines()
            if (row["backendAlive"] is True and row["worker"]["state"] == "exited"
                    and pane not in panes):
                break
            self.assertLess(time.monotonic(), deadline, row)
            time.sleep(.025)
        item, = self.resume_cli("reclaim", "--dispatch", dispatch, "--yes", **owner)["sessions"]
        self.assertEqual(item["result"], "reclaimed")
        row = self.resume_status(dispatch)
        self.assertEqual((row["state"], row["status"], row["backendAlive"], row["closeEffect"]),
                         ("done", "reclaimed", False, "worker_exited"))
        event, = [m for m in row["messages"] if m["type"] == "reclaimed"]
        self.assertEqual((event["id"], event["attempt"], event["round"], event["to"]),
                         (item["message"], attempt, 1, "worker"))
        self.assertIn("current task is complete", self.resume_cli("resume", dispatch, code=1, **owner))
        resumed = self.resume_cli("resume", dispatch, "--task", "task2", "--report", "reports/r2.md", **owner)
        self.assertEqual((resumed["session"], resumed["round"], resumed["launchResult"]), (session, 2, "accepted"))
        self.addCleanup(self.end_resume_host, dispatch, resumed["attempt"])
        self.assertNotEqual(resumed["attempt"], attempt)
        current = self.resumed_host(resumed["attempt"])
        self.assertEqual((current["argv"][-2], current["WFC_ROUND"], current["WFC_ATTEMPT"]),
                         (session, "2", resumed["attempt"]))
        self.assertTrue(first["WFC_WORKER_CALLBACK"] and current["WFC_WORKER_CALLBACK"]
                        and first["WFC_WORKER_CALLBACK"] != current["WFC_WORKER_CALLBACK"])
        current_report = Path(current["WFC_REPORT"])
        self.assertEqual(current_report, self.work / "reports" / "r2.md")
        current_report.write_text("unsubmitted second round report")
        ledger = self.state / "messages.json"
        assignment = self.state / "dispatches" / f"{dispatch}.json"
        before = (ledger.read_bytes(), assignment.read_bytes())
        cases = (
            ("round", first, report, f"Error: round mismatch for {dispatch}"),
            ("attempt", {**current, "WFC_ATTEMPT": attempt}, current_report,
             f"Error: attempt is no longer accepted for {dispatch}"),
            ("callback", {**current, "WFC_WORKER_CALLBACK": first["WFC_WORKER_CALLBACK"]}, current_report,
             {"error": "invalid_worker_callback", "operation": "done"}),
        )
        for boundary, worker, candidate_report, expected in cases:
            with self.subTest(boundary=boundary):
                error = self.resume_callback(worker, "done", "--report", str(candidate_report), code=1)
                self.assertEqual(json.loads(error) if boundary == "callback" else error.strip(), expected)
                self.assertEqual((ledger.read_bytes(), assignment.read_bytes()), before)
        row = self.resume_status(dispatch)
        self.assertNotIn("reclaimed", row)
        self.assertEqual([(m["id"], m["messageId"]) for m in row["messages"]],
                         [(done["message"], done["messageId"]), (event["id"], event["messageId"])])
        self.assertEqual([(r["round"], r["attempt"], r["state"]) for r in row["rounds"]],
                         [(1, attempt, "completed"), (2, resumed["attempt"], "running")])
        self.assertEqual(report.read_text(), "original round report")
        print(json.dumps({"callbackRejections": [boundary for boundary, *_ in cases],
                          "dispatch": dispatch, "round": row["round"], "attempt": row["attempt"],
                          "history": [{"id": m["id"], "messageId": m["messageId"]} for m in row["messages"]],
                          "ledgerAndAssignmentUnchanged": True}), flush=True)

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
        with read_only_tree(history):
            replay = data(*close, state=history)
            for field in ("dispatch", "round", "attempt", "result", "closeOperationId", "closeEffect"):
                self.assertEqual(replay[field], first[field])
            self.assertEqual(replay["diagnostic"], "session was already gone")
            self.assertEqual(archive.read_bytes(), saved)
            self.assertFalse(list(history.glob("relay-*.lock")))

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
                          "ambiguousArchiveRefused": True, "managedReplayRequiresLock": True,
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

    def reclaim_cli(self, *args, code=0, file_size_limit=None, **env):
        def limit_file_size():
            resource.setrlimit(resource.RLIMIT_FSIZE, (file_size_limit, file_size_limit))

        result = subprocess.run([WFC, *args], cwd=self.work, text=True, capture_output=True,
                                preexec_fn=limit_file_size if file_size_limit is not None else None,
                                env={**self.env, "WFC_STATE": str(self.state), **env}, timeout=25)
        self.assertEqual(result.returncode, code, result)
        return result

    def reclaim_dispatch(self, label, finished=False, scope=None, token=None, question=False):
        (self.work / "task").write_text("private reclaim test")
        report = self.work / f"{label}.md"
        config = self.work / f"{label}.json"
        script = """
import os, subprocess, sys, time
from pathlib import Path
if sys.argv[2] == "True":
    subprocess.run([os.environ["WFC_BIN"], "ask", "Continue?", "--options", "yes,no",
                    "--attempt", os.environ["WFC_ATTEMPT"]], check=True, timeout=5)
if sys.argv[1] == "True":
    Path(os.environ["WFC_REPORT"]).write_text("saved fixture report")
    subprocess.run([os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                    "--attempt", os.environ["WFC_ATTEMPT"]], check=True, timeout=5)
time.sleep(120)
"""
        config.write_text(json.dumps({"hosts": {"codex": {
            "command": [sys.executable, "-c", script, str(finished), str(question)]
        }}}))
        authorization = [] if scope is None else ["--scope", scope, "--owner-token", token]
        result = self.reclaim_cli("dispatch", "--host", "codex", "--cwd", ".",
                                  "--task", "task", "--label", label, "--report", str(report),
                                  "--launch-config", str(config), *authorization)
        row = json.loads(result.stdout)
        if finished:
            deadline = time.monotonic() + 10
            while self.reclaim_status(row)["state"] != "done":
                self.assertLess(time.monotonic(), deadline, "worker did not publish done")
                time.sleep(.025)
        return row

    def reclaim_status(self, row):
        return json.loads(self.reclaim_cli("status", "--dispatch", row["dispatch"]).stdout)["dispatches"][0]

    def test_public_reclaim_preview_mixed_results_and_history(self):
        checkout = self.work / "checkout-marker"
        checkout.write_text("private workspace contents")
        finished = self.reclaim_dispatch("finished", finished=True, question=True)
        unfinished = self.reclaim_dispatch("unfinished")
        gone = self.reclaim_dispatch("gone", finished=True)
        refused = self.reclaim_dispatch("refused", finished=True)
        blocked = self.reclaim_dispatch("blocked")
        self.reclaim_cli("close", gone["dispatch"])
        self.other.mkdir()
        subprocess.run(self.tmux + ["set-environment", "-t", "=" + refused["tmux"],
                                   "WFC_STATE", str(self.other)], env=self.env, check=True)
        subprocess.run(self.tmux + ["link-window", "-s", "sibling:0", "-t",
                                   blocked["tmux"] + ":9"], env=self.env, check=True)

        def snapshot_state():
            return {p.relative_to(self.state): p.read_bytes()
                    for p in self.state.rglob("*") if p.is_file()}

        before = snapshot_state()
        preview = self.reclaim_cli("reclaim")
        self.assertFalse(preview.stderr)
        self.assertEqual([(r["dispatch"], r["result"]) for r in json.loads(preview.stdout)["sessions"]],
                         [(finished["dispatch"], "listed"), (refused["dispatch"], "refused")])
        named = self.reclaim_cli("reclaim", "--dispatch", unfinished["dispatch"],
                                "--dispatch", unfinished["dispatch"], "--dispatch", refused["dispatch"])
        self.assertFalse(named.stderr)
        named_items = json.loads(named.stdout)["sessions"]
        self.assertEqual([(r["result"], r["reclaimable"]) for r in named_items],
                         [("listed", False), ("refused", True)])
        rejected = self.reclaim_cli("reclaim", "--yes", "--dispatch", finished["dispatch"],
                                   "--dispatch", "d-000000ff", code=1)
        self.assertFalse(rejected.stdout)
        self.assertIn("unknown dispatch d-000000ff", rejected.stderr)
        self.assertEqual(snapshot_state(), before)

        targets = (gone, refused, blocked, finished, unfinished, finished)
        options = [part for row in targets for part in ("--dispatch", row["dispatch"])]
        mixed = self.reclaim_cli("reclaim", "--yes", *options, code=1)
        items = json.loads(mixed.stdout)["sessions"]
        self.assertEqual([r["result"] for r in items],
                         ["gone", "refused", "failed", "reclaimed", "reclaimed"])
        self.assertIn(refused["dispatch"], mixed.stderr)
        self.assertIn(blocked["dispatch"], mixed.stderr)
        self.assertNotIn("message", items[0])
        for item, row in zip(items[-2:], (finished, unfinished)):
            status = self.reclaim_status(row)
            self.assertEqual(status["status"], "reclaimed")
            self.assertFalse(status["backendAlive"])
            event, = [m for m in status["messages"] if m["type"] == "reclaimed"]
            self.assertEqual((event["id"], event["attempt"], event["to"], event["tmux"]),
                             (item["message"], row["attempt"], "worker", row["tmux"]))
        status = self.reclaim_status(finished)
        self.assertEqual(status["state"], "done")
        self.assertEqual(status["rounds"][0]["state"], "completed")
        self.assertEqual((self.work / "finished.md").read_text(), "saved fixture report")
        self.assertEqual(checkout.read_text(), "private workspace contents")
        claimed = self.reclaim_cli("wait", "--dispatch", finished["dispatch"], "--timeout", "0")
        question, done = json.loads(claimed.stdout)["messages"]
        self.assertEqual([question["type"], done["type"]], ["question", "done"])
        event, = [m for m in status["messages"] if m["type"] == "reclaimed"]
        self.assertEqual([question["id"], done["id"], event["id"]],
                         [m["id"] for m in status["messages"]])
        for message in (str(question["id"]), done["messageId"]):
            ack = json.loads(self.reclaim_cli("ack", message).stdout)
            self.assertEqual(ack["state"], "processed")
        for message in (str(event["id"]), event["messageId"]):
            rejected = self.reclaim_cli("ack", message, code=1)
            self.assertIn("no controller message", rejected.stderr)
        print(json.dumps({"controllerMessages": [question["id"], done["id"]],
                          "workerReclaim": event["id"]}), flush=True)
        repeat = self.reclaim_cli("reclaim", "--yes", "--dispatch", finished["dispatch"])
        self.assertEqual(json.loads(repeat.stdout)["sessions"][0]["result"], "gone")
        for row in (gone, refused, blocked):
            self.assertNotIn("reclaimed", self.reclaim_status(row))
        for name in ("sibling", refused["tmux"], blocked["tmux"]):
            subprocess.run(self.tmux + ["has-session", "-t", "=" + name], env=self.env, check=True)
        print(json.dumps({"reclaimPreview": json.loads(preview.stdout),
                          "reclaimMixed": json.loads(mixed.stdout),
                          "mixedStderr": mixed.stderr.splitlines(), "exit": mixed.returncode}), flush=True)

    def test_public_reclaim_checks_history_before_closing(self):
        self.assertNotEqual(os.geteuid(), 0, "permission cases require an ordinary user")
        target = self.reclaim_dispatch("preflight", finished=True)
        neighbor = self.reclaim_dispatch("neighbor")
        selected = ("--dispatch", target["dispatch"])
        lock = self.state / "messages.lock"
        parked_lock = self.state / "fixture-original-message-lock"
        pending = self.state / "messages.pending"
        ledger = self.state / "messages.json"
        scope = hashlib.sha256(target["scope"].encode()).hexdigest()
        (self.state / ("relay-" + scope + ".lock")).touch()
        original_ledger = ledger.read_bytes()
        checks = []

        def refuses(case, path, **options):
            rejected = self.reclaim_cli("reclaim", "--yes", *selected, code=1, **options)
            item, = json.loads(rejected.stdout)["sessions"]
            self.assertEqual((item["dispatch"], item["result"]), (target["dispatch"], "failed"))
            self.assertNotIn("message", item)
            self.assertIn("preflight reclaim history", item["error"])
            self.assertIn(path.name, rejected.stderr)
            self.assertEqual(ledger.read_bytes(), original_ledger)
            for name in (target["tmux"], neighbor["tmux"], "sibling"):
                subprocess.run(self.tmux + ["has-session", "-t", "=" + name], env=self.env, check=True)
            checks.append({"case": case, "result": item, "stderr": rejected.stderr,
                           "exit": rejected.returncode, "targetAndNeighborsAlive": True})

        lock.rename(parked_lock)
        try:
            lock.mkdir()
            try:
                refuses("lock directory", lock)
            finally:
                lock.rmdir()
            for destination in ("missing-lock-target", lock.name):
                lock.symlink_to(destination)
                try:
                    refuses("lock symlink to " + destination, lock)
                    self.assertTrue(lock.is_symlink())
                    self.assertFalse((self.state / "missing-lock-target").exists())
                finally:
                    lock.unlink()
            os.mkfifo(lock)
            try:
                refuses("lock FIFO", lock)
            finally:
                lock.unlink()
        finally:
            parked_lock.rename(lock)

        pending.mkdir()
        try:
            refuses("pending directory", pending)
        finally:
            pending.rmdir()
        lock_mode = stat.S_IMODE(lock.stat().st_mode)
        lock.chmod(0o400)
        try:
            refuses("lock permission", lock)
        finally:
            lock.chmod(lock_mode)
        state_mode = stat.S_IMODE(self.state.stat().st_mode)
        self.state.chmod(0o500)
        try:
            refuses("scratch creation permission", pending)
        finally:
            self.state.chmod(state_mode)
        refuses("exact snapshot exceeds RLIMIT_FSIZE", pending, file_size_limit=len(original_ledger))

        # Publishing replaces a removable scratch link without following it.
        pending.symlink_to("missing-scratch-target")
        ledger.chmod(0o400)
        lock.rename(parked_lock)
        lock.symlink_to(parked_lock.name)
        try:
            success = self.reclaim_cli("reclaim", "--yes", *selected)
            self.assertTrue(lock.is_symlink())
        finally:
            lock.unlink()
            parked_lock.rename(lock)
        item, = json.loads(success.stdout)["sessions"]
        self.assertEqual(item["result"], "reclaimed")
        self.assertFalse(pending.is_symlink())
        self.assertFalse((self.state / "missing-scratch-target").exists())
        status = self.reclaim_status(target)
        self.assertEqual([m["type"] for m in status["messages"]], ["done", "reclaimed"])
        self.assertFalse(status["backendAlive"])
        for name in (neighbor["tmux"], "sibling"):
            subprocess.run(self.tmux + ["has-session", "-t", "=" + name], env=self.env, check=True)
        print(json.dumps({"historyPreflight": checks, "reclaimedAfterRepair": item}), flush=True)

    @unittest.skipUnless(os.environ.get("WFC_TEST_STICKY_OWNERSHIP") == "1",
                         "requires CI setup for private fixture ownership")
    def test_public_reclaim_requires_sticky_ownership(self):
        uid = os.geteuid()
        self.assertNotEqual(uid, 0, "reclaim must run as an ordinary user")
        sudo = shutil.which("sudo")
        chown = shutil.which("chown")
        self.assertIsNotNone(sudo)
        self.assertIsNotNone(chown)
        directory_case = self.reclaim_dispatch("sticky directory owner", finished=True)
        ledger_case = self.reclaim_dispatch("sticky ledger owner", finished=True)
        neighbor = self.reclaim_dispatch("sticky neighbor")
        ledger = self.state / "messages.json"
        foreign = self.state / "fixture-foreign-entry"
        scratch = self.state / "fixture-owned-entry"
        original = ledger.read_bytes()
        state_mode = stat.S_IMODE(self.state.stat().st_mode)
        foreign.write_bytes(b"fixture entry")
        scratch.write_bytes(b"fixture scratch")
        ledger.chmod(0o644)
        self.state.chmod(0o1777)

        def set_owner(owner, *paths):
            for path in paths:
                self.assertIn(path, (self.state, ledger, foreign))
                self.assertFalse(path.is_symlink())
                self.assertTrue(path.resolve().is_relative_to(self.root.resolve()))
            subprocess.run([sudo, "-n", "--", chown, "--no-dereference", "--",
                            str(owner), *map(str, paths)], check=True, timeout=10)

        def alive(*sessions):
            for name in sessions:
                subprocess.run(self.tmux + ["has-session", "-t", "=" + name],
                               env=self.env, check=True, timeout=10)

        def reclaim_owned(row, expected):
            result = self.reclaim_cli("reclaim", "--yes", "--dispatch", row["dispatch"])
            item, = json.loads(result.stdout)["sessions"]
            self.assertEqual(item["result"], "reclaimed")
            self.assertIn("message", item)
            status = self.reclaim_status(row)
            self.assertEqual([m["type"] for m in status["messages"]], ["done", "reclaimed"])
            self.assertFalse(status["backendAlive"])
            return {"ownership": expected, "item": item}

        try:
            # Only the fixture metadata is changed with sudo. All WFC and tmux
            # commands use the ordinary runner account.
            set_owner(0, self.state, ledger, foreign)
            self.assertEqual((self.state.stat().st_uid, ledger.stat().st_uid), (0, 0))
            with self.assertRaises(OSError) as denied:
                scratch.replace(foreign)
            self.assertIn(denied.exception.errno, (errno.EPERM, errno.EACCES))
            scratch.unlink()
            failed = self.reclaim_cli("reclaim", "--yes", "--dispatch", directory_case["dispatch"], code=1)
            item, = json.loads(failed.stdout)["sessions"]
            self.assertEqual((item["dispatch"], item["result"]), (directory_case["dispatch"], "failed"))
            self.assertNotIn("message", item)
            self.assertIn("preflight reclaim history", item["error"])
            self.assertIn("sticky directory ownership", failed.stderr)
            self.assertEqual(ledger.read_bytes(), original)
            alive(directory_case["tmux"], ledger_case["tmux"], neighbor["tmux"], "sibling")
            status = self.reclaim_status(directory_case)
            self.assertTrue(status["backendAlive"])
            self.assertEqual([m["type"] for m in status["messages"]], ["done"])

            set_owner(uid, self.state)
            self.assertEqual((self.state.stat().st_uid, ledger.stat().st_uid), (uid, 0))
            directory_ok = reclaim_owned(directory_case, "directory owner")
            set_owner(0, self.state)
            self.assertEqual((self.state.stat().st_uid, ledger.stat().st_uid), (0, uid))
            ledger_ok = reclaim_owned(ledger_case, "ledger owner")
            alive(neighbor["tmux"], "sibling")
            print(json.dumps({"stickyUnproven": item, "exit": failed.returncode,
                              "stderr": failed.stderr, "kernelRenameErrno": denied.exception.errno,
                              "confirmedDirectory": directory_ok, "confirmedLedger": ledger_ok,
                              "targetAndNeighborsAliveOnRefusal": True}), flush=True)
        finally:
            set_owner(uid, self.state, ledger, foreign)
            self.state.chmod(state_mode)

    def test_public_reclaim_stops_on_scope_authority_loss(self):
        first = self.reclaim_dispatch("first")
        owner = json.loads(self.reclaim_cli("acquire", "--scope", "managed", "--owner", "A").stdout)
        middle = self.reclaim_dispatch("managed", scope="managed", token=owner["token"])
        last = self.reclaim_dispatch("last")
        current = json.loads(self.reclaim_cli("takeover", "--scope", "managed", "--owner", "B").stdout)
        result = self.reclaim_cli(
            "reclaim", "--yes", "--dispatch", first["dispatch"], "--dispatch", middle["dispatch"],
            "--dispatch", last["dispatch"], "--owner-token", owner["token"], code=1)
        self.assertFalse(result.stdout)
        error = json.loads(result.stderr)
        self.assertEqual((error["error"], error["operation"], error["scope"], error["generation"]),
                         ("stale_controller_owner", "reclaim", "managed", current["generation"]))
        self.assertEqual(self.reclaim_status(first)["status"], "reclaimed")
        for row in (middle, last):
            status = self.reclaim_status(row)
            self.assertTrue(status["backendAlive"])
            self.assertNotIn("reclaimed", status)
        approved = self.reclaim_cli("reclaim", "--yes", "--dispatch", middle["dispatch"],
                                    "--owner-token", current["token"])
        item, = json.loads(approved.stdout)["sessions"]
        self.assertEqual(item["result"], "reclaimed")
        self.assertEqual(self.reclaim_status(middle)["status"], "reclaimed")
        self.assertTrue(self.reclaim_status(last)["backendAlive"])
        print(json.dumps({"reclaimScopeFence": error, "priorSuccessPersisted": True,
                          "currentOwnerReclaimed": item, "neighborPreserved": True}), flush=True)

    def test_public_reclaim_rechecks_selected_execution_under_scope_lock(self):
        row = self.reclaim_dispatch("selected", finished=True)
        dispatch = row["dispatch"]
        scope = self.reclaim_status(row)["scope"]
        scope_lock = self.state / ("relay-" + hashlib.sha256(scope.encode()).hexdigest() + ".lock")
        path = self.state / "dispatches" / (dispatch + ".json")
        saved = path.read_bytes()
        with scope_lock.open("ab") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with subprocess.Popen([WFC, "reclaim", "--yes"], cwd=self.work, text=True,
                                  env={**self.env, "WFC_STATE": str(self.state)},
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                try:
                    deadline = time.monotonic() + 10
                    fd_dir = Path(f"/proc/{process.pid}/fd")
                    while not any(os.path.realpath(fd) == str(scope_lock) for fd in fd_dir.iterdir()):
                        self.assertIsNone(process.poll())
                        self.assertLess(time.monotonic(), deadline, "reclaim did not take the scope lock")
                        time.sleep(.025)
                    replacement = json.loads(saved)
                    replacement.update(round=2, attempt="a-aaaaaaaaaaaa", executionId="a-aaaaaaaaaaaa")
                    staging = path.with_suffix(".fixture")
                    staging.write_text(json.dumps(replacement))
                    staging.replace(path)
                    fcntl.flock(lock, fcntl.LOCK_UN)
                    out, err = process.communicate(timeout=15)
                    self.assertEqual((process.returncode, err), (0, ""), (out, err))
                    item, = json.loads(out)["sessions"]
                    self.assertEqual((item["dispatch"], item["result"]), (dispatch, "changed"))
                finally:
                    if process.poll() is None:
                        process.kill()
                    path.write_bytes(saved)
        self.assertTrue(self.reclaim_status(row)["backendAlive"])
        subprocess.run(self.tmux + ["has-session", "-t", "=" + row["tmux"]], env=self.env, check=True)
        success = self.reclaim_cli("reclaim", "--yes")
        item, = json.loads(success.stdout)["sessions"]
        self.assertEqual((item["dispatch"], item["result"]), (dispatch, "reclaimed"))
        print(json.dumps({"reclaimChanged": json.loads(out), "reclaimConfirmed": json.loads(success.stdout)}),
              flush=True)

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

    def reply_cli(self, *args, expected=0, **overrides):
        p = subprocess.run([WFC, *map(str, args)], cwd=self.work,
                           env={**self.reply_env, **overrides},
                           text=True, capture_output=True, timeout=35)
        self.assertEqual(p.returncode, expected, (args, p.stdout, p.stderr))
        if expected:
            self.assertFalse(p.stdout)
            return p.stderr
        return json.loads(p.stdout)

    def setup_reply(self, mode, scope=None):
        self.reply_env = {**self.env, "WFC_STATE": str(self.state)}
        for key in ("WFC_WORKER_CALLBACK", "WFC_OWNER_TOKEN"):
            self.reply_env.pop(key, None)
        self.reply_log = self.root / "reply.log"
        config = self.work / "reply-config.json"
        config.write_text(json.dumps({"hosts": {"codex": {"command": [
            sys.executable, str(Path(__file__).resolve()), "--reply-worker", mode, str(self.reply_log),
        ]}}}))
        (self.work / "task").write_text("Ask one question, receive the answer, then report.")
        scope_flags = []
        if scope is not None:
            owner = self.reply_cli("acquire", "--scope", scope, "--owner", "A")
            self.reply_env["WFC_CONTROLLER_TOKEN"] = owner["token"]
            scope_flags = ["--scope", scope]
        self.reply_row = self.reply_cli("dispatch", "--host", "codex", "--backend", "tmux",
                                        "--task", "task", "--cwd", ".", "--report", "report",
                                        "--launch-config", config, *scope_flags)
        self.reply_dispatch = self.reply_row["dispatch"]
        question = self.work / "question.json"
        deadline = time.monotonic() + 10
        while not question.exists() or not self.reply_log.exists():
            self.assertLess(time.monotonic(), deadline, "controlled worker did not publish its question")
            time.sleep(.025)
        ready = {"readonly": "read-only", "upstream-reply": "original draft"}.get(
            mode, "Ask Codex to do anything")
        while ready not in self.reply_screen():
            self.assertLess(time.monotonic(), deadline, "controlled composer did not draw")
            time.sleep(.025)
        self.question = json.loads(question.read_bytes())
        self.number = str(self.question["message"])

    def reply_status(self):
        return self.reply_cli("status", "--dispatch", self.reply_dispatch)["dispatches"][0]

    def reply_screen(self):
        result = self.call("read", dispatch=self.reply_dispatch, executionId=self.reply_row["attempt"])
        self.assertEqual(result["result"], "accepted", result)
        return result["screen"]

    def reply_attempts(self):
        replies = [m for m in self.reply_status()["messages"] if m["type"] == "reply"]
        for reply in replies:
            self.assertEqual(reply["questionMessageId"], self.question["messageId"])
            self.assertEqual(reply["to"], self.question["message"])
            for key in ("dispatch", "round", "attempt"):
                self.assertEqual(reply[key], self.reply_row[key])
        return replies

    def assert_reply_bytes(self, expected):
        events = self.events(self.reply_log, len(expected))
        self.assertEqual(b"".join(bytes.fromhex(byte) for byte, _ in events), expected)

    def test_public_reply_once_and_original_done(self):
        self.setup_reply("drop")
        self.assertEqual(self.reply_status()["status"], "waiting-human")
        waiting = self.reply_cli("wait", "--dispatch", self.reply_dispatch, "--timeout", "0")
        question, = waiting["messages"]
        self.assertEqual(question["messageId"], self.question["messageId"])
        answer = "one '中文'\r\nsecond \\ $HOME #{pid}"
        argv = [WFC, "reply", self.number, answer]
        with subprocess.Popen(argv, env=self.reply_env, cwd=self.work, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE) as first, \
             subprocess.Popen(argv, env=self.reply_env, cwd=self.work, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE) as second:
            results = [(p, *p.communicate(timeout=35)) for p in (first, second)]
        self.assertEqual(sorted(p.returncode for p, _, _ in results), [0, 1])
        for proc, out, err in results:
            if proc.returncode == 0:
                self.assertEqual(json.loads(out),
                                 {"replied": self.question["message"], "dispatch": self.reply_dispatch})
            else:
                self.assertFalse(out)
                self.assertIn("already answered or has a reply pending", err)
        self.assertIn("already answered", self.reply_cli("reply", self.number, "different", expected=1))
        one_line = " ".join(answer.splitlines())
        text = f"[router] Answer to question {self.number}: {one_line}".encode()
        self.assert_reply_bytes(text + b"\r\r")
        self.assertEqual((self.work / "accepted").read_bytes(), text + b"\n")
        reply, = self.reply_attempts()
        self.assertEqual(reply["inputResult"], "accepted")
        self.assertEqual(self.reply_status()["status"], "online")
        self.assertIn("no controller message", self.reply_cli("ack", reply["messageId"], expected=1))
        (self.work / "finish").touch()
        completed = self.reply_cli("wait", "--dispatch", self.reply_dispatch, "--timeout", "8")
        done, = completed["messages"]
        self.assertEqual((done["type"], done["round"], done["attempt"]),
                         ("done", self.reply_row["round"], self.reply_row["attempt"]))
        self.assertEqual(Path(done["report"]).read_text(), "original worker received the answer\n")
        self.assertEqual(self.reply_cli("close", self.reply_dispatch)["result"], "completed")
        self.assertEqual(self.reply_status()["state"], "done")

    def test_public_reply_releases_known_unsent(self):
        self.setup_reply("readonly")
        self.assertIn("refused", self.reply_cli("reply", self.number, "blocked", expected=1))
        attempt, = self.reply_attempts()
        self.assertEqual(attempt["inputResult"], "refused")
        self.assertIn("read-only", attempt["diagnostic"])
        self.assert_reply_bytes(b"")
        (self.work / "enable-input").touch()
        deadline = time.monotonic() + 5
        while "read-only" in self.reply_screen():
            self.assertLess(time.monotonic(), deadline, "composer did not enable input")
            time.sleep(.025)
        self.assertIn("definitely-not-sent",
                      self.reply_cli("reply", self.number, "missing tmux", expected=1, PATH=str(self.work)))
        self.assert_reply_bytes(b"")
        self.reply_cli("reply", self.number, "now active", "--owner-token", "unused-for-unowned")
        self.assertEqual([m["inputResult"] for m in self.reply_attempts()],
                         ["refused", "definitely-not-sent", "accepted"])
        self.assert_reply_bytes(f"[router] Answer to question {self.number}: now active\r".encode())

    def test_public_reply_and_reclaim_preserve_controller_receipts(self):
        self.setup_reply("readonly")
        dispatch = self.reply_dispatch
        self.assertIn("read-only", self.reply_cli("reply", self.number, "blocked", expected=1))
        self.assert_reply_bytes(b"")
        (self.work / "enable-input").touch()
        deadline = time.monotonic() + 5
        while "read-only" in self.reply_screen():
            self.assertLess(time.monotonic(), deadline, "composer did not enable input")
            time.sleep(.025)
        self.reply_cli("reply", self.number, "once")
        answer = f"[router] Answer to question {self.number}: once".encode()
        self.assert_reply_bytes(answer + b"\r")
        self.assertEqual((self.work / "accepted").read_bytes(), answer + b"\n")
        (self.work / "finish").touch()
        deadline = time.monotonic() + 8
        while self.reply_status()["state"] != "done":
            self.assertLess(time.monotonic(), deadline, "worker did not publish done")
            time.sleep(.025)

        reclaim, = self.reply_cli("reclaim", "--yes", "--dispatch", dispatch)["sessions"]
        self.assertEqual(reclaim["result"], "reclaimed")
        status = self.reply_cli("status", "--dispatch", dispatch)
        row, = status["dispatches"]
        self.assertEqual((row["state"], row["status"], row["backendAlive"]), ("done", "reclaimed", False))
        self.assertEqual([m["type"] for m in row["messages"]],
                         ["question", "reply", "reply", "done", "reclaimed"])
        question, refused, accepted, done, event = row["messages"]
        self.assertEqual(question["messageId"], self.question["messageId"])
        self.assertEqual([m["inputResult"] for m in self.reply_attempts()], ["refused", "accepted"])
        self.assertEqual((event["id"], event["to"], event["delivered"]),
                         (reclaim["message"], "worker", False))
        self.assertEqual([(m["id"], m["messageId"]) for m in status["undelivered"]],
                         [(m["id"], m["messageId"]) for m in (question, done)])
        self.reply_cli("dispatch", "--task", "task", "--cwd", ".", "--report", "startup-report",
                       "--role", "startup-check", "--scope", "failed-launch", "--",
                       self.work / "missing-worker", expected=1)
        status = self.reply_cli("status")
        failed, = [row for row in status["dispatches"] if row["scope"] == "failed-launch"]
        detail, = self.reply_cli("status", "--dispatch", failed["dispatch"])["dispatches"]
        notice, = detail["messages"]
        self.assertEqual((notice["kind"], notice["type"], notice["to"], notice["delivered"]),
                         ("startup_failure", "blocked", "controller", False))
        self.assertEqual(detail["startupFailure"], notice["fault"])
        self.assertIn("no question with id", self.reply_cli("reply", notice["id"], "ignored", expected=1))
        identities = [(m["id"], m["messageId"]) for m in (question, done, notice)]
        for batch in (status["undelivered"], self.reply_cli("wait", "--timeout", "0")["messages"]):
            self.assertEqual([(m["id"], m["messageId"]) for m in batch], identities)
        selectors = (str(question["id"]), done["messageId"], notice["messageId"])
        for entry, selector in zip((question, done, notice), selectors):
            ack = self.reply_cli("ack", selector)
            self.assertEqual((ack["message"], ack["messageId"], ack["state"]),
                             (entry["id"], entry["messageId"], "processed"))
        self.assertTrue(self.reply_cli("ack", str(notice["id"]))["duplicate"])
        self.assertFalse(self.reply_cli("status")["undelivered"])
        self.assertIn("no question with id", self.reply_cli("reply", done["id"], "ignored", expected=1))
        self.assertIn("no question with id", self.reply_cli("reply", event["id"], "ignored", expected=1))
        for entry in (refused, accepted, event):
            for selector in (str(entry["id"]), entry["messageId"]):
                self.assertIn("no controller message", self.reply_cli("ack", selector, expected=1))
                self.assertIn("no controller message", self.reply_cli(
                    "resolve", selector, "--actor", "fixture", "--reason", "mixed ledger",
                    "--evidence", "test state", "--body-hash", "0" * 64, expected=1))
        self.assertEqual(Path(done["report"]).read_text(), "original worker received the answer\n")
        self.assertEqual(self.reply_cli("close", dispatch)["result"], "completed")
        self.assertEqual(self.reply_status()["messages"][-1]["messageId"], event["messageId"])

    def test_public_reply_retains_claim_on_lost_enter_confirmation(self):
        self.setup_reply("empty")
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys
argv = [{TMUX!r}, *sys.argv[1:]]
if any("send-keys -t " in x and " Enter ; " in x for x in argv):
    subprocess.run(argv, check=True, capture_output=True)
    sys.exit(1)
os.execv(argv[0], argv)
""")
        shim.chmod(0o700)
        err = self.reply_cli("reply", self.number, "sent once", expected=1,
                             PATH=f"{self.work}:{self.reply_env['PATH']}")
        self.assertIn("Enter was not confirmed", err)
        self.assertIn("already answered", self.reply_cli("reply", self.number, "again", expected=1))
        reply, = self.reply_attempts()
        self.assertEqual((reply["inputResult"], reply["category"]), ("uncertain", "input_uncertain"))
        text = f"[router] Answer to question {self.number}: sent once".encode()
        self.assert_reply_bytes(text + b"\r")
        self.assertEqual((self.work / "accepted").read_bytes(), text + b"\n")
        question, = self.reply_cli("wait", "--timeout", "0")["messages"]
        self.assertEqual(question["type"], "question")
        (self.work / "finish").touch()
        done, = self.reply_cli("wait", "--timeout", "8")["messages"]
        self.assertEqual((done["type"], done["attempt"]), ("done", self.reply_row["attempt"]))

    def test_public_reply_keeps_ambiguous_draft(self):
        self.setup_reply("scroll")
        self.assertIn("composer did not confirm",
                      self.reply_cli("reply", self.number, "visible slice of the draft", expected=1))
        self.assertIn("already answered", self.reply_cli("reply", self.number, "again", expected=1))
        reply, = self.reply_attempts()
        self.assertEqual(reply["inputResult"], "uncertain")
        self.assert_reply_bytes(f"[router] Answer to question {self.number}: visible slice of the draft\r".encode())
        self.assertFalse((self.work / "accepted").exists())

    def test_public_reply_preserves_tmux_draft_with_changed_whitespace(self):
        self.setup_reply("changed-reply")
        self.assertIn("composer did not confirm",
                      self.reply_cli("reply", self.number, "a b", expected=1))
        self.assertIn(": ab", self.reply_screen())
        self.assertIn("already answered", self.reply_cli("reply", self.number, "again", expected=1))
        reply, = self.reply_attempts()
        self.assertEqual(reply["inputResult"], "uncertain")
        self.assert_reply_bytes(f"[router] Answer to question {self.number}: a b\r".encode())
        self.assertFalse((self.work / "accepted").exists())

    def test_public_reply_keeps_claim_when_tmux_draft_reappears(self):
        self.setup_reply("always-drop")
        answer = "one controlled reply"
        text = f"[router] Answer to question {self.number}: {answer}".encode()
        trace = self.root / "reply-captures"
        empty = self.root / "empty-composer"
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, subprocess, sys
from pathlib import Path

args = sys.argv[1:]
argv = [{TMUX!r}, *args]
trace = Path({str(trace)!r})
empty = Path({str(empty)!r})
if any("send-keys -t " in arg and " Enter ; " in arg for arg in args):
    subprocess.run(argv, check=True)
    with trace.open("a") as log:
        print("enter", file=log)
    sys.exit(0)
if any("capture-pane -p -e -t " in arg for arg in args):
    result = subprocess.run(argv, check=True, capture_output=True)
    owner, newline, screen = result.stdout.partition(b"\\n")
    confirmed = b"wfc-tmux-confirmed\\n"
    assert owner.startswith(b"WFC_STATE=") and screen.endswith(confirmed), result.stdout
    screen = screen.removesuffix(confirmed)
    if trace.exists():
        assert {text!r} in screen, screen
        # Only the first styled read after Enter shows the transient empty frame.
        first_read = trace.read_text() == "enter\\n"
        if first_read:
            screen = empty.read_bytes()
        with trace.open("a") as log:
            print("empty" if first_read else "draft", file=log)
    else:
        assert b"Ask Codex to do anything" in screen, screen
        empty.write_bytes(screen)
    sys.stdout.buffer.write(owner + newline + screen + confirmed)
    sys.exit(0)
os.execv(argv[0], argv)
""")
        shim.chmod(0o700)
        path = f"{self.work}:{self.reply_env['PATH']}"
        diagnostic = self.reply_cli("reply", self.number, answer, expected=1, PATH=path)
        self.assertIn("reply draft reappeared after an empty composer", diagnostic)
        self.assertIn("already answered", self.reply_cli("reply", self.number, "again", expected=1, PATH=path))
        self.assertEqual(trace.read_text().splitlines(), ["enter", "empty", "draft"])
        self.assert_reply_bytes(text + b"\r")
        self.assertIn(text.decode(), self.reply_screen())
        self.assertFalse((self.work / "accepted").exists())
        reply, = self.reply_attempts()
        self.assertEqual((reply["inputResult"], reply["category"]), ("uncertain", "input_uncertain"))
        question, = self.reply_cli("wait", "--dispatch", self.reply_dispatch, "--timeout", "0")["messages"]
        self.assertEqual((question["type"], question["messageId"]), ("question", self.question["messageId"]))

    def test_public_reply_requires_confirmed_tmux_chat_composer(self):
        self.setup_reply("upstream-reply")
        pane = f"={self.reply_row['tmux']}:"

        def styled():
            return subprocess.check_output(
                self.tmux + ["capture-pane", "-p", "-e", "-t", pane], env=self.env, text=True)

        def wait_for_screen(text):
            deadline = time.monotonic() + 5
            while text not in self.reply_screen():
                self.assertLess(time.monotonic(), deadline, f"composer did not draw {text}")
                time.sleep(.025)

        def refuse(reason):
            before = self.reply_screen(), styled()
            diagnostic = self.reply_cli("reply", self.number, "blocked", expected=1)
            self.assertIn(reason, diagnostic)
            self.assertEqual((self.reply_screen(), styled()), before)
            self.assert_reply_bytes(b"")
            self.assertFalse((self.work / "accepted").exists())
            self.assertEqual(self.reply_attempts()[-1]["inputResult"], "definitely-not-sent")

        self.assertIn("100% context left", self.reply_screen())
        refuse("composer already contains a draft")
        (self.work / "show-placeholder-draft").touch()
        wait_for_screen("› Ask Codex to do anything")
        placeholder_draft = self.reply_screen(), styled()
        refuse("composer already contains a draft")
        (self.work / "show-unknown-footer").touch()
        wait_for_screen("unknown editor mode")
        refuse("writable chat composer could not be verified")
        (self.work / "clear-draft").touch()
        wait_for_screen("100% context left")
        self.assertEqual(self.reply_screen(), placeholder_draft[0])
        self.assertNotEqual(styled(), placeholder_draft[1])

        subprocess.run(self.tmux + ["copy-mode", "-t", pane], env=self.env, check=True)
        try:
            self.assertIn("refused", self.reply_cli("reply", self.number, "copy mode", expected=1))
            self.assert_reply_bytes(b"")
            self.assertEqual(self.reply_screen(), placeholder_draft[0])
            self.assertEqual(self.reply_attempts()[-1]["inputResult"], "refused")
        finally:
            subprocess.run(self.tmux + ["send-keys", "-t", pane, "-X", "cancel"], env=self.env, check=True)

        question, = self.reply_cli("wait", "--dispatch", self.reply_dispatch, "--timeout", "0")["messages"]
        self.assertEqual(question["messageId"], self.question["messageId"])
        self.reply_cli("reply", self.number, "once")
        self.assertEqual([m["inputResult"] for m in self.reply_attempts()],
                         ["definitely-not-sent"] * 3 + ["refused", "accepted"])
        answer = f"[router] Answer to question {self.number}: once".encode()
        self.assert_reply_bytes(answer + b"\r")
        self.assertEqual((self.work / "accepted").read_bytes(), answer + b"\n")
        self.assertIn("already answered", self.reply_cli("reply", self.number, "again", expected=1))
        (self.work / "finish").touch()
        done, = self.reply_cli("wait", "--dispatch", self.reply_dispatch, "--timeout", "8")["messages"]
        self.assertEqual((done["type"], done["dispatch"], done["round"], done["attempt"]),
                         ("done", self.reply_dispatch, self.reply_row["round"], self.reply_row["attempt"]))
        self.assertEqual(Path(done["report"]).read_text(), "original worker received the answer\n")
        self.assertEqual(self.reply_cli("close", self.reply_dispatch)["result"], "completed")

    def test_public_reply_rechecks_after_waiting_for_input_lock(self):
        self.setup_reply("finish-alone")
        path = self.state / "close-locks" / f"{self.reply_dispatch}.{self.reply_row['attempt']}"
        with path.open("r+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with subprocess.Popen([WFC, "reply", self.number, "too late"], env=self.reply_env,
                                  cwd=self.work, text=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE) as attempt:
                try:
                    deadline = time.monotonic() + 5
                    while not self.reply_attempts():
                        self.assertLess(time.monotonic(), deadline, "no durable reply reservation")
                        time.sleep(.025)
                    reply, = self.reply_attempts()
                    self.assertEqual(reply["inputResult"], "pending")
                    question, = self.reply_cli("wait", "--timeout", "0")["messages"]
                    self.assertEqual(question["type"], "question")
                    (self.work / "finish").touch()
                    done, = self.reply_cli("wait", "--timeout", "8")["messages"]
                    self.assertEqual(done["type"], "done")
                finally:
                    fcntl.flock(lock, fcntl.LOCK_UN)
                out, err = attempt.communicate(timeout=10)
        self.assertEqual(attempt.returncode, 1)
        self.assertFalse(out)
        self.assertIn("round is not running", err)
        reply, = self.reply_attempts()
        self.assertEqual(reply["inputResult"], "not-submitted")
        self.assert_reply_bytes(b"")

    def test_managed_tmux_reply_rechecks_owner_while_waiting(self):
        scope = "managed-reply"
        self.setup_reply("empty", scope)
        error = self.reply_cli("reply", self.number, "callback cannot reply", expected=1,
                               WFC_WORKER_CALLBACK="synthetic")
        self.assertEqual(json.loads(error)["error"], "worker_callback_only")
        error = self.reply_cli("reply", self.number, "missing owner", expected=1,
                               WFC_CONTROLLER_TOKEN="")
        self.assertEqual(json.loads(error)["error"], "stale_controller_owner")
        self.assertEqual(self.reply_attempts(), [])
        path = self.state / "close-locks" / f"{self.reply_dispatch}.{self.reply_row['attempt']}"
        with path.open("rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with subprocess.Popen([WFC, "reply", self.number, "stale input"], env=self.reply_env,
                                  cwd=self.work, text=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE) as old_reply:
                try:
                    deadline = time.monotonic() + 5
                    while not self.reply_attempts():
                        self.assertLess(time.monotonic(), deadline, "reply was not reserved")
                        time.sleep(.025)
                    attempt, = self.reply_attempts()
                    self.assertEqual(attempt["inputResult"], "pending")
                    current = self.reply_cli("takeover", "--scope", scope, "--owner", "B")
                    out, err = old_reply.communicate(timeout=5)
                finally:
                    fcntl.flock(lock, fcntl.LOCK_UN)
        self.assertEqual((old_reply.returncode, out), (1, ""))
        self.assertEqual((json.loads(err)["error"], json.loads(err)["generation"]),
                         ("stale_controller_owner", current["generation"]))
        attempt, = self.reply_attempts()
        self.assertEqual(attempt["inputResult"], "not-submitted")
        self.assertIn("stale_controller_owner", attempt["diagnostic"])
        self.assert_reply_bytes(b"")
        self.reply_cli("reply", self.number, "owner B", "--owner-token", current["token"])
        self.reply_env["WFC_CONTROLLER_TOKEN"] = current["token"]
        self.assertIn("already answered", self.reply_cli("reply", self.number, "again", expected=1))
        text = f"[router] Answer to question {self.number}: owner B".encode()
        self.assert_reply_bytes(text + b"\r")
        self.assertEqual((self.work / "accepted").read_bytes(), text + b"\n")
        self.assertEqual([m["inputResult"] for m in self.reply_attempts()], ["not-submitted", "accepted"])
        question, = self.reply_cli("wait", "--scope", scope, "--timeout", 0)["messages"]
        self.assertEqual(question["messageId"], self.question["messageId"])
        (self.work / "finish").touch()
        done, = self.reply_cli("wait", "--scope", scope, "--timeout", 8)["messages"]
        self.assertEqual((done["type"], done["attempt"]), ("done", self.reply_row["attempt"]))
        self.assertEqual(Path(done["report"]).read_text(), "original worker received the answer\n")
        self.assertEqual(self.reply_cli("ack", done["messageId"])["state"], "processed")
        self.assertEqual(self.reply_cli("close", self.reply_dispatch)["result"], "completed")

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

    def handoff_fixture(self, *, held_writer=False, detached_descendant=False):
        self.addCleanup(self.stop_handoff_fixtures)
        worker = self.work / "handoff-host.py"
        worker.write_text("""import json, os, signal, subprocess, sys, time
from pathlib import Path

assert "WFC_OWNER_TOKEN" not in os.environ and "WFC_CONTROLLER_TOKEN" not in os.environ
fields = ("WFC_DISPATCH", "WFC_ROUND", "WFC_ATTEMPT", "WFC_REPORT",
          "WFC_TASK", "WFC_SCOPE", "WFC_WORKER_CALLBACK")
callback = {key: os.environ[key] for key in fields}
attempt = callback["WFC_ATTEMPT"]
if os.environ.get("HANDOFF_TEST_HOLD_HUP"):
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
descendant = None
if os.environ.get("HANDOFF_TEST_DETACHED"):
    script = '''
import sys, time
from pathlib import Path
deadline = time.monotonic() + 35
while not Path(sys.argv[1]).exists() and time.monotonic() < deadline:
    time.sleep(.025)
'''
    descendant = subprocess.Popen(
        [sys.executable, "-c", script, attempt + ".stop"], start_new_session=True
    ).pid
temp = Path(attempt + ".ready.tmp")
temp.write_text(json.dumps(dict(host=sys.argv[1], argv=sys.argv[2:], callback=callback,
                                pid=os.getpid(), descendant=descendant)))
temp.rename(attempt + ".ready")
deadline = time.monotonic() + 90
while (not Path(attempt + ".stop").exists() and not Path(attempt + ".root-exit").exists()
       and time.monotonic() < deadline):
    time.sleep(.025)
""")
        config = self.work / "handoff-config.json"
        settings = {"hosts": {
            host: {"command": [sys.executable, str(worker), host]}
            for host in ("codex", "opencode")
        }}
        if held_writer:
            settings["hosts"]["codex"]["env"] = {"HANDOFF_TEST_HOLD_HUP": "1"}
        if detached_descendant:
            settings["hosts"]["codex"]["env"] = {"HANDOFF_TEST_DETACHED": "1"}
        config.write_text(json.dumps(settings))
        (self.work / "handoff-task").write_text("original task")
        scope = "cross-host"
        owner = json.loads(self.reclaim_cli("acquire", "--scope", scope, "--owner", "A").stdout)
        token = owner["token"]
        original = json.loads(self.reclaim_cli(
            "dispatch", "--scope", scope, "--owner-token", token, "--host", "codex",
            "--cwd", ".", "--task", "handoff-task", "--report", "handoff-report",
            "--model", "original-model", "--launch-config", str(config)).stdout)
        return original, token, self.handoff_ready(original["attempt"])

    def handoff_until(self, check):
        deadline = time.monotonic() + 8
        while not check():
            self.assertLess(time.monotonic(), deadline, "handoff fixture did not reach its boundary")
            time.sleep(.025)

    def handoff_ready(self, attempt):
        ready = self.work / f"{attempt}.ready"
        self.handoff_until(ready.is_file)
        return json.loads(ready.read_bytes())

    def stop_handoff_fixtures(self):
        # This is teardown for the synthetic host's wait loop. The test's
        # product close and writer-stop observations precede these signals.
        attempts = {path.stem for path in self.work.glob("a-*.ready")}
        launches = self.state / "launches"

        def stopped_attempts():
            return {json.loads(path.read_bytes())["attempt"] for path in launches.glob("*.stopped")}

        remaining = attempts - stopped_attempts()
        if remaining:
            print(json.dumps({"fixtureStopRequested": sorted(remaining)}), flush=True)
        for attempt in remaining:
            (self.work / f"{attempt}.stop").touch()
        self.handoff_until(lambda: attempts <= stopped_attempts())

    def stop_handoff_host(self, row):
        (self.work / f"{row['attempt']}.stop").touch()
        receipt = Path(row["create"]["receipt"])
        self.handoff_until(lambda: json.loads(receipt.read_bytes())["category"] == "process_exited")
        def gone():
            result = subprocess.run(self.tmux + ["has-session", "-t", "=" + row["tmux"]],
                                    env=self.env, capture_output=True, timeout=5)
            return result.returncode != 0
        self.handoff_until(gone)

    def test_public_handoff_switches_host_once_in_the_same_round(self):
        row, token, original = self.handoff_fixture(held_writer=True)
        args = ("handoff", row["dispatch"], "--host", "opencode", "--owner-token", token)
        archive = self.state / "dispatches" / f"{row['dispatch']}.{row['attempt']}.closed"
        with subprocess.Popen([WFC, *args], cwd=self.work,
                              env={**self.env, "WFC_STATE": str(self.state)}, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE) as control:
            try:
                self.handoff_until(archive.is_file)
                self.assertIsNone(control.poll())
                self.assertEqual(len(list(self.work.glob("*.ready"))), 1)
                os.kill(original["pid"], 0)
                refused = self.reclaim_cli("ask", "old writer still active", "--attempt", row["attempt"],
                                           code=1, **original["callback"])
                self.assertIn("handoff is closing the old execution", refused.stderr)
            finally:
                (self.work / f"{row['attempt']}.stop").touch()
                try:
                    out, err = control.communicate(timeout=15)
                except subprocess.TimeoutExpired:
                    control.kill()
                    control.wait(timeout=5)
                    raise
        self.assertEqual(control.returncode, 0, (out, err))
        result = json.loads(out)
        self.assertEqual((result["result"], result["round"], result["fromAttempt"], result["host"]),
                         ("accepted", 1, row["attempt"], "opencode"))
        self.assertNotEqual(result["attempt"], row["attempt"])
        worker = self.handoff_ready(result["attempt"])
        self.assertEqual(worker["host"], "opencode")
        self.assertEqual(worker["argv"][worker["argv"].index("-m") + 1], "original-model")
        self.assertEqual(worker["callback"]["WFC_TASK"], original["callback"]["WFC_TASK"])
        self.assertEqual(worker["callback"]["WFC_REPORT"], original["callback"]["WFC_REPORT"])
        self.assertNotEqual(worker["callback"]["WFC_WORKER_CALLBACK"],
                            original["callback"]["WFC_WORKER_CALLBACK"])
        stale = self.reclaim_cli("ask", "stale worker", "--attempt", row["attempt"], code=1,
                                 **original["callback"])
        self.assertIn("attempt is no longer accepted", stale.stderr)
        spoofed = {**worker["callback"], "WFC_WORKER_CALLBACK": original["callback"]["WFC_WORKER_CALLBACK"]}
        stale = self.reclaim_cli("ask", "old credential", "--attempt", result["attempt"], code=1, **spoofed)
        self.assertIn("invalid_worker_callback", stale.stderr)
        self.reclaim_cli("ask", "new worker", "--attempt", result["attempt"], **worker["callback"])
        duplicate = json.loads(self.reclaim_cli(*args).stdout)
        self.assertEqual(duplicate, {**result, "duplicate": True})
        status = self.reclaim_status(row)
        self.assertNotIn("startupFailure", status)
        event, = status["audit"]
        self.assertEqual((event["fromHost"], event["toHost"], event["fromAttempt"], event["toAttempt"],
                          event["close"]["resourceCleanup"], event["oldWriterStopped"]),
                         ("codex", "opencode", row["attempt"], result["attempt"], "confirmed", True))
        self.assertTrue(event["writerStopEvidence"]["writerTreeStopped"])
        self.assertEqual(event["writerStopEvidence"]["attempt"], row["attempt"])
        self.assertEqual(len(list(self.work.glob("*.ready"))), 2)
        self.reclaim_cli("close", row["dispatch"], "--owner-token", token)
        print(json.dumps({"handoff": result, "duplicate": duplicate, "audit": event}), flush=True)

    def test_public_handoff_preserves_startup_notice_and_original_close(self):
        row, token, original = self.handoff_fixture()
        dispatch, attempt = row["dispatch"], row["attempt"]
        owner = ("--owner-token", token)
        question = json.loads(self.reclaim_cli(
            "ask", "original worker question", "--attempt", attempt, **original["callback"]).stdout)
        self.stop_handoff_host(row)
        config = self.work / "handoff-config.json"
        settings = json.loads(config.read_bytes())
        settings["hosts"]["opencode"]["command"] = ["/bin/sh", "-c", "exit 17"]
        config.write_text(json.dumps(settings))

        args = ("handoff", dispatch, "--host", "opencode", *owner)
        result = json.loads(self.reclaim_cli(*args).stdout)
        new_attempt = result["attempt"]
        self.assertEqual((result["result"], result["round"], result["fromAttempt"]),
                         ("uncertain", 1, attempt))
        self.assertNotEqual(new_attempt, attempt)
        state = self.reclaim_status(row)
        event, = state["audit"]
        receipt = Path(event["launch"]["receipt"])
        stopped = receipt.with_suffix(".stopped")
        self.handoff_until(stopped.is_file)
        self.assertEqual(json.loads(stopped.read_bytes()),
                         {"launchId": receipt.stem, "dispatch": dispatch, "attempt": new_attempt,
                          "round": "1", "writerTreeStopped": True})
        terminal = json.loads(receipt.read_bytes())
        archive = self.state / "dispatches" / f"{dispatch}.{attempt}.closed"
        original_close = archive.read_bytes()
        close_id = json.loads(original_close)["closeOperationId"]
        self.assertEqual((event["close"]["closeOperationId"], event["close"]["attempt"],
                          event["close"]["resourceCleanup"], event["oldWriterStopped"]),
                         (close_id, attempt, "confirmed", True))
        self.assertNotEqual(receipt, Path(row["create"]["receipt"]))
        self.assertEqual((terminal["category"], terminal["exitCode"], terminal["treeTracked"]),
                         ("process_exited", 17, True))
        old, fault = state["messages"]
        self.assertEqual((old["id"], old["messageId"], old["attempt"]),
                         (question["message"], question["messageId"], attempt))
        self.assertEqual((fault["kind"], fault["dispatch"], fault["attempt"], fault["round"],
                          fault["host"], fault["fault"]["launchId"], fault["fault"]["exitCode"]),
                         ("startup_failure", dispatch, new_attempt, 1, "opencode", receipt.stem, 17))
        for key in ("pid", "processStart"):
            self.assertEqual(fault["fault"][key], terminal["startupExit"][key])
        self.assertEqual(state["startupFailure"], fault["fault"])
        self.assertEqual(state["state"], "unknown")

        batch = json.loads(self.reclaim_cli(
            "wait", "--scope", row["scope"], "--timeout", "0", *owner).stdout)["messages"]
        self.assertEqual([(m["id"], m["messageId"]) for m in batch],
                         [(m["id"], m["messageId"]) for m in (old, fault)])
        ack = json.loads(self.reclaim_cli("ack", fault["messageId"], *owner).stdout)
        self.assertEqual((ack["message"], ack["messageId"], ack["state"]),
                         (fault["id"], fault["messageId"], "processed"))
        retry = json.loads(self.reclaim_cli(*args).stdout)
        self.assertEqual((retry["attempt"], retry["audit"], retry["result"]),
                         (new_attempt, result["audit"], "uncertain"))
        self.assertEqual(archive.read_bytes(), original_close)
        self.assertEqual([m["messageId"] for m in self.reclaim_status(row)["messages"]],
                         [old["messageId"], fault["messageId"]])
        self.assertEqual(len(list((self.state / "launches").glob("*.submission"))), 2)
        print(json.dumps({"startupNotice": fault["messageId"], "oldQuestion": old["messageId"],
                          "oldClose": close_id, "failedAttempt": new_attempt,
                          "failedLaunch": receipt.stem, "noSecondLaunch": True}), flush=True)

    def test_public_handoff_requires_new_paths_after_done_and_preserves_receipts(self):
        row, token, original = self.handoff_fixture()
        Path(row["report"]).write_text("finished old task")
        done = json.loads(self.reclaim_cli("done", "--report", row["report"], "--attempt", row["attempt"],
                                           **original["callback"]).stdout)
        self.stop_handoff_host(row)
        args = ("handoff", row["dispatch"], "--host", "opencode", "--owner-token", token)
        self.assertIn("give the next --task and --report", self.reclaim_cli(*args, code=1).stderr)
        (self.work / "next-task").write_text("new round")
        self.assertIn("report was already used", self.reclaim_cli(
            *args, "--task", "next-task", "--report", row["report"], code=1).stderr)
        options = ("--task", "next-task", "--report", "next-report", "--model", "next-model")
        result = json.loads(self.reclaim_cli(*args, *options).stdout)
        self.assertEqual((result["result"], result["round"]), ("accepted", 2))
        worker = self.handoff_ready(result["attempt"])
        self.assertEqual(worker["argv"][worker["argv"].index("-m") + 1], "next-model")
        self.assertEqual(worker["callback"]["WFC_REPORT"], str(self.work / "next-report"))
        self.assertEqual(json.loads(self.reclaim_cli(*args, *options).stdout), {**result, "duplicate": True})
        status = self.reclaim_status(row)
        old, new = status["rounds"]
        self.assertEqual((old["state"], old["result"]["messageId"], old["report"]),
                         ("completed", done["messageId"], row["report"]))
        self.assertEqual((new["round"], new["attempt"], new["report"]),
                         (2, result["attempt"], str(self.work / "next-report")))
        self.assertEqual(Path(row["report"]).read_text(), "finished old task")
        self.assertEqual((status["audit"][0]["fromModel"], status["audit"][0]["toModel"]),
                         ("original-model", "next-model"))
        self.assertEqual(len(list(self.work.glob("*.ready"))), 2)
        self.reclaim_cli("close", row["dispatch"], "--owner-token", token)
        print(json.dumps({"newRound": result, "originalReceipt": done}), flush=True)

    def test_public_handoff_refuses_stale_owner_and_foreign_tmux_binding(self):
        row, token, _ = self.handoff_fixture()
        next_owner = json.loads(self.reclaim_cli("takeover", "--scope", row["scope"], "--owner", "B").stdout)
        args = ("handoff", row["dispatch"], "--host", "opencode", "--owner-token")
        self.assertEqual(json.loads(self.reclaim_cli(*args, token, code=1).stderr)["error"],
                         "stale_controller_owner")
        path = self.state / "dispatches" / f"{row['dispatch']}.json"
        before = json.loads(path.read_bytes())
        session = "=" + row["tmux"]
        subprocess.run(self.tmux + ["set-environment", "-t", session, "WFC_STATE", str(self.other)],
                       env=self.env, check=True, timeout=5)
        ambiguous = json.loads(self.reclaim_cli(*args, next_owner["token"], code=1).stderr)
        self.assertEqual((ambiguous["result"], ambiguous["fromAttempt"], ambiguous["host"], ambiguous["audit"]),
                         ("refused", row["attempt"], "opencode", 1))
        saved = json.loads(path.read_bytes())
        event, = saved.pop("audit")
        self.assertEqual(saved, before)
        self.assertEqual((event["phase"], event["fromAttempt"], event["toHost"]),
                         ("preflight", row["attempt"], "opencode"))
        subprocess.run(self.tmux + ["set-environment", "-t", session, "WFC_STATE", str(self.state)],
                       env=self.env, check=True, timeout=5)
        receipt = Path(row["create"]["receipt"])
        original = receipt.read_bytes()
        legacy = json.loads(original)
        del legacy["treeTracked"]
        try:
            receipt.write_text(json.dumps(legacy))
            unverified = json.loads(self.reclaim_cli(*args, next_owner["token"]).stdout)
            self.assertEqual((unverified["result"], unverified["attempt"], unverified["audit"]),
                             ("uncertain", row["attempt"], 2))
        finally:
            receipt.write_bytes(original)
        self.assertEqual(len(list(self.work.glob("*.ready"))), 1)
        self.active_pane(row["dispatch"])
        status = self.reclaim_status(row)
        self.assertEqual((status["state"], status["audit"][-1]["result"]), ("unknown", "uncertain"))
        subprocess.run(self.tmux + ["has-session", "-t", "=sibling"], env=self.env, check=True, timeout=5)
        print(json.dumps({"foreignHandoff": ambiguous, "missingWriterEvidence": unverified,
                          "sameAssignment": True}), flush=True)

    def test_public_handoff_interruption_after_close_does_not_launch_twice(self):
        row, token, _ = self.handoff_fixture()
        self.stop_handoff_host(row)
        shim = self.work / "tmux"
        shim.write_text(f"""#!{sys.executable}
import os, signal, sys
from pathlib import Path

if "new-session" in sys.argv[1:]:
    pid = os.getppid()
    parent = (Path("/proc") / str(pid) / "cmdline").read_bytes()
    assert b"--tmux-backend" in parent.split(bytes([0]))
    Path("handoff-before-create").touch()
    os.kill(pid, signal.SIGKILL)
    sys.exit(2)
os.execv({TMUX!r}, [{TMUX!r}, *sys.argv[1:]])
""")
        shim.chmod(0o700)
        args = ("handoff", row["dispatch"], "--host", "opencode", "--owner-token", token)
        result = json.loads(self.reclaim_cli(*args, PATH=str(self.work) + ":" + self.env["PATH"]).stdout)
        self.assertTrue((self.work / "handoff-before-create").is_file())
        self.assertEqual(result["result"], "uncertain")
        status = self.reclaim_status(row)
        self.assertNotIn("startupFailure", status)
        event, = status["audit"]
        self.assertEqual((event["phase"], event["oldWriterStopped"], event["close"]["resourceCleanup"],
                          event["fromAttempt"], event["toAttempt"], status["state"]),
                         ("starting", True, "confirmed", row["attempt"], result["attempt"], "unknown"))
        old_archive = self.state / "dispatches" / f"{row['dispatch']}.{row['attempt']}.closed"
        self.assertTrue(old_archive.is_file())
        replay = json.loads(self.reclaim_cli(*args).stdout)
        self.assertEqual((replay["result"], replay["audit"], replay["attempt"]),
                         ("uncertain", result["audit"], result["attempt"]))
        self.assertIn("handoff audit", self.reclaim_cli("close", row["dispatch"], "--owner-token", token,
                                                       code=1).stderr)
        self.assertIn("handoff audit", self.reclaim_cli("takeover", "--scope", row["scope"], "--owner", "B",
                                                       code=1).stderr)
        self.assertEqual(len(list(self.work.glob("*.ready"))), 1)
        subprocess.run(self.tmux + ["has-session", "-t", "=sibling"], env=self.env, check=True, timeout=5)
        print(json.dumps({"abandonedLaunch": result, "sameAudit": replay["audit"], "oldClose": event["close"]}),
              flush=True)

    def test_public_handoff_never_launches_while_the_original_writer_survives(self):
        row, token, original = self.handoff_fixture(detached_descendant=True)
        args = ("handoff", row["dispatch"], "--host", "opencode", "--owner-token", token)
        receipt = Path(row["create"]["receipt"])
        stopped = receipt.with_suffix(".stopped")
        try:
            (self.work / f"{row['attempt']}.root-exit").touch()
            self.handoff_until(lambda: json.loads(receipt.read_bytes())["category"] == "process_exited")
            subprocess.run(self.tmux + ["kill-session", "-t", "=" + row["tmux"]],
                           env=self.env, check=True, timeout=5)
            self.assertFalse(stopped.exists())
            result = json.loads(self.reclaim_cli(*args).stdout)
            self.assertEqual(result["result"], "uncertain")
            self.assertIn("original writer has not stopped", result["diagnostic"])
            os.kill(original["descendant"], 0)
            self.assertEqual(len(list(self.work.glob("*.ready"))), 1)
            event, = self.reclaim_status(row)["audit"]
            self.assertEqual((event["phase"], event["fromAttempt"], event["result"]),
                             ("closing", row["attempt"], "pending"))
            old_archive = self.state / "dispatches" / f"{row['dispatch']}.{row['attempt']}.closed"
            self.assertEqual(json.loads(old_archive.read_bytes())["resourceCleanup"], "confirmed")
            refused = self.reclaim_cli("ask", "old writer still active", "--attempt", row["attempt"],
                                       code=1, **original["callback"])
            self.assertIn("handoff is closing the old execution", refused.stderr)
            replay = json.loads(self.reclaim_cli(*args).stdout)
            self.assertEqual((replay["result"], replay["audit"]), ("uncertain", event["id"]))
        finally:
            (self.work / f"{row['attempt']}.stop").touch()
            self.handoff_until(stopped.is_file)
        print(json.dumps({"sessionClosed": True, "writerSurvived": True, "newLaunches": 0,
                          "audit": event["id"]}), flush=True)

    def test_public_handoff_blocks_owner_if_intent_cannot_be_published(self):
        self.assertNotEqual(os.geteuid(), 0, "read-only fixture requires an ordinary user")
        row, token, _ = self.handoff_fixture()
        dispatches = self.state / "dispatches"
        path = dispatches / f"{row['dispatch']}.json"
        original = path.read_bytes()
        with read_only_tree(dispatches):
            failed = self.reclaim_cli("handoff", row["dispatch"], "--host", "opencode",
                                      "--owner-token", token, code=1)
            self.assertIn("Permission denied", failed.stderr)
        order, = self.state.glob("handoff-order-*.json")
        self.assertEqual(json.loads(order.read_text())["fromAttempt"], row["attempt"])
        denied = self.reclaim_cli("takeover", "--scope", row["scope"], "--owner", "B", code=1)
        self.assertIn("handoff order identity mismatch", denied.stderr)
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(len(list(self.work.glob("*.ready"))), 1)
        self.active_pane(row["dispatch"])
        print(json.dumps({"failedIntentWrite": True, "sameAssignment": True, "takeoverBlocked": True}),
              flush=True)

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
