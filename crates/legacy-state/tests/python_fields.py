"""Check path and result recovery from dispatch, ask, continue and done in both pinned CLIs."""
from contextlib import closing
from itertools import combinations
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

from python_history import check


def verify(e, binary):
    did = e.dispatch()
    source = e.state / "router.db"
    report = str(e.report)
    clear_parent = "UPDATE dispatch SET report=NULL;"
    clear_round = "UPDATE dispatch_round SET report=NULL WHERE round=1;"
    restore_parent = "UPDATE dispatch SET report=(SELECT report FROM dispatch_round WHERE round=1);"
    restore_round = "UPDATE dispatch_round SET report=(SELECT report FROM dispatch) WHERE round=1;"

    def probe(label, damage="", repair="", **kw):
        check(source, binary, did, label, damage=damage, repair=repair, current_report=report, **kw)

    probe("new-dispatch-no-path-evidence", clear_parent + clear_round, refuse=True)
    e.json("ask", "private fixture", dispatch=did)
    probe("running-intact")
    for table, restore in [("dispatch_round", restore_round), ("dispatch", restore_parent)]:
        for label, gap, schema in [
            ("null", f"UPDATE {table} SET report=NULL;", ""),
            ("empty", f"UPDATE {table} SET report='';", ""),
            ("column", f"ALTER TABLE {table} DROP COLUMN report;", f"ALTER TABLE {table} ADD COLUMN report TEXT;"),
        ]:
            probe(f"running-{table}-{label}", gap, schema + restore)
    probe("running-no-path-evidence", clear_parent + clear_round, refuse=True)
    probe("running-wrong-task", clear_round + "UPDATE dispatch SET task='/wrong';", refuse=True)
    probe("running-wrong-attempt", clear_round + "UPDATE dispatch SET current_attempt='other';", refuse=True)
    probe("running-disagreeing-paths", "UPDATE dispatch SET report='/wrong';", refuse=True)

    e.report.write_text("round one")
    e.json("done", "--report", report, dispatch=did)
    # This round has no repository: both present Git values are JSON null.
    probe("null-Git-pair", "UPDATE dispatch_round SET result=json_remove(result,'$.head','$.dirty');",
          "UPDATE dispatch_round SET result=json_insert(result,'$.head',NULL,'$.dirty',NULL);")
    git = ["git", "-C", str(e.cwd), "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid"]
    subprocess.run([*git, "init", "--quiet"], check=True)
    subprocess.run([*git, "commit", "--allow-empty", "-qm", "fixture"], check=True)
    task = e.task.with_name("next.md")
    task.write_text("next")
    second = e.report.with_name("next-report.md")
    report = str(second)
    e.json("continue", did, "--task", str(task), "--report", report)
    clear_both = clear_parent + "UPDATE dispatch_round SET report=NULL WHERE round=2;"
    fix_paths = """UPDATE dispatch SET report=(SELECT body->>'$.report' FROM message WHERE type='continue');
                   UPDATE dispatch_round SET report=(SELECT report FROM dispatch) WHERE round=2;"""
    probe("running-continue-proves-both-paths", clear_both, fix_paths)
    second.write_text("round two")
    e.json("done", "--report", report, dispatch=did)

    with closing(sqlite3.connect(source)) as db:
        receipt = db.execute("SELECT message_uid,body,body_hash,status_version FROM message WHERE type='done' ORDER BY id DESC LIMIT 1").fetchone()
        attempt, raw, at = db.execute("SELECT attempt,result,completed FROM dispatch_round WHERE round=2").fetchone()
        fields = json.loads(raw)
        assert fields["dirty"] is False and fields["report"] == report and fields["head"]
        print("modern completed", did, attempt, raw, at, "receipt", receipt)

    current = " WHERE round=2;"
    receipt_sql = "(SELECT body FROM message WHERE type='done' ORDER BY id DESC LIMIT 1)"
    for n in (1, 2, 3):
        for keys in combinations(("report", "head", "dirty"), n):
            paths = ",".join(f"'$.{k}'" for k in keys)
            inserts = ",".join(f"'$.{k}',{receipt_sql}->'$.{k}'" for k in keys)
            # The extra field must survive every repair.
            damage = f"UPDATE dispatch_round SET result=json_set(json_remove(result,{paths}),'$.extra','preserved')" + current
            repair = f"UPDATE dispatch_round SET result=json_insert(result,{inserts})" + current
            for label, gap, fix in [("intact-paths", "", ""), ("missing-paths", clear_both, fix_paths)]:
                probe("completed-" + "-".join(keys) + "-" + label, gap + damage, repair + fix)
    partial = "UPDATE dispatch_round SET result=json_remove(result,'$.dirty')" + current
    probe("explicit-null-conflicts", partial + "UPDATE dispatch_round SET result=json_set(result,'$.head',NULL)" + current, refuse=True)
    probe("different-round-done-only", partial + "DELETE FROM message WHERE type='done' AND body->>'$.round'=2;", refuse=True)
    probe("modern-result-without-git-or-done",
          "UPDATE dispatch_round SET result=json_remove(result,'$.head','$.dirty')" + current +
          "DELETE FROM message WHERE type='done' AND body->>'$.round'=2;", refuse=True)
    probe("continue-proves-modern-result",
          "UPDATE dispatch_round SET result=json_remove(result,'$.head','$.dirty'),callback_hash=NULL,callback_channel=NULL" + current +
          "DELETE FROM message WHERE type='done' AND body->>'$.round'=2;", refuse=True)
    probe("partial-result-and-wrong-report", partial + "UPDATE dispatch SET report='/wrong';", refuse=True)
    # Both sides can survive only in part. Matched round and attempt bind the evidence.
    remove_dirty = f"UPDATE dispatch_round SET result=json_remove(result,'$.dirty'){current}"
    remove_head = """UPDATE message SET body=json_remove(body,'$.head') WHERE type='done' AND body->>'$.round'=2;
                     UPDATE message SET body_hash=body_sha(body) WHERE type='done' AND body->>'$.round'=2;"""
    probe("complementary-result-and-receipt", remove_dirty + remove_head,
          "UPDATE dispatch_round SET result=json_insert(result,'$.dirty',json('false'))" + current)
    missing_in_both = remove_head + "UPDATE dispatch_round SET result=json_remove(result,'$.head')" + current
    probe("head-evidence-absent", missing_in_both, refuse=True)
    # The second snapshot must not supply Git values to the first snapshot.
    probe("historical-result", "UPDATE dispatch_round SET result=json_remove(result,'$.dirty') WHERE round=1;",
          "UPDATE dispatch_round SET result=json_insert(result,'$.dirty',NULL) WHERE round=1;")

    # Two different real commits, on the same attempt and report.
    subprocess.run([*git, "commit", "--allow-empty", "-qm", "second"], check=True)
    e.json("done", "--report", report, dispatch=did)
    missing_dirty = "UPDATE dispatch_round SET result=json_remove(result,'$.dirty')" + current
    fill_dirty = "UPDATE dispatch_round SET result=json_insert(result,'$.dirty',json('false'))" + current
    probe("last-head-selects-receipt-and-clock",
          missing_dirty + "UPDATE dispatch_round SET completed=NULL" + current,
          fill_dirty + "UPDATE dispatch_round SET completed=(SELECT created FROM message WHERE type='done' ORDER BY id DESC LIMIT 1)" + current)
    probe("last-dirty-cannot-select-head",
          "UPDATE dispatch_round SET result=json_remove(result,'$.head'),completed=NULL" + current, refuse=True)
    probe("last-receipt-and-snapshot-complement",
          missing_dirty + """UPDATE message SET body=json_remove(body,'$.head') WHERE id=(SELECT max(id) FROM message);
                             UPDATE message SET body_hash=body_sha(body) WHERE id=(SELECT max(id) FROM message);""", fill_dirty)


if __name__ == "__main__":
    src, binary = (Path(arg).resolve() for arg in sys.argv[1:])
    revision = subprocess.check_output(["git", "-C", src, "rev-parse", "HEAD"], text=True).strip()
    assert revision in {"2409c40b469e0e40bd5c55326df3d5ebb86ca81b", "60d7d76e4bb861ac9e0bbb043aa1a36946578397"}
    print("Python revision", revision)
    for key in list(os.environ):
        if key.startswith("WFC_"):
            del os.environ[key]
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    sys.path[:0] = [str(src / "tests"), str(src)]
    from test_router import Env
    fixture = Env()
    try:
        verify(fixture, binary)
    finally:
        fixture.close()
