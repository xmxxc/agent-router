"""Check repeated completions and failed-then-successful resumes from pinned Python CLIs."""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import closing
from pathlib import Path

from python_recovery import backup, digest, dump


def check(source, binary, dispatch, label, *, damage="", repair="", refuse=False):
    original_sha = digest(source)
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        work = root / "router.db"
        backup(source, work)
        with closing(sqlite3.connect(work)) as db:
            db.executescript(damage)
        before = digest(work)
        prior = dump(work)
        with closing(sqlite3.connect(":memory:")) as wanted:
            with closing(sqlite3.connect(work)) as src:
                src.backup(wanted)
            wanted.executescript(repair)
            expected = list(wanted.iterdump())

        after = before
        for attempt in range(2):
            result = subprocess.run([binary, "migrate", "--offline-copy", root],
                                    env={"HOME": tmp, "PATH": "/usr/bin:/bin", "WFC_STATE": tmp},
                                    capture_output=True, text=True, timeout=20)
            if refuse:
                assert result.returncode == 1 and dispatch in result.stderr and not result.stdout, result
                assert digest(work) == before and dump(work) == prior
            else:
                assert result.returncode == 0, result.stderr
                saved = json.loads(result.stdout)["backup"]
                assert bool(saved) == bool(repair and attempt == 0), (label, saved)
                assert dump(work) == expected, label
                if attempt or not repair:
                    assert digest(work) == after
                after = digest(work)
        copies = list(root.glob("router.before-*.db"))
        assert len(copies) == (2 if refuse else int(bool(repair)))
        assert all(dump(p) == prior and p.stat().st_mode & 0o777 == 0o600 for p in copies)
        assert digest(source) == original_sha
        print(label, "source", original_sha, "before", before, "after/retry", after,
              "backups", [digest(p) for p in copies], "diagnostic", result.stderr.strip())


def repeated(e, binary):
    for args in (["init", "--quiet"], ["commit", "--allow-empty", "-qm", "fixture"]):
        subprocess.run(["git", "-C", e.cwd, "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
                        *args], env={**os.environ, "HOME": str(e.bin.parent)}, check=True, capture_output=True)
    did = e.dispatch()
    e.report.write_text("completed")
    first = e.json("done", "--report", str(e.report), dispatch=did)
    source = e.state / "router.db"
    reset = "DELETE FROM message WHERE type='done'; UPDATE dispatch_round SET state='running';"
    check(source, binary, did, "running-result-without-receipt", damage=reset,
          repair="UPDATE dispatch_round SET state='completed';")
    check(source, binary, did, "incomplete-running-result", damage=reset+"UPDATE dispatch_round SET completed=NULL;", refuse=True)
    check(source, binary, did, "running-result-missing-report",
          damage=reset+"UPDATE dispatch_round SET result=json_remove(result,'$.report');", refuse=True)
    for missing_role in (False, True):
        damage = "UPDATE dispatch SET task='/other/task.md';"
        if missing_role:
            damage += "UPDATE dispatch SET role=NULL;"
        check(source, binary, did, f"conflicting-current-task missing-role={missing_role}", damage=damage, refuse=True)

    (e.cwd / "changed.txt").write_text("working tree changed")
    second = e.json("done", "--report", str(e.report), dispatch=did)
    assert first["messageId"] != second["messageId"] and not first["duplicate"] and not second["duplicate"]
    with closing(sqlite3.connect(source)) as db:
        receipts = db.execute("SELECT id,message_uid,body,status_version,created,request_key FROM message ORDER BY id").fetchall()
        assert [json.loads(r[2])["dirty"] for r in receipts] == [False, True]
        assert all(r[5] is None for r in receipts)
        result, when = db.execute("SELECT result,completed FROM dispatch_round").fetchone()
        assert json.loads(result)["dirty"] and when is not None
        print("two Python receipts", receipts, "final snapshot", result, when)
    check(source, binary, did, "repeat-clean-to-dirty")
    check(source, binary, did, "impossible-keyed-retry",
          damage="UPDATE message SET request_key='same-key' WHERE type='done';", refuse=True)
    restore_role = "UPDATE dispatch SET role=(SELECT body->>'$.role' FROM message WHERE type='done' ORDER BY id DESC LIMIT 1);"
    check(source, binary, did, "repeat-with-role-gap", damage="UPDATE dispatch SET role=NULL;", repair=restore_role)
    check(source, binary, did, "repeat-with-time-gap", damage="UPDATE dispatch_round SET completed=NULL;",
          repair="UPDATE dispatch_round SET completed=(SELECT created FROM message ORDER BY id DESC LIMIT 1);")
    check(source, binary, did, "repeat-without-final-result", damage="UPDATE dispatch_round SET result=NULL;", refuse=True)
    check(source, binary, did, "repeat-time-predates-final", damage="UPDATE dispatch_round SET completed=(SELECT min(created) FROM message);", refuse=True)
    check(source, binary, did, "repeat-final-result-disagrees",
          damage="UPDATE dispatch_round SET result=json_set(result,'$.dirty',json('false'));", refuse=True)
    # Another legal submission with exactly the same Git content has a new receipt.
    third = e.json("done", "--report", str(e.report), dispatch=did)
    assert third["messageId"] != second["messageId"]
    check(source, binary, did, "repeat-identical-final")
    check(source, binary, did, "repeat-identical-stale-time",
          damage="DELETE FROM message WHERE id=1; UPDATE dispatch_round SET completed=(SELECT min(created) FROM message);",
          refuse=True)


def resumed(e, binary):
    did = e.dispatch("claude")
    e.report.write_text("done one")
    e.json("done", "--report", str(e.report), dispatch=did)
    report = e.report.with_name("second.md")
    task = e.task.with_name("second.md")
    task.write_text("second task")
    e.json("continue", did, "--task", str(task), "--report", str(report))
    source = e.state / "router.db"
    check(source, binary, did, "ordinary-continue")
    e.end_session(did)
    failure = e.run("resume", did, timeout=90, env={"PATH": "/missing-fixture-tmux"})
    assert failure.returncode != 0
    check(source, binary, did, "failed-resume")
    check(source, binary, did, "unconfirmed-launch", damage="UPDATE lifecycle_event SET result='uncertain' WHERE operation='resume';")
    e.json("resume", did, timeout=90)
    current = e.json("status", "--dispatch", did)["dispatches"][0]["attempt"]
    report.write_text("done two")
    e.json("done", "--report", str(report), "--attempt", current, dispatch=did)
    with closing(sqlite3.connect(source)) as db:
        rows = db.execute("SELECT round,attempt,state FROM dispatch_round ORDER BY rowid").fetchall()
        events = db.execute("SELECT from_attempt,to_attempt,result FROM lifecycle_event WHERE operation='resume' ORDER BY id").fetchall()
        assert [r[2] for r in rows] == ["completed", "unknown", "unknown", "completed"]
        assert events == [(rows[1][1], rows[2][1], "definitely-not-sent"), (rows[2][1], current, "accepted")]
        assert rows[-1][1] == db.execute("SELECT body->>'$.attempt' FROM message WHERE type='done' ORDER BY id DESC LIMIT 1").fetchone()[0] == current
        print("Python committed attempts", rows, "launch outcomes", events, "current", current)
    check(source, binary, did, "successful-resume-done")
    # A partial audit still names its target. Both of its exact snapshot keys survive.
    check(source, binary, did, "partial-audit-with-known-target",
          damage="UPDATE lifecycle_event SET attempt=NULL,round=NULL WHERE operation='resume';")
    check(source, binary, did, "retry-chain-with-role-gap", damage="UPDATE dispatch SET role=NULL;",
          repair="UPDATE dispatch SET role=(SELECT body->>'$.role' FROM message WHERE type='done' LIMIT 1);")
    check(source, binary, did, "broken-chain", damage="DELETE FROM lifecycle_event WHERE result='definitely-not-sent';", refuse=True)
    check(source, binary, did, "forked-chain", damage="""UPDATE lifecycle_event SET from_attempt=
        (SELECT from_attempt FROM lifecycle_event WHERE result='definitely-not-sent') WHERE result='accepted';""", refuse=True)


if __name__ == "__main__":
    checkout = Path(sys.argv[1]).resolve()
    revision = subprocess.check_output(["git", "-C", checkout, "rev-parse", "HEAD"], text=True).strip()
    assert revision in {"2409c40b469e0e40bd5c55326df3d5ebb86ca81b", "60d7d76e4bb861ac9e0bbb043aa1a36946578397"}
    print("Python revision", revision)
    for key in list(os.environ):
        if key.startswith("WFC_"):
            del os.environ[key]
    sys.dont_write_bytecode = True
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    sys.path[:0] = [str(checkout / "tests"), str(checkout)]
    from test_router import Env
    for probe in (repeated, resumed):
        fixture = Env()
        try:
            probe(fixture, Path(sys.argv[2]).resolve())
        finally:
            fixture.close()
