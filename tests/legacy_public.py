"""Run the public Linux wfc read-only contract against private state directories."""
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import subprocess
import sys
import tempfile
from contextlib import closing


DISPATCH = "d-00000030"
LEGACY_INPUTS = (("unset", None), ("empty", ""), ("tilde", "~/custom"),
                 ("relative", "relative-state"), ("absolute", "ABSOLUTE"), ("symlink", "state-link"))
COMMANDS = (("status",), ("status", "--active"), ("status", "--dispatch", DISPATCH),
            ("reclaim",), ("reclaim", "--dispatch", DISPATCH))


def snapshot(root):
    def entry(path):
        if path.is_symlink():
            return "link", os.readlink(path)
        if path.is_dir():
            return "directory",
        return "file", hashlib.sha256(path.read_bytes()).hexdigest()
    return {str(path.relative_to(root)): entry(path) for path in root.rglob("*")}


def run(binary, cwd, home, setting, args, code=0):
    env = {"HOME": str(home), "PATH": os.defpath}
    if setting is not None:
        env["WFC_STATE"] = setting
    result = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=30)
    assert result.returncode == code, (args, setting, result)
    if code:
        assert not result.stdout and result.stderr, result
        return result.stderr
    assert not result.stderr, result
    return json.loads(result.stdout)


def seed_legacy(state, dispatch=DISPATCH):
    state.mkdir(parents=True, exist_ok=True)
    with closing(sqlite3.connect(state / "router.db")) as db:
        db.executescript(f"""
            CREATE TABLE dispatch(id TEXT PRIMARY KEY, current_round INTEGER, current_attempt TEXT);
            INSERT INTO dispatch VALUES('{dispatch}',1,'a-000000000030');
            CREATE TABLE dispatch_round(dispatch TEXT, round INTEGER, attempt TEXT, state TEXT);
            INSERT INTO dispatch_round VALUES('{dispatch}',1,'a-000000000030','completed');
            CREATE TABLE message(id INTEGER PRIMARY KEY, dispatch TEXT, recipient TEXT, type TEXT, body TEXT);
            INSERT INTO message VALUES(30,'{dispatch}','controller','done','{{"report":"/old/report"}}');
        """)


def legacy_case(binary, label, setting):
    with tempfile.TemporaryDirectory(prefix="wfc-public-legacy-") as tmp:
        root = Path(tmp).resolve()
        home, cwd = root / "home", root / "cwd"
        home.mkdir()
        cwd.mkdir()
        legacy_default = home / ".local/state/workflow-core"
        native_default = home / ".local/state/agent-router"
        if label == "tilde":
            legacy = home / "custom"
        elif label == "relative":
            legacy = cwd / setting
        elif label == "absolute":
            legacy = root / "explicit"
            setting = str(legacy)
        elif label == "symlink":
            legacy = root / "target"
            (cwd / setting).symlink_to(legacy, target_is_directory=True)
        else:
            legacy = legacy_default

        before = snapshot(root)
        native_empty = {"unset": native_default, "tilde": cwd / "~/custom", "symlink": cwd / "state-link"}
        for args in (("status",), ("status", "--active"), ("reclaim",)):
            view = run(binary, cwd, home, setting, args)
            state = native_empty.get(label, legacy) if args[0] == "status" else legacy
            collection = "dispatches" if args[0] == "status" else "sessions"
            assert view["state"] == str(state) and view[collection] == [], view
        assert snapshot(root) == before, (label, "read commands created a file")

        seed_legacy(legacy)
        native = native_empty.get(label, legacy)
        (native / "dispatches").mkdir(parents=True)
        (native / "relays").mkdir()
        before = snapshot(root)
        for args in COMMANDS:
            view = run(binary, cwd, home, setting, args)
            assert view["state"] == str(legacy) and "Older router.db schema" in view["schema"], view
            if args[0] == "reclaim":
                item, = view["sessions"]
                assert view["yes"] is False and item["dispatch"] == DISPATCH
                assert item["reclaimable"] is False and item["result"] == "unknown", item
            elif args[1:] == ("--active",):
                assert view["dispatches"] == view["reclaimable"] == [], view
            else:
                item, = view["dispatches"]
                assert item["dispatch"] == DISPATCH and item["backendAlive"] is None, item
                if "--dispatch" in args:
                    receipt, = item["messages"]
                    assert receipt["id"] == 30 and receipt["body"] == '{"report":"/old/report"}', receipt
            assert snapshot(root) == before, (label, args, "ledger changed")
        for command in ("status", "reclaim"):
            err = run(binary, cwd, home, setting, (command, "--dispatch", "d-000000ff"), code=1)
            assert "unknown dispatch d-000000ff" in err, err
        run(binary, cwd, home, setting, ("reclaim", "--yes"), code=2)
        run(binary, cwd, home, setting, ("reclaim", "--dispatch"), code=2)
        assert snapshot(root) == before
        print(label, "selected", legacy, "five legacy reads, errors, no writes")


def native_precedence(binary):
    with tempfile.TemporaryDirectory(prefix="wfc-public-native-") as tmp:
        root = Path(tmp).resolve()
        home, cwd = root / "home", root / "cwd"
        cwd.mkdir()
        seed_legacy(home / ".local/state/workflow-core")
        for setting, state in ((None, home / ".local/state/agent-router"), ("~/custom", cwd / "~/custom"),
                               ("native", cwd / "native")):
            dispatches = state / "dispatches"
            dispatches.mkdir(parents=True)
            did = "d-00000024"
            record = dispatches / f"{did}.json"
            record.write_text(json.dumps({"dispatch": did, "attempt": "a-000000000024",
                                          "round": 1, "label": "native"}))
            if setting == "native":
                seed_legacy(state)
            elif setting == "~/custom":
                seed_legacy(home / "custom")
            before = snapshot(root)
            for args in (("status",), ("status", "--active"), ("status", "--dispatch", did)):
                view = run(binary, cwd, home, setting, args)
                assert view["state"] == str(state) and "schema" not in view, view
                if "--active" in args:
                    assert view["dispatches"] == view["reclaimable"] == [], view
                else:
                    entry, = view["dispatches"]
                    assert entry["dispatch"] == did and entry["label"] == "native", entry
            assert snapshot(root) == before
            print(setting, "native status selected", state, "when both paths contain records")


def native_dispatch_traverse_only(binary):
    assert os.geteuid() != 0, "run the directory permissions fixture as an ordinary user"
    with tempfile.TemporaryDirectory(prefix="wfc-public-traverse-") as tmp:
        root = Path(tmp).resolve()
        dispatch = "d-00000000000000000000000000000024"
        seed_legacy(root, dispatch)
        ledger = root / "router.db"
        ledger.chmod(0o400)
        dispatches = root / "dispatches"
        dispatches.mkdir(mode=0o700)
        record = dispatches / f"{dispatch}.json"
        native = {"dispatch": dispatch, "attempt": "a-000000000024", "round": 1, "label": "native"}
        record.write_text(json.dumps(native))
        record.chmod(0o400)
        before = snapshot(root)
        dispatches.chmod(0o100)
        try:
            assert json.loads(record.read_text()) == native
            try:
                os.listdir(dispatches)
            except PermissionError as error:
                assert error.errno == errno.EACCES
            else:
                raise AssertionError("the fixture unexpectedly allows directory enumeration")

            args = ("status", "--dispatch", dispatch)
            view = run(binary, root, root, str(root), args)
            entry, = view["dispatches"]
            assert entry["label"] == "native" and entry["attempt"] == native["attempt"], view
            assert "schema" not in view, view
            preview = run(binary, root, root, str(root), ("reclaim", "--dispatch", dispatch))
            entry, = preview["sessions"]
            assert entry["dispatch"] == dispatch and entry["result"] == "gone", preview
            assert "schema" not in preview, preview
            for flags in ((), ("--active",)):
                error = run(binary, root, root, str(root), ("status", *flags), code=1)
                assert "list dispatch records" in error and f"os error {errno.EACCES}" in error, error
            assert stat.S_IMODE(dispatches.stat().st_mode) == 0o100
            assert stat.S_IMODE(ledger.stat().st_mode) == stat.S_IMODE(record.stat().st_mode) == 0o400
        finally:
            dispatches.chmod(0o700)
        assert snapshot(root) == before
        print("ordinary UID", os.geteuid(), "named status keeps Rust priority with traversal only; lists report EACCES")


def native_literal_path(binary):
    with tempfile.TemporaryDirectory(prefix="wfc-public-literal-") as tmp:
        root = Path(tmp).resolve()
        setting = f"~wfc-no-account-{root.name}/state"
        view = run(binary, root, root, setting, ("status",))
        assert view["state"] == str(root / setting) and view["dispatches"] == [], view
        assert not snapshot(root)
        print("an unknown ~account still works as an empty literal Rust path")


def broken_legacy_path(binary):
    with tempfile.TemporaryDirectory(prefix="wfc-public-broken-") as tmp:
        root = Path(tmp).resolve()
        default = root / ".local/state/workflow-core"
        default.parent.mkdir(parents=True)
        default.symlink_to(default.name)
        (root / "loop").symlink_to("loop")
        before = snapshot(root)
        for setting in (None, "", "~/loop"):
            for command in ("status", "reclaim"):
                error = run(binary, root, root, setting, (command,), code=1)
                assert f"os error {errno.ELOOP}" in error, error
                assert snapshot(root) == before
        print("legacy directory cycles report errors for default and explicit home paths")


def native_ledger_and_relay(binary):
    with tempfile.TemporaryDirectory(prefix="wfc-public-native-records-") as tmp:
        root = Path(tmp).resolve()
        seed_legacy(root)
        native = root / "messages.json"
        native.write_text("[]")
        before = snapshot(root)
        assert "schema" not in run(binary, root, root, str(root), ("status",))
        assert snapshot(root) == before
        native.unlink()

        relays = root / "relays"
        relays.mkdir()
        relay = relays / "fixture.json"
        registration = {"kind": "relay", "pid": os.getpid(), "since": "fixture"}
        relay.write_text(json.dumps(registration))
        before = snapshot(root)
        with relay.open("rb") as f:
            fcntl.flock(f, fcntl.LOCK_EX)
            status = run(binary, root, root, str(root), ("status",))
            assert status["waiters"] == [registration] and "schema" not in status, status
        status = run(binary, root, root, str(root), ("status",))
        assert status["dispatches"][0]["dispatch"] == DISPATCH and "schema" in status, status
        assert snapshot(root) == before
        print("a Rust message ledger or held relay registration selects the native view")


def native_ledger_directory(binary):
    with tempfile.TemporaryDirectory(prefix="wfc-public-message-directory-") as tmp:
        root = Path(tmp).resolve()
        seed_legacy(root)
        native = root / "messages.json"
        native.mkdir()
        before = snapshot(root)
        for args in COMMANDS[:3]:
            error = run(binary, root, root, str(root), args, code=1)
            assert "messages.json" in error and "not a regular file" in error, error
        preview = run(binary, root, root, str(root), ("reclaim", "--dispatch", DISPATCH))
        assert preview["sessions"][0]["dispatch"] == DISPATCH and "schema" in preview, preview
        assert preview["yes"] is False
        assert preview["sessions"][0]["result"] == "unknown"
        assert preview["sessions"][0]["reclaimable"] is False
        assert run(binary, root, root, str(root), ("reclaim",)) == preview
        unknown = run(binary, root, root, str(root), ("reclaim", "--dispatch", "d-000000ff"), code=1)
        assert "unknown dispatch d-000000ff" in unknown, unknown
        assert snapshot(root) == before

        task = root / "private-task.md"
        task.write_text("controlled worker for the reclaim refusal fixture")
        worker = run(binary, root, root, str(root), (
            "dispatch", "--role", "fixture", "--task", str(task), "--cwd", str(root),
            "--", "/bin/sleep", "90",
        ))
        target = worker["dispatch"]
        database = root / "router.db"
        parked = root / "router.fixture"

        def reject_reclaim(path_fragment, diagnostic):
            before = snapshot(root)
            for flags in ((), ("--dispatch", DISPATCH), ("--dispatch", target)):
                error = run(binary, root, root, str(root), ("reclaim", "--yes", *flags), code=1)
                assert path_fragment in error and diagnostic in error, error
                assert snapshot(root) == before

        try:
            for legacy_present in (True, False):
                if not legacy_present:
                    database.rename(parked)
                    before = snapshot(root)
                    missing = run(binary, root, root, str(root),
                                  ("reclaim", "--dispatch", DISPATCH), code=1)
                    assert f"unknown dispatch {DISPATCH}" in missing, missing
                    assert snapshot(root) == before
                reject_reclaim("messages.json", "not a regular file")
            native.rmdir()
            native.symlink_to("missing-messages-target")
            try:
                reject_reclaim("messages.json", f"os error {errno.ENOENT}")
            finally:
                native.unlink()

            # A valid native ledger and live worker do not make an ambiguous
            # old path safe to ignore for confirmation.
            native.write_text("[]")
            database.symlink_to(database.name)
            try:
                reject_reclaim("router.db", f"os error {errno.ELOOP}")
            finally:
                database.unlink()
            database.mkdir()
            try:
                reject_reclaim("router.db", "not a regular file")
            finally:
                database.rmdir()
            database.write_bytes(b"SQLite format 3\0")
            try:
                reject_reclaim("router.db", "verify")
            finally:
                database.unlink()
            for statement in ("CREATE TABLE unrelated(id INTEGER PRIMARY KEY)",
                              "CREATE TABLE message(body TEXT)"):
                with closing(sqlite3.connect(database)) as db:
                    db.execute(statement)
                    db.commit()
                try:
                    reject_reclaim("router.db", "unrecognized router.db schema")
                finally:
                    database.unlink()
            status = run(binary, root, root, str(root), ("status", "--dispatch", target))
            assert status["dispatches"][0]["backendAlive"] is True, status
        finally:
            if parked.exists():
                parked.rename(database)
            run(binary, root, root, str(root), ("close", target))
        print("a messages.json directory reports the path type error; the legacy ledger is unchanged")


def wal_refusal(binary):
    with tempfile.TemporaryDirectory(prefix="wfc-public-wal-") as tmp:
        root = Path(tmp).resolve()
        with closing(sqlite3.connect(root / "router.db")) as db:
            assert db.execute("PRAGMA journal_mode=WAL").fetchone() == ("wal",)
            db.execute("CREATE TABLE dispatch(id TEXT PRIMARY KEY)")
            db.commit()
            before = snapshot(root)
            for args in COMMANDS:
                stderr = run(binary, root, root, str(root), args, code=1)
                assert "WAL-mode ledger" in stderr and snapshot(root) == before
        print("WAL: five read commands refused without changing main file or sidecars")


if __name__ == "__main__":
    wfc = Path(sys.argv[1]).resolve()
    for label, setting in LEGACY_INPUTS:
        legacy_case(wfc, label, setting)
    native_precedence(wfc)
    native_dispatch_traverse_only(wfc)
    native_literal_path(wfc)
    broken_legacy_path(wfc)
    native_ledger_and_relay(wfc)
    native_ledger_directory(wfc)
    wal_refusal(wfc)
