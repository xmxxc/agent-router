"""Check repeated completions and failed-then-successful resumes from pinned Python CLIs."""
import hashlib
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
from contextlib import closing
from pathlib import Path

from python_recovery import backup, digest, dump


def check(source, binary, dispatch, label, *, damage="", repair="", refuse=False, diagnostic="", current_report=None):
    original_sha = digest(source)
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        work = root / "router.db"
        backup(source, work)
        with closing(sqlite3.connect(work)) as db:
            db.create_function("body_sha", 1, lambda body: hashlib.sha256(body.encode()).hexdigest())
            db.executescript(damage)
        before = digest(work)
        prior = dump(work)
        with closing(sqlite3.connect(":memory:")) as wanted:
            with closing(sqlite3.connect(work)) as src:
                src.backup(wanted)
            wanted.executescript(repair)
            expected = list(wanted.iterdump())
            if current_report is not None and not refuse:
                expected_rounds = dict(((n, a), (state, path, result, at))
                                       for n, a, state, path, result, at in wanted.execute(
                                           "SELECT round,attempt,state,report,result,completed FROM dispatch_round WHERE dispatch=?", (dispatch,)))

        after = before
        for attempt in range(2):
            result = subprocess.run([binary, "migrate", "--offline-copy", root],
                                    env={"HOME": tmp, "PATH": "/usr/bin:/bin", "WFC_STATE": tmp},
                                    capture_output=True, text=True, timeout=20)
            if refuse:
                assert result.returncode == 1 and dispatch in result.stderr and not result.stdout, result
                assert diagnostic in result.stderr, result.stderr
                assert digest(work) == before and dump(work) == prior
            else:
                assert result.returncode == 0, result.stderr
                saved = json.loads(result.stdout)["backup"]
                assert bool(saved) == bool(repair and attempt == 0), (label, saved)
                assert dump(work) == expected, label
                if current_report is not None:
                    view = subprocess.check_output([binary, "status", "--dispatch", dispatch],
                                                   env={"HOME": tmp, "WFC_STATE": tmp})
                    parent, = json.loads(view)["dispatches"]
                    current, = (r for r in parent["rounds"] if r["attempt"] == parent["attempt"] and r["round"] == parent["round"])
                    assert parent["report"] == current["report"] == current_report
                    assert {(r["round"], r["attempt"]): (r["state"], r["report"], r["result"], r["completed"])
                            for r in parent["rounds"]} == expected_rounds
                if attempt or not repair:
                    assert digest(work) == after
                after = digest(work)
        copies = list(root.glob("router.before-*.db"))
        assert len(copies) == (2 if refuse else int(bool(repair)))
        assert all(dump(p) == prior and p.stat().st_mode & 0o777 == 0o600 for p in copies)
        if refuse:
            restored = root / "restored.db"
            for copy in copies:
                backup(copy, restored)
                assert dump(restored) == prior
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
    clear_paths = "UPDATE dispatch SET report=NULL; UPDATE dispatch_round SET report=NULL;"
    restore_paths = """UPDATE dispatch SET report=(SELECT body->>'$.report' FROM message WHERE type='done');
        UPDATE dispatch_round SET report=(SELECT report FROM dispatch);"""
    for label, damage, repair in [
        ("both-paths", clear_paths, restore_paths),
        ("both-columns", "ALTER TABLE dispatch DROP COLUMN report; ALTER TABLE dispatch_round DROP COLUMN report;",
         "ALTER TABLE dispatch ADD COLUMN report TEXT; ALTER TABLE dispatch_round ADD COLUMN report TEXT;" + restore_paths),
        ("paths-from-receipt", clear_paths + "UPDATE dispatch_round SET result=NULL;",
         restore_paths + "UPDATE dispatch_round SET result=(SELECT body FROM message WHERE type='done');"),
        ("paths-from-result", clear_paths + "DELETE FROM message;",
         "UPDATE dispatch SET report=(SELECT result->>'$.report' FROM dispatch_round); UPDATE dispatch_round SET report=(SELECT report FROM dispatch);"),
    ]:
        check(source, binary, did, label, damage=damage, repair=repair, current_report=str(e.report))
    gaps = "UPDATE dispatch_round SET result=NULL, completed=NULL;"
    check(source, binary, did, "matching-body-hash", damage=gaps, repair="""
        UPDATE dispatch_round SET result=(SELECT body FROM message WHERE type='done'),
            completed=(SELECT created FROM message WHERE type='done');
    """)
    changed = """UPDATE message SET body=replace(body,'"dirty": false','"dirty": true') WHERE type='done';"""
    for label, damage in [("missing-final", gaps), ("complete", ""), ("missing-role", "UPDATE dispatch SET role=NULL;"),
                          ("old-schema", "ALTER TABLE dispatch_round DROP COLUMN callback_hash;")]:
        check(source, binary, did, "changed-body-hash-" + label, damage=changed + damage,
              refuse=True, diagnostic="body hash")
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


def reconciled(e, binary):
    # Dropping the endpoint's ping leaves create uncertain. Its disappearance
    # makes the subsequent resume inspection uncertain on this same attempt.
    endpoint = e.bin.parent / "herdr.sock"
    env = {"HOME": str(e.bin.parent)}
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(str(endpoint))
        endpoint.chmod(0o600)
        server.listen(1)
        server.settimeout(30)
        def drop_ping():
            with closing(server.accept()[0]) as conn:
                conn.settimeout(15)
                conn.recv(4096)
        thread = threading.Thread(target=drop_ping)
        thread.start()
        try:
            created = e.run("dispatch", "--host", "claude", "--backend", "herdr", "--herdr-endpoint", str(endpoint),
                            "--cwd", str(e.cwd), "--task", str(e.task), "--report", str(e.report), env=env, timeout=90)
            assert created.returncode == 1, created
        finally:
            thread.join()

    did = e.json("status")["dispatches"][0]["dispatch"]
    source = e.state / "router.db"
    def accept(operation):
        status = e.json("status", "--dispatch", did)["dispatches"][0]
        event = status["audit"][-1]
        assert (event["operation"], event["result"]) == (operation, "uncertain"), event
        e.json("reconcile", did, str(event["id"]), "--result", "accepted", "--actor", "test",
               "--evidence", "private fixture", env=env)

    accept("create")
    resume = e.run("resume", did, env=env, timeout=90)
    assert resume.returncode == 1 and "lifecycle_reconcile_required" in resume.stderr, resume
    check(source, binary, did, "uncertain-inspection")
    accept("resume")
    with closing(sqlite3.connect(source)) as db:
        row = db.execute("SELECT attempt,state,result,completed FROM dispatch_round").fetchone()
        assert row[1] == "running" and json.loads(row[2])["backend"] == "herdr" and row[3] is not None
        assert db.execute("SELECT count(*) FROM message WHERE type='done'").fetchone() == (0,)
        audit = db.execute("SELECT attempt,result,resolver_actor,resolved_at FROM lifecycle_event WHERE operation='resume'").fetchone()
        assert audit[:3] == (row[0], "accepted", "test") and audit[3] >= row[3]
        print("accepted backend observation", did, row, "audit", audit)
    check(source, binary, did, "accepted-inspection")
    check(source, binary, did, "accepted-inspection-without-create-result",
          damage="UPDATE dispatch_round SET result=NULL;")
    check(source, binary, did, "accepted-inspection-with-partial-done",
          damage="UPDATE dispatch_round SET result=json_set(result,'$.dirty',json('false'));", refuse=True)
    check(source, binary, did, "accepted-inspection-with-mixed-result",
          damage="""UPDATE dispatch_round SET result=json_set(result,'$.report',report,'$.head',NULL,'$.dirty',NULL);""",
          refuse=True, diagnostic="mixes task completion")
    check(source, binary, did, "inspection-without-resolution",
          damage="DELETE FROM lifecycle_event WHERE operation='resume';", refuse=True)
    e.report.write_text("completed after reconciliation")
    e.json("done", "--report", str(e.report), "--attempt", row[0], dispatch=did, env=env)
    check(source, binary, did, "completed-after-reconciliation")
    diagnostic_hex = row[2].encode().hex()
    check(source, binary, did, "done-with-stale-backend-result",
          damage=f"UPDATE dispatch_round SET state='running',result=CAST(X'{diagnostic_hex}' AS TEXT),completed={row[3]};",
          refuse=True, diagnostic="backend diagnostic")
    check(source, binary, did, "done-with-stale-observation-time",
          damage=f"UPDATE dispatch_round SET state='running',result=NULL,completed={row[3]};",
          refuse=True, diagnostic="time precedes")


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
    for probe in (repeated, resumed, reconciled):
        fixture = Env()
        try:
            probe(fixture, Path(sys.argv[2]).resolve())
        finally:
            fixture.close()
