"""Exercise lock-boundary refusals only on the revised public Rust binary."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from legacy_public import snapshot
from legacy_write_fence import call, command, FENCE, legacy_rows, refused, seed_owned_legacy, success


def waiting(pid, path, nested):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        try:
            if nested:
                pids = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
            else:
                pids = [pid]
            for current in pids:
                if any(os.path.realpath(fd) == str(path) for fd in Path(f"/proc/{current}/fd").iterdir()):
                    return
        except FileNotFoundError:
            pass
        time.sleep(.02)
    raise AssertionError(f"operation did not wait on {path.name}")


def race(binary, root, setting, lock_path, legacy, args, *, nested=False):
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path.touch(mode=0o600)
    argv, env = command(binary, root, setting, *args)
    with lock_path.open("r+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with subprocess.Popen(argv, cwd=root, env=env, text=True, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE) as child:
            try:
                waiting(child.pid, lock_path, nested)
                database = seed_owned_legacy(legacy)
                before, rows = snapshot(root), legacy_rows(database)
                fcntl.flock(lock, fcntl.LOCK_UN)
                out, err = child.communicate(timeout=12)
            finally:
                if child.poll() is None:
                    child.kill()
    result = subprocess.CompletedProcess(argv, child.returncode, out, err)
    if args[0] == "reclaim":
        assert result.returncode == 1 and FENCE in result.stderr, result
        item, = json.loads(out)["sessions"]
        assert item["result"] == "failed" and FENCE in item["error"], item
    else:
        refused(result)
    assert snapshot(root) == before and legacy_rows(database) == rows
    return database


def controlled(binary, root, operation):
    home = root / "home"
    home.mkdir()
    state = home / ".local/state/agent-router"
    legacy = home / ".local/state/workflow-core"
    task = root / "task.md"
    task.write_text("fixture task")
    row = success(call(binary, root, None, "dispatch", "--role", "worker", "--task", task,
                       "--cwd", root, "--", "/bin/sleep", "45"))
    dispatch, attempt = row["dispatch"], row["attempt"]
    args = ("close", dispatch) if operation == "close" else ("reclaim", "--yes", "--dispatch", dispatch)
    try:
        race(binary, root, None, state / "close-locks" / f"{dispatch}.{attempt}", legacy, args)
        status = success(call(binary, root, None, "status", "--dispatch", dispatch))
        assert status["dispatches"][0]["backendAlive"] is True
    finally:
        database = legacy / "router.db"
        if database.exists():
            database.rename(root / "offline-fixture.db")
        success(call(binary, root, None, "close", dispatch))
    before = snapshot(root)
    for directory in (state, state / "dispatches"):
        directory.chmod(0o500)
    try:
        closed = success(call(binary, root, None, "close", dispatch))
        assert closed["result"] == "completed" and closed["attempt"] == attempt
    finally:
        for directory in (state, state / "dispatches"):
            directory.chmod(0o700)
    assert snapshot(root) == before
    print(f"{operation}: attempt-lock refusal preserves live worker and archive replays read-only")


def hosted(binary, root, adapter, setting):
    home = root / "home"
    home.mkdir()
    if setting is None:
        state = home / ".local/state/agent-router"
        legacy = home / ".local/state/workflow-core"
    else:
        state = root / "~/alt"
        legacy = home / "alt"
    dispatch = "d-00000031"
    records = state / "dispatches"
    records.mkdir(parents=True)
    row = {"dispatch": dispatch, "round": 1, "attempt": "a-000000000031", "scope": "fixture-scope",
           "host": "codex", "backend": adapter}
    if adapter == "tmux":
        # The private backend must never contact an existing user's server.
        row["tmuxSocket"] = str(root / "no-server.sock")
        lock_path = state / "tmux" / f"{dispatch}.lock"
        args = ("close", dispatch)
    else:
        lock_path = records / f"{dispatch}.herdr-lock"
        args = ("herdr-reconcile", dispatch, "--pane", "fixture", "--terminal", "fixture",
                "--actor", "fixture", "--evidence", "fixture")
    (records / f"{dispatch}.json").write_text(json.dumps(row))
    race(binary, root, setting, lock_path, legacy, args, nested=adapter == "tmux")
    before = snapshot(root)
    detail = success(call(binary, root, setting, "status", "--dispatch", dispatch))
    assert detail["dispatches"][0]["backendAlive"] is None
    assert snapshot(root) == before
    print(f"{adapter} {setting}: inner-lock refusal sees the inherited legacy path; history stays read-only")


if __name__ == "__main__":
    assert os.geteuid() != 0
    binary = Path(sys.argv[1]).resolve(strict=True)
    cases = [(controlled, (operation,)) for operation in ("close", "reclaim")]
    cases.extend((hosted, (adapter, setting)) for adapter, setting in (
        ("herdr", None), ("tmux", None), ("tmux", "~/alt"),
    ))
    for run, args in cases:
        with tempfile.TemporaryDirectory(prefix="wfc-inner-fence-") as directory:
            root = Path(directory).resolve()
            run(binary, root, *args)
        assert not root.exists()
