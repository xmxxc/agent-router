"""Replay a fixed workflow-core #39 or #40 checkout against the Rust binary.

python3 python_ledger.py /path/to/python-checkout /path/to/wfc-legacy-state
"""
import hashlib
import json
import os
from pathlib import Path
import pwd
import sqlite3
import subprocess
import sys
import tempfile


def run(source, binary):
    revision = subprocess.check_output(["git", "-C", source, "rev-parse", "HEAD"], text=True).strip()
    assert revision in {"2409c40b469e0e40bd5c55326df3d5ebb86ca81b",
                        "60d7d76e4bb861ac9e0bbb043aa1a36946578397"}
    for key in list(os.environ):
        if key.startswith("WFC_"):
            del os.environ[key]
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    sys.dont_write_bytecode = True
    sys.path[:0] = [str(source / "tests"), str(source)]
    from test_router import Env

    # The reference suite supplies its own host/tmux shims and private state.
    env = Env()
    try:
        # Compare Python's path resolution in a disposable tree, including a
        # named account from the system user database rather than the fake HOME.
        sandbox = env.cwd
        (sandbox / "alias").symlink_to("missing/target")
        account = pwd.getpwuid(os.getuid())
        named = f"~{account.pw_name}/" + os.path.relpath(sandbox / "named", account.pw_dir)
        for value in ["~/copy", "./~/copy", "alias/state", named, "~wfc-no-such-user-3ef7f43c/copy"]:
            settings = {"HOME": str(sandbox), "WFC_STATE": value}
            python = env.run("status", env=settings, cwd=sandbox)
            rust_path = subprocess.run([binary, "status"], env=env.router_env(env=settings),
                                       cwd=sandbox, capture_output=True, text=True, check=False)
            assert rust_path.returncode == python.returncode
            if value.startswith("~wfc-no-such"):
                assert python.returncode != 0 and "home directory" in rust_path.stderr
            else:
                actual = json.loads(rust_path.stdout)["state"]
                assert actual == json.loads(python.stdout)["state"] and not Path(actual).exists()
        token = env.json("owner", "claim", "--scope", "verified")["token"]
        dispatch = env.dispatch(extra_args=["--scope", "verified", "--owner-token", token])
        auth = ["--owner-token", token]
        env.report.write_text("# first complete report\n")
        first = env.json("done", "--report", str(env.report), *auth, dispatch=dispatch)
        [received] = env.json("wait", "--scope", "verified", *auth, "--timeout", "1")["messages"]
        task = env.task.with_name("r2.md")
        task.write_text("second task\n")
        env.json("continue", dispatch, "--task", str(task), "--report", str(env.report.with_name("r2.md")), *auth)
        current = env.json("status", "--dispatch", dispatch)["dispatches"][0]
        ask = env.json("ask", "second question", "--attempt", current["attempt"], *auth, dispatch=dispatch)
        env.json("reply", str(ask["message"]), "yes", *auth)
        with sqlite3.connect(env.state / "router.db") as db:
            db.execute("""INSERT INTO lifecycle_event(dispatch,operation,result,owner_generation,resolver_actor,
                resolution_owner_generation,resolution_evidence,resolved_at,consumed_at)
                VALUES(?, 'handoff','accepted',1,'fixture-reviewer',2,'fixture-evidence',17.9999,19.9999)""", (dispatch,))
            # Match #40's own unknown-receipt fixture. Also simulate a historical
            # partially backfilled hash, so the modern ledger takes the write path.
            db.execute("UPDATE message SET state='unknown', delivered=0, status_version=2, body_hash=NULL WHERE id=?",
                       (ask["message"],))
        reference = env.json("status", "--dispatch", dispatch)["dispatches"][0]
        reference_reply = next(m for m in reference["messages"] if m["type"] == "reply")
        reference_audit = reference["audit"][-1]
        assert reference_reply["to"] == ask["message"]
        assert (reference_audit["ownerGeneration"],reference_audit["resolution"]["ownerGeneration"]) == (1, 2)
        with tempfile.TemporaryDirectory() as copy:
            copy = Path(copy)
            source_db = env.state / "router.db"
            copy_db = copy / "router.db"
            with sqlite3.connect(source_db) as reader, sqlite3.connect(copy_db) as writer:
                reader.backup(writer)
            original = hashlib.sha256(source_db.read_bytes()).digest()
            before = hashlib.sha256(copy_db.read_bytes()).digest()
            def rust(*args):
                return json.loads(subprocess.check_output([binary, *args], env=env.router_env(env={"WFC_STATE": str(copy)})))

            view = rust("status", "--dispatch", dispatch)
            assert token not in json.dumps(view)
            assert "schema" not in view
            assert view["dispatches"][0]["rounds"] == current["rounds"]
            assert view["dispatches"][0]["controllerOwner"]["generation"] == 1
            assert hashlib.sha256(copy_db.read_bytes()).digest() == before
            backup = rust("migrate", "--offline-copy", copy)["backup"]
            assert Path(backup).is_file()
            d = rust("status", "--dispatch", dispatch)["dispatches"][0]
            assert d["rounds"] == current["rounds"] and d["attempt"] == current["attempt"]
            assert d["rounds"][0]["state"] == "completed" and d["rounds"][1]["state"] == "running"
            by_id = {m["id"]: m for m in d["messages"]}
            msg = by_id[first["message"]]
            assert (msg["messageId"], msg["state"], msg["statusVersion"]) == (
                received["messageId"], "received", received["statusVersion"])
            assert by_id[reference_reply["id"]]["to"] == reference_reply["to"]
            assert d["audit"][-1]["ownerGeneration"] == reference_audit["ownerGeneration"]
            assert d["audit"][-1]["resolution"] == reference_audit["resolution"]
            unknown = by_id[ask["message"]]
            assert unknown["state"] == "unknown" and unknown["statusVersion"] == 2
            assert unknown["bodyHash"] == hashlib.sha256(unknown["body"].encode()).hexdigest()
            # Compare every SQL record including callback credentials and owner secrets.
            with sqlite3.connect(source_db) as a, sqlite3.connect(copy_db) as b:
                b.execute("BEGIN")
                b.execute("UPDATE message SET body_hash=NULL WHERE id=?", (ask["message"],))
                assert list(a.iterdump()) == list(b.iterdump())
                b.rollback()
            assert rust("migrate", "--offline-copy", copy)["backup"] is None
            # On the disposable upgraded DB, the reference owner fence still applies.
            copy_env = {"WFC_STATE": str(copy)}
            bad = env.run("ack", msg["messageId"], "--owner-token", "bad", env=copy_env)
            assert bad.returncode == 1 and "stale_controller_owner" in bad.stderr
            ack = env.json("ack", msg["messageId"], *auth, env=copy_env)
            assert ack["state"] == "processed" and ack["statusVersion"] == msg["statusVersion"] + 1
            assert hashlib.sha256(source_db.read_bytes()).digest() == original
        print(f"{revision[:8]}: paths, two rounds, claimed scope, received/unknown, reply target, resolution generation, backup, full SQL comparison, Python ack OK")
    finally:
        env.close()


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
