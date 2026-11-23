"""Hosted acceptance of stopped #39/#40 worker, fault and delivery history."""
from contextlib import closing
import json
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile

from legacy_offline_import import (
    CURRENT, DISPATCH, FIRST, NEW, SCOPE, build_source, denied, digest, identifier,
    import_args, output, proof_for, run,
)
from legacy_public import snapshot

FAULTED = "d-00000032"
FAULT_ATTEMPT = "a-000000000032"
FINAL_HEAD = "b" * 40


def post(db, number, dispatch, recipient, kind, payload, *, state="unsent", version=1, created=14):
    raw = json.dumps(payload, ensure_ascii=False, sort_keys=True)
    db.execute("""INSERT INTO message (id,dispatch,recipient,type,body,created,delivered,message_uid,
               body_hash,state,status_version) VALUES (?,?,?,?,?,?,?,?,?,?,?)""",
               (number, dispatch, recipient, kind, raw, created, int(state in ("received", "processed")),
                identifier(number), digest(raw.encode()), state, version))
    return raw


def seed_full_history(source, work):
    database = source / "router.db"
    with closing(sqlite3.connect(database)) as db, db:
        question = {"question": "合成问题", "options": ["yes", "no"], "scope": SCOPE,
                    "role": "worker", "round": 1, "attempt": FIRST}
        archived = {
            40: post(db, 40, DISPATCH, "worker", "reply", {"to": 39, "answer": "yes"}),
            43: post(db, 43, DISPATCH, "worker", "reclaimed", {"tmux": f"wfc-{DISPATCH}"}),
            44: post(db, 44, DISPATCH, "worker", "resumed", {"session": "fixture-session",
                     "scope": SCOPE, "role": "worker", "round": 2, "attempt": CURRENT}),
            45: post(db, 45, DISPATCH, "worker", "continue", {"task": str(work / "task.md"),
                     "report": str(work / "last.md"), "scope": SCOPE, "role": "worker",
                     "round": 2, "attempt": CURRENT}),
            46: post(db, 46, DISPATCH, "worker", "nudge", {"text": "fixture note"}),
            65: post(db, 65, DISPATCH, "worker", "closed", {"round": 2, "attempt": CURRENT,
                     "result": "pending"}),
        }
        q_body = post(db, 39, DISPATCH, "controller", "question", question, state="processed", version=4)
        final_done = {"report": str(work / "last.md"), "head": FINAL_HEAD, "dirty": True}
        modern_done = {**final_done, "scope": SCOPE, "role": "worker", "round": 2, "attempt": CURRENT}
        post(db, 47, DISPATCH, "controller", "done", {**modern_done, "head": "a" * 40, "dirty": False},
             state="processed", version=4)
        raw = json.dumps(modern_done, ensure_ascii=False, sort_keys=True)
        db.execute("UPDATE message SET body=?, body_hash=?, created=15 WHERE id=49", (raw, digest(raw.encode())))
        db.execute("UPDATE dispatch_round SET result=?, completed=16 WHERE dispatch=? AND round=2",
                   (json.dumps(final_done), DISPATCH))
        # An unqualified receipt does not supersede a fully attributed completion.
        post(db, 51, DISPATCH, "controller", "done", {**final_done, "head": "c" * 40, "dirty": False},
             state="processed", version=4, created=17)
        db.execute("""UPDATE message SET resolver_actor='fixture',resolution_source='operator',
                   resolution_reason='received',resolution_evidence='proof',resolved_at=15 WHERE id=39""")
        db.execute("""INSERT INTO message_resolution (id,message,actor,source,reason,evidence,body_hash,
                   from_state,to_state,created) VALUES (8,39,'fixture','operator','received','proof',?,'unknown','received',15)""",
                   (digest(q_body.encode()),))
        db.execute("""INSERT INTO lifecycle_event (id,dispatch,operation,round,attempt,from_attempt,
                   result,owner_generation,resolver_actor,resolution_evidence,resolution_owner_generation,
                   resolved_at,created) VALUES (9,?,'close',2,?,?,'completed',7,'fixture','stopped',8,17,14)""",
                   (DISPATCH, CURRENT, CURRENT))
        db.execute("""INSERT INTO dispatch (id,host,backend,cwd,task,created,scope,role,current_round,current_attempt,report)
                   VALUES (?,'codex','tmux',?,?,10,?,'worker',1,?,?)""",
                   (FAULTED, str(work), str(work / "task.md"), SCOPE, FAULT_ATTEMPT, str(work / "faulted.md")))
        db.execute("""INSERT INTO dispatch_round (dispatch,round,attempt,task,report,state,result,created,completed)
                   VALUES (?,1,?,?,?,'failed',NULL,10,17)""",
                   (FAULTED, FAULT_ATTEMPT, str(work / "task.md"), str(work / "faulted.md")))
        archived[76] = post(db, 76, FAULTED, "controller", "unknown", {"kind": "turn_timeout", "message": "deadline",
                            "fault": {"category": "timeout", "requiresDecision": True}}, state="unknown", version=2)
        archived[82] = post(db, 82, FAULTED, "controller", "exited", {
                            "reason": "execution backend reports the worker ended without done", "kind": "process_exit",
                            "backendResult": "accepted", "fault": {"category": "process_exited", "requiresDecision": True}})
        db.execute("INSERT INTO fault_event VALUES (?,'launch','failed','process_exited','fixture exit',17)", (FAULTED,))
        db.execute("INSERT INTO host_error VALUES (?,'fixture host turn')", (FAULTED,))
        db.execute("INSERT INTO watchdog_fault VALUES (?,1,?,'timeout','deadline')", (FAULTED, FAULT_ATTEMPT))
        db.execute("INSERT INTO screen_watch VALUES (?,'unpublished screen body',16,NULL)", (FAULTED,))
        db.execute("UPDATE sqlite_sequence SET seq=125 WHERE name='message'")
    return archived


def rejects_unverified(binary, root, source):
    target = root / "reject-target"
    for index, (mutation, operands, diagnostic) in enumerate((
        ("UPDATE message SET dispatch=? WHERE id=40", (FAULTED,), "conflicts with its question assignment"),
        ("UPDATE message SET body=?, body_hash=? WHERE id=40",
         ('{"to": 60, "answer": "future"}', digest(b'{"to": 60, "answer": "future"}')),
         "no earlier controller question"),
        ("UPDATE message SET body='{}' WHERE id=82", (), "body hash mismatch"),
        ("UPDATE message SET recipient='worker' WHERE id=82", (), "unsupported legacy message type or recipient"),
        ("UPDATE watchdog_fault SET attempt='a-000000000099'", (), "unverified legacy watchdog fault attempt"),
        ("UPDATE message_resolution SET message=999", (), "legacy resolution has no referenced receipt"),
    )):
        rejected = root / f"negative-{index}"
        rejected.mkdir()
        db_file = Path(shutil.copyfile(source / "router.db", rejected / "router.db"))
        with closing(sqlite3.connect(db_file)) as db, db:
            db.execute(mutation, operands)
        before = snapshot(rejected)
        proof = proof_for(root / f"negative-{index}.json", db_file, target)
        denied(run(binary, root, target, *import_args(rejected, proof)), diagnostic)
        assert not target.exists() and snapshot(rejected) == before


def rejects_inconsistent_ledger(binary, root, source):
    for scenario in ("head", "dirty", "duplicate-numeric-id"):
        rejected = root / f"consistency-{scenario}"
        target = root / f"unpublished-{scenario}"
        rejected.mkdir()
        database = Path(shutil.copyfile(source / "router.db", rejected / "router.db"))
        with closing(sqlite3.connect(database)) as db, db:
            if scenario == "duplicate-numeric-id":
                # Keep the same columns and rows while removing the PK constraint
                # in this synthetic copy. The archived fault collides with done.
                db.executescript("""CREATE TABLE message_copy AS SELECT * FROM message;
                                   DROP TABLE message;
                                   ALTER TABLE message_copy RENAME TO message;
                                   INSERT INTO sqlite_sequence(name,seq) VALUES ('message',125);
                                   UPDATE message SET id=41 WHERE id=76;""")
                assert db.execute("SELECT dispatch, message_uid FROM message WHERE id=41 ORDER BY dispatch").fetchall() == [
                    (DISPATCH, identifier(41)), (FAULTED, identifier(76))]
                diagnostic = "duplicate legacy numeric receipt ID 41"
            else:
                result, = db.execute("SELECT result FROM dispatch_round WHERE dispatch=? AND round=2",
                                     (DISPATCH,)).fetchone()
                damaged = {**json.loads(result), scenario: {"head": "d" * 40, "dirty": False}[scenario]}
                db.execute("UPDATE dispatch_round SET result=? WHERE dispatch=? AND round=2",
                           (json.dumps(damaged), DISPATCH))
                diagnostic = f"legacy dispatch {DISPATCH} round 2 attempt {CURRENT} final done receipt 49"
        before = snapshot(rejected)
        proof = proof_for(root / f"consistency-{scenario}.json", database, target)
        result = run(binary, root, target, *import_args(rejected, proof))
        denied(result, diagnostic)
        if scenario != "duplicate-numeric-id":
            assert "conflicting stored result and final done receipt" in result.stderr, result
        assert not target.exists() and snapshot(rejected) == before
        print(json.dumps({"rejectedConsistencyCase": scenario, "sourceBeforeAfter": before["router.db"][1],
                          "publishedTarget": False}), flush=True)


def lookup(binary, root, target, dispatch):
    snapshot_now = snapshot(target)
    view = output(run(binary, root, target, "status", "--dispatch", dispatch))
    assert snapshot(target) == snapshot_now
    result, = view["dispatches"]
    return result, view["undelivered"]


def verify_history(binary, root, target, archived, bodies, work):
    done, _ = lookup(binary, root, target, DISPATCH)
    by_id = {row["id"]: row for row in done["messages"]}
    assert list(by_id) == [39, 40, 41, 43, 44, 45, 46, 47, 49, 51, 60, 61, 65]
    assert done["status"] == "completed" and done["backendAlive"] is False
    assert [(by_id[number]["head"], by_id[number]["dirty"]) for number in (47, 49, 51)] == [
        ("a" * 40, False), (FINAL_HEAD, True), ("c" * 40, False)]
    assert done["rounds"][1]["result"] == {"report": str(work / "last.md"), "head": FINAL_HEAD, "dirty": True}
    assert done["legacyAudit"][0]["ownerGeneration"] == 7
    assert done["legacyAudit"][0]["resolutionOwnerGeneration"] == 8
    assert done["legacyResolutions"][0]["messageId"] == identifier(39)
    assert by_id[39]["resolution"]["evidence"] == "proof"
    reply = by_id[40]
    assert (reply["type"], reply["recipient"], reply["to"], reply["questionMessageId"], reply["answer"]) == (
        "reply", "worker", 39, identifier(39), "yes")
    assert (reply["round"], reply["attempt"], reply["statusVersion"], reply["state"]) == (1, FIRST, 1, "unsent")
    assert by_id[43]["attempt"] is None and by_id[46]["attempt"] is None
    assert by_id[45]["report"] == str(work / "last.md") and by_id[45]["attempt"] == CURRENT
    assert by_id[65]["result"] == "pending" and done["legacyAudit"][0]["result"] == "completed"
    assert by_id[60]["state"] == "unknown"
    faulted, undelivered = lookup(binary, root, target, FAULTED)
    assert undelivered == [] and faulted["status"] == "failed" and faulted["backendAlive"] is False
    assert faulted["fault"]["category"] == "process_exited" and faulted["fault"]["created"] == 17
    assert faulted["hostError"] == "fixture host turn" and faulted["screenObservedSince"] == 16
    assert faulted["watchdogFaults"] == [{"round": 1, "attempt": FAULT_ATTEMPT, "kind": "timeout", "fingerprint": "deadline"}]
    assert all(item["attempt"] is None for item in faulted["messages"])
    by_id.update({item["id"]: item for item in faulted["messages"]})
    for number, raw in archived.items():
        seen = by_id[number]
        assert (seen["messageId"], seen["body"], seen["bodyHash"], seen["legacyArchiveOnly"]) == (
            identifier(number), raw, digest(raw.encode()), True)
    assert {row["id"] for row in json.loads((target / "messages.json").read_text())} == {39, 41, 47, 49, 51, 60, 61}
    for receipt in ("40", identifier(40), "76", identifier(82)):
        denied(run(binary, root, target, "ack", receipt, token=NEW), "no controller message")
    denied(run(binary, root, target, "resolve", "76", "--actor", "fixture", "--reason", "fixture",
               "--evidence", "fixture", "--body-hash", digest(archived[76].encode()), token=NEW), "no controller message")
    empty = output(run(binary, root, target, "wait", "--scope", SCOPE, "--timeout", "0", token=NEW), code=3)
    assert empty["messages"] == []
    one = output(run(binary, root, target, "ack", "41", "--body", bodies[41], token=NEW))
    two = output(run(binary, root, target, "ack", identifier(41), token=NEW))
    assert one["messageId"] == identifier(41) and one["statusVersion"] == 4 and not one["duplicate"]
    assert two == {**one, "duplicate": True}
    current, _ = lookup(binary, root, target, DISPATCH)
    assert next(item for item in current["messages"] if item["id"] == 60) == by_id[60]
    assert next(item for item in current["messages"] if item["id"] == 40) == reply
    task, report = work / "task.md", work / "new-report.md"
    report.write_text("fixture report")
    row = output(run(binary, root, target, "dispatch", "--scope", SCOPE, "--role", "worker", "--cwd", work,
                     "--task", task, "--report", report, "--", "/bin/true", token=NEW))
    try:
        native = output(run(binary, root, target, "done", "--report", report, "--attempt", row["attempt"], token=NEW,
                            extra={"WFC_DISPATCH": row["dispatch"], "WFC_ROUND": "1", "WFC_ATTEMPT": row["attempt"]}))
        assert native["message"] == 126
    finally:
        output(run(binary, root, target, "close", row["dispatch"], token=NEW))
    return native["message"]


def fixture(binary, schema, root, version, backend):
    root.mkdir()
    (root / "home").mkdir()
    source, work, bodies = build_source(root, schema, backend)
    archived = seed_full_history(source, work)
    original = snapshot(source)
    rejects_unverified(binary, root, source)
    rejects_inconsistent_ledger(binary, root, source)
    target = root / "native"
    proof = proof_for(root / "history-proof.json", source / "router.db", target)
    result = output(run(binary, root, target, *import_args(source, proof)))
    assert result["messages"] == 15 and result["messageHighWater"] == 125
    backup = Path(result["backup"])
    assert digest(backup.read_bytes()) == original["router.db"][1]
    new_id = verify_history(binary, root, target, archived, bodies, work)
    assert snapshot(source) == original

    # The newest reclaim still projects the resource state. Its receipt lacks
    # an execution identity and must not change the known completed round.
    reclaimed_source = root / "reclaimed-source"
    reclaimed_source.mkdir()
    database = Path(shutil.copyfile(source / "router.db", reclaimed_source / "router.db"))
    with closing(sqlite3.connect(database)) as db, db:
        post(db, 83, DISPATCH, "worker", "reclaimed", {"tmux": f"wfc-{DISPATCH}"})
    reclaimed_target = root / "reclaimed-native"
    proof = proof_for(root / "reclaimed-proof.json", database, reclaimed_target)
    output(run(binary, root, reclaimed_target, *import_args(reclaimed_source, proof)))
    reclaimed, _ = lookup(binary, root, reclaimed_target, DISPATCH)
    assert reclaimed["status"] == "reclaimed" and reclaimed["state"] == "done"
    assert reclaimed["last"]["id"] == 83 and reclaimed["last"]["attempt"] is None
    print(json.dumps({"syntheticPythonSchema": version, "sourceAndBackup": original["router.db"][1],
                      "archivedMessages": sorted(archived), "nextNativeId": new_id,
                      "preservedReplyQuestion": 39, "terminalFaultReadOnly": True}), flush=True)


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve(strict=True)
    schema = Path(__file__).resolve().parents[1] / "crates/legacy-state/src/schema.sql"
    with tempfile.TemporaryDirectory(prefix="wfc-legacy-history-") as directory:
        for version, backend in (("39", "tmux"), ("40", "herdr")):
            fixture(binary, schema, Path(directory) / version, version, backend)
    assert not Path(directory).exists()
