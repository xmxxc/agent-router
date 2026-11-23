"""Recover equivalent done results written by a real early writer and #39/#40."""
import hashlib
import json
import os
import runpy
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import closing
from pathlib import Path

from python_history import check
from python_recovery import backup, digest

EARLY = "71d2082c0fca0fb3c42da2360cf5ccb2a0f16c40"
MODERN = (
    "2409c40b469e0e40bd5c55326df3d5ebb86ca81b",
    "60d7d76e4bb861ac9e0bbb043aa1a36946578397",
)


def run(early, later, binary, work):
    e = runpy.run_path(str(early / "tests/test_router.py"))["Env"]()
    try:
        git = ["git", "-C", e.cwd, "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid"]
        subprocess.run([*git, "init", "--quiet"], check=True)
        subprocess.run([*git, "commit", "--allow-empty", "-qm", "fixture"], check=True)
        did = e.dispatch()
        report = str(e.report)
        e.report.write_text("private report")
        e.json("done", "--report", report, dispatch=did)
        original = e.state / "router.db"
        original_sha = digest(original)
        for version, checkout in zip(MODERN, later):
            private = work / version
            private.mkdir()
            source = private / "router.db"
            backup(original, source)
            env = {"HOME": str(work), "PATH": "/usr/bin:/bin", "WFC_STATE": str(private), "WFC_DISPATCH": did}
            done = [sys.executable, "-B", checkout / "router.py", "done", "--report", report]
            subprocess.run(done, env=env, check=True, capture_output=True, timeout=30)
            with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as db:
                rows = db.execute(
                    "SELECT id,message_uid,body,body_hash,created,status_version FROM message ORDER BY id"
                ).fetchall()
                result, completed = db.execute("SELECT result,completed FROM dispatch_round").fetchone()
            first, last = rows
            first_body, last_body = json.loads(first[2]), json.loads(last[2])
            assert first[2] != last[2] and first_body == last_body and list(first_body) != list(last_body)
            assert first[0] != last[0] and first[1] != last[1] and first[4] < last[4] <= completed
            assert all(hashlib.sha256(row[2].encode()).hexdigest() == row[3] for row in rows)
            assert json.loads(result) == {key: last_body[key] for key in ("report", "head", "dirty")}
            print("Python revisions", EARLY, version, "dispatch", did, "receipts", rows,
                  "snapshot", result, completed, flush=True)

            def probe(label, **kwargs):
                check(source, binary, did, label, current_report=report, **kwargs)

            missing_result = "UPDATE dispatch_round SET result=NULL;"
            restore = "UPDATE dispatch_round SET result=(SELECT body FROM message ORDER BY id DESC LIMIT 1);"
            probe("equivalent-intact")
            probe("equivalent-result-null", damage=missing_result, repair=restore)
            probe("equivalent-result-column", damage="ALTER TABLE dispatch_round DROP COLUMN result;",
                  repair="ALTER TABLE dispatch_round ADD COLUMN result TEXT;" + restore)
            probe("equivalent-dirty-missing",
                  damage="UPDATE dispatch_round SET result=json_remove(result,'$.dirty');",
                  repair="UPDATE dispatch_round SET result=json_insert(result,'$.dirty',json('false'));")
            probe("only-modern-receipt", damage=missing_result + "DELETE FROM message WHERE id=1;", repair=restore)

            # Older and newer serializers may also differ inside extension fields.
            # Keep these edits in private copies and hash each changed body independently.
            def edit_receipt(pos, expression):
                return (f"UPDATE message SET body={expression} WHERE id=(SELECT {pos}(id) FROM message);"
                        f"UPDATE message SET body_hash=body_sha(body) WHERE id=(SELECT {pos}(id) FROM message);")

            nested = edit_receipt("min", """json_set(body,'$.extra',json('{"a":[1,false],"b":{"n":null}}'))""")
            nested += edit_receipt("max", """json_set(body,'$.extra',json('{"b":{"n":null},"a":[1,false]}'))""")
            probe("nested-equivalent", damage=nested + missing_result, repair=restore)
            for label, expression in [
                ("nested-array-order", "json_set(body,'$.extra.a',json('[false,1]'))"),
                ("nested-field-missing", "json_remove(body,'$.extra.b.n')"),
                ("nested-field-different", "json_set(body,'$.extra.b.n',0)"),
            ]:
                probe(label, damage=nested + edit_receipt("max", expression) + missing_result,
                      refuse=True, diagnostic="conflicting done results")
            probe("one-attempt-two-rounds", damage=edit_receipt("max", "json_set(body,'$.round',2)") + missing_result,
                  refuse=True, diagnostic="one attempt belongs to conflicting rounds")
            competing = """INSERT INTO dispatch_round(dispatch,round,attempt,task,report,state)
                           SELECT dispatch,round,'a-other',task,report,'unknown' FROM dispatch_round;"""
            probe("ambiguous-attempt",
                  damage=competing + edit_receipt("max", "json_remove(body,'$.attempt')") + missing_result,
                  refuse=True, diagnostic="done has no unique attempt snapshot")
            # Neighboring Python integers outside u64 must not compare equal.
            wide = 18446744073709551616
            def wide_receipt(pos, value):
                return edit_receipt(pos, f"json_set(body,'$.extra',json('{json.dumps({'n': value})}'))")
            probe("nested-equivalent-wide-integers",
                  damage=wide_receipt("min", wide) + wide_receipt("max", wide) + missing_result, repair=restore)
            probe("nested-distinct-wide-integers",
                  damage=wide_receipt("min", wide) + wide_receipt("max", wide + 1) + missing_result,
                  refuse=True, diagnostic="conflicting done results")
            probe("nested-equivalent-decimals", damage=(
                edit_receipt("min", """json_set(body,'$.extra',json('{"n":1.0}'))""")
                + edit_receipt("max", """json_set(body,'$.extra',json('{"n":0.100e+1}'))""")
                + missing_result
            ), repair=restore)
            probe("nested-number-versus-object", damage=(
                edit_receipt("min", """json_set(body,'$.extra',json('{"$serde_json::private::Number":"42"}'))""")
                + edit_receipt("max", "json_set(body,'$.extra',42)")
                + missing_result
            ), refuse=True, diagnostic="conflicting done results")
            probe("nested-number-versus-string", damage=(
                edit_receipt("min", "json_set(body,'$.extra','n1e0')")
                + edit_receipt("max", "json_set(body,'$.extra',1)")
                + missing_result
            ), refuse=True, diagnostic="conflicting done results")

            changed = e.cwd / "dirty-fixture"
            changed.write_text("make the real Git result differ")
            try:
                subprocess.run(done, env=env, check=True, capture_output=True, timeout=30)
            finally:
                changed.unlink()
            with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as db:
                bodies = db.execute("SELECT body FROM message ORDER BY id").fetchall()
                assert [json.loads(body)["dirty"] for (body,) in bodies] == [False, False, True]
            probe("different-git-with-final-snapshot")
            probe("different-git-without-final-snapshot", damage=missing_result,
                  refuse=True, diagnostic="conflicting done results without a final snapshot result")
        assert digest(original) == original_sha
    finally:
        e.close()


if __name__ == "__main__":
    pr39, pr40, binary = (Path(arg).resolve() for arg in sys.argv[1:])
    for expected, checkout in zip(MODERN, (pr39, pr40)):
        version = subprocess.check_output(["git", "-C", checkout, "rev-parse", "HEAD"], text=True).strip()
        assert version == expected, version
    for key in list(os.environ):
        if key.startswith("WFC_"):
            del os.environ[key]
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        early = work / EARLY
        (early / "tests").mkdir(parents=True)
        for filename in ("router.py", "tests/test_router.py"):
            (early / filename).write_bytes(subprocess.check_output(
                ["git", "-C", pr40, "show", f"{EARLY}:{filename}"]
            ))
        run(early, (pr39, pr40), binary, work)
