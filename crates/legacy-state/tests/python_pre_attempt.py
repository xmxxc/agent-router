"""Compare a genuine pre-attempt two-report dispatch with the #39 and #40 upgrades."""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import closing
from pathlib import Path

from python_recovery import backup, digest, dump


def run(early, later, binary):
    for key in list(os.environ):
        if key.startswith("WFC_"):
            del os.environ[key]
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(early / "tests"))
    from test_router import Env
    e = Env()
    try:
        did = e.dispatch()
        report = e.report
        for n in (1, 2):
            if n == 2:
                report = e.report.with_name("second.md")
                task = e.task.with_name("next.md")
                task.write_text("next task")
                e.json("continue", did, "--task", str(task), "--report", str(report))
            report.write_text(f"done {n}")
            e.json("done", "--report", str(report), dispatch=did)
            original = e.state / "router.db"
            before = digest(original)
            with closing(sqlite3.connect(original)) as db:
                columns = ",".join(row[1] for row in db.execute("PRAGMA table_info(message)"))
                select_messages = f"SELECT {columns} FROM message ORDER BY id"
                messages = db.execute(select_messages).fetchall()
                mid, body, when = db.execute("SELECT id,body,created FROM message WHERE type='done' ORDER BY id DESC LIMIT 1").fetchone()
            assert mid == 2*n - 1 and "attempt" not in json.loads(body)
            with tempfile.TemporaryDirectory() as tmp:
                for name, source in [("39", later[0]), ("40", later[1]), ("rust", None)]:
                    work = Path(tmp) / name
                    work.mkdir()
                    db_path = work / "router.db"
                    backup(original, db_path)
                    prior = dump(db_path)
                    settings = {"PATH": "/usr/bin:/bin", "HOME": tmp, "WFC_STATE": str(work)}
                    if source:
                        subprocess.run([sys.executable, "-B", "-c", "import router; router.connect().close()"],
                                       cwd=source, env=settings, check=True)
                    else:
                        cmd = [binary, "migrate", "--offline-copy", work]
                        first = json.loads(subprocess.check_output(cmd, env=settings))
                        saved = Path(first["backup"])
                        assert saved.stat().st_mode & 0o777 == 0o600 and dump(saved) == prior
                        after = digest(db_path)
                        assert json.loads(subprocess.check_output(cmd, env=settings))["backup"] is None
                        assert digest(db_path) == after
                        print("rust backup", digest(saved), "repaired/retry", after)
                    with closing(sqlite3.connect(db_path)) as db:
                        assert db.execute(select_messages).fetchall() == messages
                        snap, = db.execute("SELECT round,attempt,report,state,result,completed FROM dispatch_round")
                        assert snap[2:] == (str(report), "completed", body, when)
                        assert db.execute("SELECT current_round,current_attempt FROM dispatch").fetchone() == snap[:2]
                    print(name, "pre-attempt reports", n, "selected done", mid, "clock", when, "messages", len(messages))
                    if source and n == 1:
                        # The same original report can be completed again after Python upgrades the database.
                        subprocess.run([sys.executable, "-B", source / "router.py", "done", "--report", str(report)],
                                       env={**settings, "WFC_DISPATCH": did}, check=True, capture_output=True)
                        mixed, mixed_sha = dump(db_path), digest(db_path)
                        with closing(sqlite3.connect(db_path)) as db:
                            bodies = [json.loads(row[0]) for row in db.execute("SELECT body FROM message ORDER BY id")]
                            assert len(bodies) == 2 and "attempt" not in bodies[0] and bodies[1]["attempt"] == snap[1]
                        for _ in range(2):
                            opened = json.loads(subprocess.check_output([binary, "migrate", "--offline-copy", work], env=settings))
                            assert opened["backup"] is None and dump(db_path) == mixed and digest(db_path) == mixed_sha
                        print(name, "pre-attempt -> modern repeated done", mid, "all rows and bytes unchanged", mixed_sha)
                if n == 2:
                    conflict = Path(tmp) / "conflict"
                    conflict.mkdir()
                    path = conflict / "router.db"
                    backup(original, path)
                    with closing(sqlite3.connect(path)) as db:
                        db.execute("UPDATE message SET body=json_set(body,'$.report',?,'$.head','different') WHERE id=1",
                                   (str(report),))
                        db.commit()
                    prior, unchanged = dump(path), digest(path)
                    for _ in range(2):
                        failed = subprocess.run([binary, "migrate", "--offline-copy", conflict],
                                                env=settings, capture_output=True, text=True)
                        assert failed.returncode == 1 and did in failed.stderr and not failed.stdout
                        assert digest(path) == unchanged and dump(path) == prior
                    copies = list(conflict.glob("router.before-*.db"))
                    assert len(copies) == 2 and all(p.stat().st_mode & 0o777 == 0o600 and dump(p) == prior for p in copies)
                    print("same report conflicting done", failed.stderr.strip(), "unchanged", unchanged, "backups", len(copies))
            assert digest(original) == before
            print("source before/after", before)
    finally:
        e.close()


if __name__ == "__main__":
    # Export precisely the old CLI and its own isolated host fixture without checking out a branch.
    later = [Path(arg).resolve() for arg in sys.argv[1:3]]
    revisions = ["2409c40b469e0e40bd5c55326df3d5ebb86ca81b", "60d7d76e4bb861ac9e0bbb043aa1a36946578397"]
    for source, rev in zip(later, revisions):
        assert subprocess.check_output(["git", "-C", source, "rev-parse", "HEAD"], text=True).strip() == rev
    with tempfile.TemporaryDirectory() as directory:
        early = Path(directory)
        (early / "tests").mkdir()
        for path in ("router.py", "tests/test_router.py"):
            (early / path).write_bytes(subprocess.check_output(
                ["git", "-C", later[1], "show", "b64b0e0e861d17cf2ae5776baf304cb429c3762b:" + path]))
        run(early, later, Path(sys.argv[3]).resolve())
