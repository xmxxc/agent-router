"""Use one WFC ELF and two isolated synthetic hosts for a default bundled handoff."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


HOST = r"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

name = sys.argv[1]
if name == "opencode":
    previous = json.loads(Path("original.json").read_text())
    assert not Path(previous["group"]).exists()
    try:
        fields = Path(f"/proc/{previous['worker']['pid']}/stat").read_text().rsplit(") ", 1)[1].split()
    except FileNotFoundError:
        pass
    else:
        assert fields[0] in ("Z", "X") or int(fields[19]) != previous["worker"]["start_ticks"]
    assert "--prompt" in sys.argv
    brief = sys.argv[sys.argv.index("--prompt") + 1]
    assert "handed off to a new host" in brief and os.environ["WFC_TASK"] in brief
assert "WFC_CONTROLLER_TOKEN" not in os.environ
with Path(f"{name}-starts").open("a") as log:
    log.write(os.environ["WFC_ATTEMPT"] + "\n")
for _ in range(600):
    if Path("finish").exists():
        if name == "opencode":
            Path(os.environ["WFC_REPORT"]).write_text("original task completed by the replacement\n")
            result = subprocess.check_output([
                os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                "--attempt", os.environ["WFC_ATTEMPT"], "--message-id", "handoff-done",
            ])
            Path("receipt.tmp").write_bytes(result)
            Path("receipt.tmp").replace("receipt.json")
        break
    time.sleep(.05)
"""


def until(check):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if value := check():
            return value
        time.sleep(.04)
    raise AssertionError("bundled handoff fixture timed out")


def exercise(binary, root):
    state = root / "state"
    home = root / "home"
    home.mkdir()
    task = root / "task.md"
    task.write_text("a controlled original task\n")
    report = root / "report.md"
    (root / "worker.py").write_text(HOST)
    config = root / "launch.json"
    config.write_text(json.dumps({"hosts": {host: {"command": [
        sys.executable, str(root / "worker.py"), host
    ]} for host in ("codex", "opencode")}}))
    env = {"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8", "HOME": str(home), "WFC_STATE": str(state)}
    scope = "handoff-fixture"

    def cli(*args, code=0):
        result = subprocess.run([binary, *map(str, args)], cwd=root, env=env,
                                capture_output=True, text=True, timeout=35)
        assert result.returncode == code, (args, result.returncode, result.stderr)
        return json.loads(result.stdout) if code == 0 else result.stderr

    owner = cli("acquire", "--scope", scope, "--owner", "fixture", "--lease", "180")
    env["WFC_CONTROLLER_TOKEN"] = owner["token"]
    original = cli("dispatch", "--host", "codex", "--scope", scope, "--cwd", root, "--task", task,
                   "--report", report, "--launch-config", config)
    dispatch = original["dispatch"]
    old = original["create"]
    old_group = old["identity"]["containment"]["path"]
    old_worker = old["identity"]["worker"]
    try:
        until((root / "codex-starts").is_file)
        (root / "original.json").write_text(json.dumps({"group": old_group, "worker": old_worker}))
        args = ("handoff", dispatch, "--host", "opencode", "--backend", "bundled")
        assert "without --task or --report" in cli(*args, "--task", task, "--report", report, code=1)
        assert not (root / "opencode-starts").exists()
        handoff = cli(*args)
        assert (handoff["result"], handoff["round"], handoff["fromAttempt"]) == (
            "accepted", 1, original["attempt"]), handoff
        assert handoff["attempt"] != original["attempt"]
        assert cli(*args) == {**handoff, "duplicate": True}
        until((root / "opencode-starts").is_file)
        assert (root / "codex-starts").read_text().splitlines() == [original["attempt"]]
        assert (root / "opencode-starts").read_text().splitlines() == [handoff["attempt"]]
        current, = cli("status", "--dispatch", dispatch)["dispatches"]
        assert current["backendHandle"] == handoff["ticket"]
        assert current["scope"] == scope and current["task"] == str(task) and current["report"] == str(report)
        audit = current["audit"][handoff["audit"] - 1]
        assert audit["phase"] == "complete" and audit["priorCreate"] == old
        assert audit["close"]["ticket"] == old["ticket"]
        assert audit["close"]["proof"] == audit["writerStopEvidence"]
        assert audit["writerStopEvidence"]["proves"] == "execution-empty"
        assert not Path(old_group).exists()
        (root / "finish").touch()
        until((root / "receipt.json").is_file)
        receipt = json.loads((root / "receipt.json").read_text())
        received, = cli("wait", "--scope", scope, "--dispatch", dispatch, "--timeout", 3)["messages"]
        assert (received["id"], received["messageId"], received["attempt"]) == (
            receipt["message"], receipt["messageId"], handoff["attempt"])
        assert cli("ack", received["messageId"])["state"] == "processed"
        assert report.read_text() == "original task completed by the replacement\n"
        assert cli(*args) == {**handoff, "duplicate": True}
        closed = cli("close", dispatch)
        assert closed["result"] == "completed" and closed["ticket"] == handoff["ticket"]
        assert closed["cleanup"] == "confirmed"
        print("BUNDLED_CODEX_OPENCODE_HANDOFF_PASS", dispatch, handoff["audit"])
    finally:
        (root / "finish").touch()
        subprocess.run([binary, "close", dispatch], cwd=root, env=env, capture_output=True, timeout=20)


if __name__ == "__main__":
    executable = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="wfc-bundled-handoff-") as work:
        exercise(executable, Path(work).resolve())
