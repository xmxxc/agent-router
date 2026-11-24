"""Check the public write fence in private Linux state directories."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import sqlite3
import subprocess
import sys
import tempfile
import time

from legacy_public import DISPATCH, seed_legacy, snapshot


SCOPE = "fixture-scope"
TOKEN = "fixture-owner-token"
FENCE = "legacy state write blocked"


def command(binary, root, setting, *args, worker=None):
    env = {"PATH": os.defpath, "HOME": str(root / "home"), "WFC_OWNER_TOKEN": TOKEN}
    if setting is not None:
        env["WFC_STATE"] = str(setting)
    if worker:
        env.update(WFC_DISPATCH=worker["dispatch"], WFC_ROUND=str(worker["round"]), WFC_ATTEMPT=worker["attempt"])
    return [binary, *args], env


def call(binary, root, setting, *args, worker=None):
    argv, env = command(binary, root, setting, *args, worker=worker)
    return subprocess.run(argv, cwd=root, env=env, text=True, capture_output=True, timeout=15)


def success(result):
    assert result.returncode == 0 and not result.stderr, result
    return json.loads(result.stdout)


def refused(result, diagnostic="router.db"):
    assert result.returncode == 1 and not result.stdout, result
    assert FENCE in result.stderr and diagnostic in result.stderr and "isolated WFC_STATE" in result.stderr, result


def legacy_rows(path):
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as db:
        return tuple(db.iterdump())


def seed_owned_legacy(state):
    seed_legacy(state)
    path = state / "router.db"
    with closing(sqlite3.connect(path)) as db, db:
        db.execute("ALTER TABLE dispatch ADD COLUMN scope TEXT")
        db.execute("UPDATE dispatch SET scope=?", (SCOPE,))
        db.execute("""CREATE TABLE controller_owner (scope TEXT PRIMARY KEY, owner TEXT, token TEXT,
                      generation INTEGER, lease_until REAL, updated REAL)""")
        db.execute("INSERT INTO controller_owner VALUES (?, ?, ?, 7, ?, 10)",
                   (SCOPE, "fixture-owner", TOKEN, time.time() + 600))
    return path


def aliases(binary, root):
    home = root / "home"
    home.mkdir(parents=True)
    database = seed_owned_legacy(home / ".local/state/workflow-core")
    old_state = database.parent
    account = pwd.getpwuid(os.getuid())
    named_home = f"~{account.pw_name}/{os.path.relpath(old_state, account.pw_dir)}"
    (root / "state-link").symlink_to(old_state, target_is_directory=True)
    (root / "absolute-link").symlink_to(old_state, target_is_directory=True)
    settings = (None, "", "~/.local/state/workflow-core", "home/.local/state/workflow-core",
                old_state, "state-link", root / "absolute-link", named_home)
    before = snapshot(root)
    original_rows = legacy_rows(database)
    for setting in settings:
        view = success(call(binary, root, setting, "status", "--dispatch", DISPATCH))
        row, = view["dispatches"]
        receipt, = row["messages"]
        assert receipt["id"] == 30 and row["controllerOwner"]["generation"] == 7
        preview = success(call(binary, root, setting, "reclaim", "--dispatch", DISPATCH))
        assert preview["yes"] is False and preview["sessions"][0]["reclaimable"] is False
        refused(call(binary, root, setting, "owner", "acquire", "--scope", SCOPE, "--owner", "fixture-owner"))
        refused(call(binary, root, setting, "reclaim", "--yes", "--dispatch", DISPATCH))
        assert snapshot(root) == before
    with ThreadPoolExecutor(max_workers=4) as pool:
        futures = [pool.submit(call, binary, root, setting, "takeover", "--scope", SCOPE,
                               "--owner", "fixture-owner") for setting in settings]
        for future in futures:
            refused(future.result())
    assert snapshot(root) == before and legacy_rows(database) == original_rows
    print(f"legacy-only: {len(settings)} path selections, read-only history, concurrent owner rejection")


def mixed_and_native(binary, root):
    (root / "home").mkdir(parents=True)
    # An unrelated default legacy directory does not affect an explicit target.
    seed_owned_legacy(root / "home/.local/state/workflow-core")
    target = root / "native"
    task, report = root / "task.md", root / "report.md"
    task.write_text("fixture task\n")
    report.write_text("fixture report\n")
    configure_owner = ("owner", "acquire", "--scope", SCOPE, "--owner", "fixture-owner", "--token", TOKEN)
    assert success(call(binary, root, target, *configure_owner))["generation"] == 1
    spawn = ("dispatch", "--role", "worker", "--task", task, "--cwd", root, "--scope", SCOPE)
    assignment = success(call(binary, root, target, *spawn, "--report", report, "--", "/bin/true"))
    dispatch, attempt = assignment["dispatch"], assignment["attempt"]
    try:
        done = success(call(binary, root, target, "done", "--report", report, "--attempt", attempt, worker=assignment))
        received, = success(call(binary, root, target, "wait", "--scope", SCOPE, "--timeout", "0"))["messages"]
        assert received["messageId"] == done["messageId"] and received["state"] == "received"
        ack = success(call(binary, root, target, "ack", done["messageId"]))
        assert ack["state"] == "processed" and ack["statusVersion"] == received["statusVersion"] + 1
    finally:
        success(call(binary, root, target, "close", dispatch))
    historical = success(call(binary, root, target, "status", "--dispatch", dispatch))
    database = seed_owned_legacy(target)
    rows = legacy_rows(database)
    hosted = "d-00000031"
    (target / "dispatches" / f"{hosted}.json").write_text(json.dumps({
        "dispatch": hosted, "round": 1, "attempt": "a-000000000031", "host": "codex",
        "backend": "tmux", "tmuxSocket": str(root / "unused.sock"),
    }))
    (root / "mixed-link").symlink_to(target, target_is_directory=True)
    before = snapshot(root)
    commands = (
        (configure_owner, None),
        (("renew", "--scope", SCOPE), None),
        (("takeover", "--scope", SCOPE, "--owner", "fixture-owner"), None),
        ((*spawn, "--report", root / "never-created/report.md", "--", "/bin/true"), None),
        (("wait", "--scope", SCOPE, "--timeout", "0"), None),
        (("done", "--report", report, "--attempt", attempt), assignment),
        (("ask", "fixture question", "--attempt", attempt), assignment),
        (("ack", done["messageId"]), None),
        (("resolve", done["messageId"], "--actor", "fixture", "--reason", "confirmed",
          "--evidence", "fixture", "--body-hash", "a" * 64), None),
        (("relay", "--codex-thread", "11111111-2222-4333-8444-555555555555", "--scope", SCOPE,
          "--detach", "--for", "1"), None),
        (("resume", dispatch), None),
        (("herdr-reconcile", dispatch, "--pane", "pane", "--terminal", "terminal", "--actor", "fixture",
          "--evidence", "fixture"), None),
        (("close", dispatch), None),
        (("reclaim", "--yes", "--dispatch", dispatch), None),
    )
    for args, worker in commands:
        refused(call(binary, root, "mixed-link", *args, worker=worker))
        assert snapshot(root) == before, args[0]
    current = success(call(binary, root, target, "status", "--dispatch", dispatch))
    assert current == historical
    hosted_view = success(call(binary, root, target, "status", "--dispatch", hosted))
    assert hosted_view["dispatches"][0]["backendAlive"] is None
    preview = success(call(binary, root, target, "reclaim", "--dispatch", dispatch))
    assert preview["yes"] is False and preview["sessions"][0]["result"] == "gone"
    assert snapshot(root) == before and legacy_rows(database) == rows
    print(f"native-only: owner/dispatch/done/wait/ack/close. Mixed: {len(commands)} writes refused, exact history retained")


def ambiguous(binary, root):
    (root / "home").mkdir(parents=True)
    state = root / "state"
    state.mkdir()
    database = state / "router.db"
    owner = ("owner", "acquire", "--scope", SCOPE, "--owner", "fixture-owner")
    for make_entry in (lambda: database.write_bytes(b"bad SQLite"),
                       lambda: database.symlink_to("missing"), lambda: database.symlink_to("router.db"),
                       database.mkdir):
        make_entry()
        before = snapshot(root)
        refused(call(binary, root, state, *owner))
        assert snapshot(root) == before
        if not database.is_symlink() and database.is_dir():
            database.rmdir()
        else:
            database.unlink()
    for name in ("router.db-wal", "router.db-shm", "router.db-journal"):
        marker = state / name
        marker.touch()
        before = snapshot(root)
        refused(call(binary, root, state, *owner), name)
        assert snapshot(root) == before
        marker.unlink()
    before = snapshot(root)
    state.chmod(0)
    try:
        refused(call(binary, root, state, *owner))
        assert state.stat().st_mode & 0o777 == 0
    finally:
        state.chmod(0o755)
    assert snapshot(root) == before
    loop = root / "loop"
    loop.symlink_to(loop.name, target_is_directory=True)
    before = snapshot(root)
    refused(call(binary, root, loop, *owner))
    account = pwd.getpwuid(os.getuid())
    named_loop = f"~{account.pw_name}/{os.path.relpath(loop, account.pw_dir)}"
    refused(call(binary, root, named_loop, *owner), "Python state path")
    assert snapshot(root) == before
    print("ambiguous: malformed database, links, directory, recovery sidecars, EACCES and state loop fail closed")


def after_waiting_for_scope(binary, root):
    (root / "home").mkdir(parents=True)
    state = root / "state"
    state.mkdir()
    lock_path = state / ("relay-" + hashlib.sha256(SCOPE.encode()).hexdigest() + ".lock")
    lock_path.touch(mode=0o600)
    argv, env = command(binary, root, state, "owner", "acquire", "--scope", SCOPE, "--owner", "fixture-owner")
    with lock_path.open("r+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with subprocess.Popen(argv, cwd=root, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
            try:
                deadline = time.monotonic() + 5
                fds = Path(f"/proc/{child.pid}/fd")
                while not any(os.path.realpath(fd) == str(lock_path) for fd in fds.iterdir()):
                    assert child.poll() is None and time.monotonic() < deadline, "worker did not wait on scope lock"
                    time.sleep(0.02)
                database = seed_owned_legacy(state)
                before = snapshot(root)
                rows = legacy_rows(database)
                fcntl.flock(lock, fcntl.LOCK_UN)
                out, err = child.communicate(timeout=10)
                refused(subprocess.CompletedProcess(argv, child.returncode, out, err))
            finally:
                if child.poll() is None:
                    child.kill()
    assert snapshot(root) == before and legacy_rows(database) == rows
    print("late legacy selection: waiting owner rechecks before creating message lock or owner record")


if __name__ == "__main__":
    assert os.geteuid() != 0
    binary = Path(sys.argv[1]).resolve(strict=True)
    for check in (aliases, mixed_and_native, ambiguous, after_waiting_for_scope):
        with tempfile.TemporaryDirectory(prefix="wfc-legacy-fence-") as path:
            root = Path(path).resolve()
            check(binary, root)
        assert not root.exists()
