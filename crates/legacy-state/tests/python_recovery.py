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
            original_rounds = db.execute("SELECT task,report,result,completed FROM dispatch_round ORDER BY round").fetchall()
            messages = db.execute("SELECT * FROM message ORDER BY id").fetchall()
        assert len(receipts) == 2
        for snap, receipt in zip(original_rounds, receipts):
            assert snap[2] != receipt[2] and snap[3] != receipt[3]
        rejected = {"conflict", "insufficient", "prior-migration", "in-flight", "wrong-report", "wrong-result",
                    "missing-time", "missing-done", "wrong-continue", "partial-continue", "same-round-conflict", "cancelled", "cancelled-no-role"}
        missing_columns = {"missing-columns", "wrong-report", "missing-time", "missing-done"}
        for snap, receipt in zip(original_rounds, receipts):
            print("completion clocks", json.loads(receipt[2])["round"], "snapshot", snap[3], "receipt", receipt[3])

        with tempfile.TemporaryDirectory() as tmp:
            for mode in ["complete", "recover", "running", "running-no-role", "stale-current", "missing-time-only",
                         "report-from-result", *sorted(missing_columns | rejected)]:
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
                    elif mode in ("conflict", "insufficient", "recover"):
                        db.executescript("""DROP TABLE dispatch_round;
                            CREATE TABLE old(id TEXT PRIMARY KEY, task TEXT,report TEXT,created REAL);
                            INSERT INTO old SELECT id,task,report,created FROM dispatch;
                            DROP TABLE dispatch; ALTER TABLE old RENAME TO dispatch;""")
                    if mode in ("running", "running-no-role", "wrong-result"):
                        db.execute("UPDATE dispatch_round SET state='running' WHERE round=2")
                    if mode == "running-no-role":
                        db.execute("UPDATE dispatch SET role=NULL")
                    if mode == "wrong-result":
                        db.execute("UPDATE dispatch_round SET result=json_set(result,'$.report','/wrong') WHERE round=2")
                    if mode in missing_columns:
                        for field in ("report", "result", "completed"):
                            db.execute(f"ALTER TABLE dispatch_round DROP COLUMN {field}")
                    if mode == "wrong-report":
                        db.execute("UPDATE message SET body=json_set(body,'$.report','/wrong') WHERE type='done' AND id=3")
                    if mode == "missing-time":
                        db.execute("UPDATE message SET created=NULL WHERE type='done' AND id=3")
                    if mode == "missing-done":
                        db.execute("DELETE FROM message WHERE type='done' AND id=3")
                    if mode == "missing-time-only":
                        db.execute("UPDATE dispatch_round SET completed=NULL")
                    if mode == "report-from-result":
                        db.execute("UPDATE dispatch_round SET report=NULL WHERE round=1")
                        db.execute("DELETE FROM message WHERE type='done' AND id=1")
                    if mode == "stale-current":
                        db.execute("UPDATE dispatch SET current_round=1,current_attempt=?,task=?,report=?",
                                   (attempts[0], *original_rounds[0][:2]))
                    if mode == "wrong-continue":
                        db.execute("UPDATE message SET body=json_set(body,'$.task','/wrong') WHERE type='continue'")
                    if mode == "partial-continue":
                        db.execute("UPDATE message SET body=json_set(json_remove(body,'$.attempt'),'$.report','/wrong') WHERE type='continue'")
                    if mode in ("cancelled", "cancelled-no-role"):
                        db.execute("UPDATE dispatch_round SET state='cancelled' WHERE round=2")
                        if mode == "cancelled-no-role":
                            db.execute("UPDATE dispatch SET role=NULL")
                    if mode == "same-round-conflict":
                        db.execute("INSERT INTO dispatch_round(dispatch,round,attempt,task,report,state) VALUES(?,2,'other',?,?,'running')",
                                   (dispatch, *original_rounds[1][:2]))
                        db.execute("UPDATE dispatch SET current_attempt='other'")
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

                if mode in rejected:
                    for _ in range(2):
                        failed = migrate()
                        assert failed.returncode == 1 and dispatch in failed.stderr and not failed.stdout, failed
                        assert digest(db_path) == before and dump(db_path) == prior_sql
                    copies = list(work.glob("router.before-*.db"))
                    assert len(copies) == 2 and all(dump(p) == prior_sql and p.stat().st_mode & 0o777 == 0o600 for p in copies)
                    print(mode, failed.stderr.strip(), "unchanged SHA", before, "backups", len(copies))
                    continue

                result = migrate()
                assert result.returncode == 0, result.stderr
                original = json.loads(result.stdout)["backup"]
                if mode != "complete":
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
                    assert (snap["round"], snap["attempt"], snap["state"], snap["report"]) == (
                        n, attempts[n-1], "completed", payload["report"])
                    if mode == "recover" or mode == "missing-columns":
                        assert (snap["result"], snap["completed"]) == (body, when)
                    else:
                        original_time = when if mode == "missing-time-only" else original_rounds[n-1][3]
                        assert (snap["result"], snap["completed"]) == (original_rounds[n-1][2], original_time)
                    if mode == "recover":
                        assert snap["result"] == body and snap["completed"] == when
                        assert snap["task"] == (str(task) if n == 2 else None)
                expected_messages = [m for m in messages if mode != "report-from-result" or m[0] != 1]
                with closing(sqlite3.connect(db_path)) as checked:
                    assert checked.execute("SELECT * FROM message ORDER BY id").fetchall() == expected_messages
                after = digest(db_path)
                assert json.loads(migrate().stdout)["backup"] is None and digest(db_path) == after
                print(mode, attempts, "before", before, "after", after, "backup", digest(Path(original)) if original else None)
        assert digest(source_db) == original_hash
        print(version[:8], "original SHA before and after", original_hash)
    finally:
        env.close()


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
