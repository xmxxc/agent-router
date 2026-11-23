"""Verify repaired and refused private copies of an actual Python #39/#40 two-round ledger."""
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import closing
from pathlib import Path


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def backup(source, target):
    with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as reader:
        with closing(sqlite3.connect(target)) as writer:
            reader.backup(writer)


def dump(path):
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as db:
        assert db.execute("PRAGMA integrity_check").fetchone() == ("ok",)
        return list(db.iterdump())


def run(source, binary):
    version = subprocess.check_output(["git", "-C", source, "rev-parse", "HEAD"], text=True).strip()
    assert version in {"2409c40b469e0e40bd5c55326df3d5ebb86ca81b", "60d7d76e4bb861ac9e0bbb043aa1a36946578397"}
    for name in list(os.environ):
        if name.startswith("WFC_"):
            del os.environ[name]
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    sys.dont_write_bytecode = True
    sys.path[:0] = [str(source / "tests"), str(source)]
    from test_router import Env

    env = Env()
    try:
        dispatch = env.dispatch()
        attempts = []
        report = env.report
        for n in [1, 2]:
            if n == 2:
                task = env.task.with_name("t2.md")
                task.write_text("round two")
                report = report.with_name("r2.md")
                env.json("continue", dispatch, "--task", str(task), "--report", str(report))
            row = env.json("status", "--dispatch", dispatch)["dispatches"][0]
            attempts.append(row["attempt"])
            report.write_text(f"completed round {n}")
            env.json("done", "--report", str(report), "--attempt", row["attempt"], dispatch=dispatch)
        source_db = env.state / "router.db"
        original_hash = digest(source_db)
        with closing(sqlite3.connect(source_db)) as db:
            receipts = db.execute("SELECT id,message_uid,body,created FROM message WHERE type='done' ORDER BY id").fetchall()
        assert len(receipts) == 2

        with tempfile.TemporaryDirectory() as tmp:
            for mode in ["complete", "recover", "conflict", "insufficient", "prior-migration", "in-flight"]:
                work = Path(tmp) / mode
                work.mkdir()
                db_path = work / "router.db"
                backup(source_db, db_path)
                with closing(sqlite3.connect(db_path)) as db:
                    if mode in ("prior-migration", "in-flight"):
                        db.execute("DELETE FROM dispatch_round")
                        db.execute("INSERT INTO dispatch_round(dispatch,round,attempt,report,state) VALUES(?,1,'a-wrong',?,'running')",
                                   (dispatch, str(report)))
                        db.execute("UPDATE dispatch SET current_round=1,current_attempt='a-wrong' WHERE id=?", (dispatch,))
                        if mode == "in-flight":
                            # The only evidence of the missing second round is a genuine continue.
                            db.execute("DELETE FROM message WHERE type!='continue'")
                            db.execute("DELETE FROM lifecycle_event")
                    elif mode != "complete":
                        db.executescript("""DROP TABLE dispatch_round;
                            CREATE TABLE old(id TEXT PRIMARY KEY, task TEXT,report TEXT,created REAL);
                            INSERT INTO old SELECT id,task,report,created FROM dispatch;
                            DROP TABLE dispatch; ALTER TABLE old RENAME TO dispatch;""")
                    if mode == "conflict":
                        db.execute("INSERT INTO lifecycle_event(dispatch,round,attempt,operation,result) VALUES(?,1,?,'create','accepted')",
                                   (dispatch, attempts[1]))
                    if mode == "insufficient":
                        # An observed next writer has no recorded snapshot/outcome.
                        db.execute("INSERT INTO lifecycle_event(dispatch,round,attempt,operation,result) VALUES(?,3,'a-unconfirmed','resume','pending')",
                                   (dispatch,))
                    db.commit()
                before = digest(db_path)
                prior_sql = dump(db_path)
                args = [binary, "migrate", "--offline-copy", work]
                settings = env.router_env(env={"WFC_STATE": str(work)})
                def migrate():
                    return subprocess.run(args, env=settings, capture_output=True, text=True, timeout=30)

                if mode in ("conflict", "insufficient", "prior-migration", "in-flight"):
                    for _ in range(2):
                        failed = migrate()
                        assert failed.returncode == 1 and dispatch in failed.stderr and not failed.stdout, failed
                        assert digest(db_path) == before and dump(db_path) == prior_sql
                    copies = list(work.glob("router.before-*.db"))
                    assert copies and all(dump(p) == prior_sql for p in copies)
                    print(mode, failed.stderr.strip(), "unchanged SHA", before, "backups", len(copies))
                    continue

                result = migrate()
                assert result.returncode == 0, result.stderr
                original = json.loads(result.stdout)["backup"]
                if mode == "recover":
                    assert original and dump(Path(original)) == prior_sql
                    assert Path(original).stat().st_mode & 0o777 == 0o600
                else:
                    assert original is None and before == digest(db_path)
                view = json.loads(subprocess.check_output([binary, "status", "--dispatch", dispatch], env=settings))
                d = view["dispatches"][0]
                assert (d["round"], d["attempt"], d["status"], d["report"]) == (2, attempts[1], "completed", str(report))
                assert d["scope"] == row["scope"] and d["role"] == row["role"]
                assert len(d["rounds"]) == 2
                for n, (snap, (num, uid, body, when)) in enumerate(zip(d["rounds"], receipts), start=1):
                    payload = json.loads(body)
                    msg = next(m for m in d["messages"] if m["id"] == num)
                    assert (snap["round"], snap["attempt"], snap["state"], snap["report"]) == (
                        n, attempts[n-1], "completed", payload["report"])
                    assert msg["messageId"] == uid and msg["body"] == body
                    if mode == "recover":
                        assert snap["result"] == body and snap["completed"] == when
                        assert snap["task"] == (str(task) if n == 2 else None)
                after = digest(db_path)
                assert json.loads(migrate().stdout)["backup"] is None and digest(db_path) == after
                print(mode, attempts, "before", before, "after", after, "backup", digest(Path(original)) if original else None)
        assert digest(source_db) == original_hash
        print(version[:8], "original SHA before and after", original_hash)
    finally:
        env.close()


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
