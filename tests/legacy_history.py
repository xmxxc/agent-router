"""Hosted acceptance of stopped #39/#40 worker, fault and delivery history."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import traceback

from legacy_offline_import import (
    CURRENT, DISPATCH, FIRST, NEW, OLD, SCOPE, build_source, denied, digest, identifier,
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
                   resolution_reason='received',resolution_evidence='proof',resolved_at=15.125 WHERE id=39""")
        db.execute("""INSERT INTO message_resolution (id,message,actor,source,reason,evidence,body_hash,
                   from_state,to_state,created) VALUES (8,39,'fixture','operator','received','proof',?,'unknown','received',15.125)""",
                   (digest(q_body.encode()),))
        db.execute("""INSERT INTO lifecycle_event (id,dispatch,operation,round,attempt,from_attempt,
                   result,owner_generation,owner_token,resolver_actor,resolution_evidence,resolution_owner_generation,
                   resolved_at,created) VALUES
                   (9,?,'close',2,?,?,'completed',7,'fixture-operation-owner','fixture','stopped',8,17,14)""",
                   (DISPATCH, CURRENT, CURRENT))
        db.execute("""INSERT INTO lifecycle_event (id,dispatch,operation,round,attempt,result,owner_generation,created)
                   VALUES (3,?,'resume',1,?,'completed',6,11)""", (DISPATCH, FIRST))
        db.execute("UPDATE controller_owner SET generation=8 WHERE scope=?", (SCOPE,))
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


def rejects_inconsistent_ledger(binary, root, source, scenario):
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


def rejects_incomplete_confirmation(binary, root, source, case, mutation, operands, diagnostic):
    rejected = root / case
    target = root / f"unpublished-{case}"
    rejected.mkdir()
    database = Path(shutil.copyfile(source / "router.db", rejected / "router.db"))
    with closing(sqlite3.connect(database)) as db, db:
        db.execute(mutation, operands)
    before = snapshot(rejected)
    scratch = set(root.glob(".wfc-legacy-import-*"))
    proof = proof_for(root / f"{case}.json", database, target)
    denied(run(binary, root, target, *import_args(rejected, proof)), diagnostic)
    assert not target.exists() and snapshot(rejected) == before
    assert set(root.glob(".wfc-legacy-import-*")) == scratch
    print(json.dumps({"rejectedConfirmationCase": case, "sourceBeforeAfter": before["router.db"][1],
                      "publishedTarget": False}), flush=True)


def rejects_owner_history(binary, root, source):
    for case, mutation, operands, diagnostic in (
        ("owner-missing", "DELETE FROM controller_owner WHERE scope=?", (SCOPE,),
         "managed legacy lifecycle history has no current owner generation"),
        ("resolution-ahead", "UPDATE controller_owner SET generation=7 WHERE scope=?", (SCOPE,),
         "current legacy owner generation 7 precedes lifecycle generation 8"),
        ("operation-ahead", "UPDATE lifecycle_event SET owner_generation=9 WHERE id=9", (),
         "current legacy owner generation 8 precedes lifecycle generation 9"),
        ("resolution-before-operation", "UPDATE lifecycle_event SET resolution_owner_generation=6 WHERE id=9", (),
         "legacy resolution owner generation precedes the operation"),
        ("token-without-generation", """UPDATE lifecycle_event SET owner_generation=NULL,
                                     resolution_owner_generation=NULL WHERE id=9""", (),
         "managed legacy lifecycle operation lacks owner generation"),
    ):
        rejects_incomplete_confirmation(binary, root, source, case, mutation, operands, diagnostic)


def rejects_receipt_history(binary, root, source):
    conflict = "legacy resolution 8 conflicts with primary receipt 39"
    for case, mutation, diagnostic in (
        ("primary-only", "DELETE FROM message_resolution WHERE id=8",
         "legacy primary receipt 39 is missing its resolution history"),
        ("history-only", """UPDATE message SET resolver_actor=NULL,resolution_source=NULL,resolution_reason=NULL,
                            resolution_evidence=NULL,resolved_at=NULL WHERE id=39""",
         "legacy resolution for receipt 39 lacks unique primary evidence"),
        ("partial-primary", "UPDATE message SET resolver_actor=NULL WHERE id=39",
         "incomplete or invalid primary legacy receipt resolution"),
        ("repeated-history", """INSERT INTO message_resolution SELECT 9,message,actor,source,reason,evidence,
                                body_hash,from_state,to_state,created FROM message_resolution WHERE id=8""",
         "legacy resolution for receipt 39 lacks unique primary evidence"),
        ("raw-time", "UPDATE message_resolution SET created=15.875 WHERE id=8", conflict),
        ("wrong-source", "UPDATE message_resolution SET source='other' WHERE id=8", conflict),
        ("wrong-transition", "UPDATE message_resolution SET to_state='processed' WHERE id=8", conflict),
        ("wrong-digest", "UPDATE message_resolution SET body_hash='wrong' WHERE id=8", conflict),
    ):
        rejects_incomplete_confirmation(binary, root, source, case, mutation, (), diagnostic)


def lookup(binary, root, target, dispatch):
    snapshot_now = snapshot(target)
    view = output(run(binary, root, target, "status", "--dispatch", dispatch))
    assert snapshot(target) == snapshot_now
    result, = view["dispatches"]
    return result, view["undelivered"]


def verify_resolution_history(binary, root, target, work):
    done, _ = lookup(binary, root, target, DISPATCH)
    by_id = {row["id"]: row for row in done["messages"]}
    assert done["status"] == "completed" and done["backendAlive"] is False
    assert [(by_id[number]["head"], by_id[number]["dirty"]) for number in (47, 49, 51)] == [
        ("a" * 40, False), (FINAL_HEAD, True), ("c" * 40, False)]
    assert done["rounds"][1]["result"] == {"report": str(work / "last.md"), "head": FINAL_HEAD, "dirty": True}
    audit = {event["id"]: event for event in done["legacyAudit"]}
    assert list(audit) == [3, 9]
    assert audit[3]["resolutionOwnerGeneration"] is None
    close = audit[9]
    assert (close["operation"], close["round"], close["attempt"], close["result"]) == (
        "close", 2, CURRENT, "completed")
    assert (close["ownerGeneration"], close["resolutionOwnerGeneration"], close["resolution_owner_generation"]) == (
        7, 8, 8)
    assert done["controllerOwner"]["generation"] == 9
    assert (close["resolver_actor"], close["resolution_evidence"], close["resolved_at"]) == ("fixture", "stopped", 17)
    assert {"owner_token", "ownerTokenRecorded"}.isdisjoint(close)
    assert by_id[65]["result"] == "pending"
    question = by_id[39]
    resolution, = done["legacyResolutions"]
    assert (question["id"], question["messageId"], question["state"], question["statusVersion"]) == (
        39, identifier(39), "processed", 4)
    assert resolution == {"id": 8, "message": 39, "messageId": identifier(39), "bodyHash": question["bodyHash"],
                          "actor": "fixture", "source": "operator", "reason": "received", "evidence": "proof",
                          "fromState": "unknown", "toState": "received", "created": 15.125}
    for field in ("actor", "source", "reason", "evidence"):
        assert question["resolution"][field] == resolution[field]
    proof = ("--actor", "fixture", "--source", "operator", "--reason", "received",
             "--evidence", "proof", "--body-hash", question["bodyHash"])
    denied(run(binary, root, target, "resolve", "39", *proof, token=OLD), "stale_controller_owner")
    for receipt in ("39", identifier(39)):
        replay = output(run(binary, root, target, "resolve", receipt, *proof, token=NEW))
        assert (replay["message"], replay["messageId"], replay["state"], replay["statusVersion"],
                replay["duplicate"], replay["resolution"]) == (39, identifier(39), "processed", 4, True,
                                                                question["resolution"])
    print(json.dumps({"lifecycleAuditId": close["id"], "ownerGeneration": close["ownerGeneration"],
                      "resolutionOwnerGeneration": close["resolutionOwnerGeneration"],
                      "newOwnerGeneration": done["controllerOwner"]["generation"],
                      "messageResolutionId": resolution["id"], "resolvedMessage": resolution["message"]}), flush=True)


def verify_archive_direction(binary, root, target, archived, work):
    done, _ = lookup(binary, root, target, DISPATCH)
    by_id = {row["id"]: row for row in done["messages"]}
    assert list(by_id) == [39, 40, 41, 43, 44, 45, 46, 47, 49, 51, 60, 61, 65]
    reply = by_id[40]
    assert (reply["type"], reply["recipient"], reply["to"], reply["questionMessageId"], reply["answer"]) == (
        "reply", "worker", 39, identifier(39), "yes")
    assert (reply["round"], reply["attempt"], reply["statusVersion"], reply["state"]) == (1, FIRST, 1, "unsent")
    assert by_id[43]["attempt"] is None and by_id[46]["attempt"] is None
    assert by_id[45]["report"] == str(work / "last.md") and by_id[45]["attempt"] == CURRENT
    faulted, undelivered = lookup(binary, root, target, FAULTED)
    assert undelivered == [] and faulted["status"] == "failed" and faulted["backendAlive"] is False
    assert faulted["fault"]["category"] == "process_exited" and faulted["fault"]["created"] == 17
    assert faulted["hostError"] == "fixture host turn" and faulted["screenObservedSince"] == 16
    assert faulted["watchdogFaults"] == [{"round": 1, "attempt": FAULT_ATTEMPT, "kind": "timeout", "fingerprint": "deadline"}]
    assert all(item["attempt"] is None for item in faulted["messages"])
    assert [(item["id"], item["recipient"], item["type"], item["state"], item["statusVersion"])
            for item in faulted["messages"]] == [
        (76, "controller", "unknown", "unknown", 2), (82, "controller", "exited", "unsent", 1)]
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
    print(json.dumps({"archivedMessages": sorted(archived), "preservedReplyQuestion": 39,
                      "terminalFaultReadOnly": True}), flush=True)


def verify_controller_ledger(binary, root, target, bodies, work):
    done, _ = lookup(binary, root, target, DISPATCH)
    by_id = {row["id"]: row for row in done["messages"]}
    assert by_id[60]["state"] == "unknown" and by_id[61]["legacyDeliveryHeld"] is True
    empty = output(run(binary, root, target, "wait", "--scope", SCOPE, "--timeout", "0", token=NEW), code=3)
    assert empty["messages"] == []
    one = output(run(binary, root, target, "ack", "41", "--body", bodies[41], token=NEW))
    two = output(run(binary, root, target, "ack", identifier(41), token=NEW))
    assert one["messageId"] == identifier(41) and one["statusVersion"] == 4 and not one["duplicate"]
    assert two == {**one, "duplicate": True}
    current, _ = lookup(binary, root, target, DISPATCH)
    assert [item for item in current["messages"] if item["id"] in (40, 60, 61)] == [
        by_id[number] for number in (40, 60, 61)]
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
    print(json.dumps({"ackedOriginal": one["messageId"], "statusVersion": one["statusVersion"],
                      "nextNativeId": native["message"]}), flush=True)


def verify_combination(binary, root, target, source, work):
    assert (source / "router.db").is_file() and not (target / "router.db").exists()
    assert (target / "legacy-import.json").is_file() and (target / "messages.json").is_file()
    archive, _ = lookup(binary, root, target, DISPATCH)
    assert archive["legacyArchived"] is True and archive["backendAlive"] is False
    archived = {item["id"]: item for item in archive["messages"]}
    question = archived[39]
    assert (question["type"], question["messageId"], question["bodyHash"]) == (
        "question", identifier(39), digest(question["body"].encode()))
    assert (archived[40]["recipient"], archived[40]["to"], archived[40]["questionMessageId"]) == (
        "worker", 39, identifier(39))

    # The archive must be rejected before the transport is even selected.
    trap_bin = root / "old-host-transport"
    trap_bin.mkdir()
    trap = trap_bin / "tmux"
    trap.write_text('#!/bin/sh\n: > "$WFC_TEST_OLD_HOST_CALL"\nexit 1\n')
    trap.chmod(0o700)
    old_call = root / "old-host-called"
    old_env = {"PATH": str(trap_bin), "WFC_TEST_OLD_HOST_CALL": str(old_call)}
    before = snapshot(target)
    refusal = run(binary, root, target, "reply", "39", "archive must stay stopped", token=NEW, extra=old_env)
    assert (refusal.returncode, refusal.stdout, refusal.stderr) == (
        1, "", "Error: offline legacy assignment has no writable worker\n"), refusal
    assert not old_call.exists() and snapshot(target) == before
    print(json.dumps({"archivedQuestion": question["messageId"], "archivedReplySent": False,
                      "archiveRefusal": refusal.stderr.strip()}), flush=True)

    # The same spelling selects the literal native directory and the expanded
    # Python directory. A synthetic DB at the latter still fences a reply.
    literal = root / "~"
    literal.mkdir()
    (literal / "mixed").symlink_to(target, target_is_directory=True)
    legacy = root / "home" / "mixed"
    legacy.mkdir()
    shutil.copyfile(source / "router.db", legacy / "router.db")
    mixed_before = snapshot(legacy)
    denied(run(binary, root, target, "reply", "39", "mixed legacy state", token=NEW,
               extra={**old_env, "WFC_STATE": "~/mixed"}), f"legacy state write blocked at {legacy / 'router.db'}.")
    assert not old_call.exists() and snapshot(target) == before and snapshot(legacy) == mixed_before
    print(json.dumps({"mixedWriteBlocked": True, "mixedSourceBeforeAfter": mixed_before["router.db"][1]}), flush=True)

    fixture = work / "live"
    fixture.mkdir()
    socket = root / "reply.sock"
    tmux_env = {"TERM": "xterm", "WFC_TMUX_SOCKET": str(socket)}
    tmux_path = shutil.which("tmux", path=os.defpath)
    assert tmux_path is not None
    tmux = [tmux_path, "-S", str(socket), "-f", "/dev/null"]
    composer = Path(__file__).resolve().parents[1] / "tools" / "check_tmux.py"
    log = fixture / "input.log"
    config = fixture / "launch.json"
    config.write_text(json.dumps({"hosts": {"codex": {"command": [
        sys.executable, str(composer), "--reply-worker", "empty", str(log)]}}}))
    task = work / "task.md"
    row = None
    try:
        row = output(run(binary, root, target, "dispatch", "--scope", SCOPE, "--role", "worker",
                         "--host", "codex", "--backend", "tmux", "--cwd", fixture, "--task", task,
                         "--report", fixture / "report", "--launch-config", config, token=NEW, extra=tmux_env))
        question_file = fixture / "question.json"
        deadline = time.monotonic() + 10
        while True:
            screen = subprocess.run(tmux + ["capture-pane", "-p", "-t", f"=wfc-{row['dispatch']}:"],
                                    capture_output=True, text=True, timeout=3)
            if question_file.exists() and "Ask Codex to do anything" in screen.stdout:
                break
            assert time.monotonic() < deadline, (screen.stdout, screen.stderr)
            time.sleep(.025)
        question = json.loads(question_file.read_bytes())
        assert question["message"] == 127
        answer = output(run(binary, root, target, "reply", "127", "one native answer", token=NEW, extra=tmux_env))
        assert answer == {"replied": 127, "dispatch": row["dispatch"]}
        detail, _ = lookup(binary, root, target, row["dispatch"])
        reply, = (item for item in detail["messages"] if item["type"] == "reply")
        assert (reply["id"], reply["to"], reply["questionMessageId"], reply["inputResult"]) == (
            128, 127, question["messageId"], "accepted")
        expected = b"[router] Answer to question 127: one native answer"
        assert (fixture / "accepted").read_bytes() == expected + b"\n"
        events = [json.loads(line) for line in log.read_text().splitlines()]
        assert b"".join(bytes.fromhex(event[0]) for event in events[1:]) == expected + b"\r"
        (fixture / "finish").touch()
        received = output(run(binary, root, target, "wait", "--scope", SCOPE, "--dispatch", row["dispatch"], "--timeout", "8",
                              token=NEW, extra=tmux_env))
        completed, = (item for item in received["messages"] if item["type"] == "done")
        assert completed["id"] == 129 and completed["attempt"] == row["attempt"]
    finally:
        try:
            if row is not None:
                output(run(binary, root, target, "close", row["dispatch"], token=NEW, extra=tmux_env))
        finally:
            subprocess.run(tmux + ["kill-server"], capture_output=True, timeout=5)

    failed = run(binary, root, target, "dispatch", "--scope", SCOPE, "--role", "worker", "--cwd", work,
                 "--task", task, "--report", work / "failed-start", "--", work / "absent-executable", token=NEW)
    assert failed.returncode == 1, failed
    ledger = json.loads((target / "messages.json").read_text())
    startup, = (item for item in ledger if item.get("kind") == "startup_failure")
    received = output(run(binary, root, target, "wait", "--scope", SCOPE, "--dispatch", startup["dispatch"],
                          "--timeout", "4", token=NEW))
    notice, = received["messages"]
    assert (notice["id"], notice["messageId"], notice["kind"], notice["fault"]["category"]) == (
        130, startup["messageId"], "startup_failure", "executable_missing")
    assert [item["id"] for item in ledger if item["id"] > 125] == [126, 127, 128, 129, 130]
    assert len({item["messageId"] for item in ledger}) == len(ledger)
    print(json.dumps({"nativeIds": [item["id"] for item in ledger if item["id"] > 125],
                      "nativeQuestion": 127, "nativeReply": reply["id"], "nativeDone": completed["id"],
                      "startupDiagnostic": notice["id"], "confirmedInputCount": 1}),
          flush=True)


def verify_publication(binary, root, source, target, original):
    proof = proof_for(root / "history-proof.json", source / "router.db", target)
    result = output(run(binary, root, target, *import_args(source, proof)))
    assert result["messages"] == 15 and result["messageHighWater"] == 125
    backup = Path(result["backup"])
    assert digest(backup.read_bytes()) == original["router.db"][1]
    assert snapshot(source) == original
    print(json.dumps({"sourceAndBackup": original["router.db"][1], "messageHighWater": 125}), flush=True)


def verify_reclaimed(binary, root, source):
    # The newest reclaim still projects the resource state. Its receipt lacks
    # an execution identity and must not change the known completed round.
    reclaimed_source = root / "reclaimed-source"
    reclaimed_source.mkdir()
    database = Path(shutil.copyfile(source / "router.db", reclaimed_source / "router.db"))
    with closing(sqlite3.connect(database)) as db, db:
        post(db, 83, DISPATCH, "worker", "reclaimed", {"tmux": f"wfc-{DISPATCH}"})
    before = snapshot(reclaimed_source)
    reclaimed_target = root / "reclaimed-native"
    proof = proof_for(root / "reclaimed-proof.json", database, reclaimed_target)
    output(run(binary, root, reclaimed_target, *import_args(reclaimed_source, proof)))
    reclaimed, _ = lookup(binary, root, reclaimed_target, DISPATCH)
    assert reclaimed["status"] == "reclaimed" and reclaimed["state"] == "done"
    assert reclaimed["last"]["id"] == 83 and reclaimed["last"]["attempt"] is None
    assert snapshot(reclaimed_source) == before


def verify_source_integrity(source, original):
    assert snapshot(source) == original


def run_group(version, group, check, *args):
    marker = f"history schema={version} group={group}"
    print(f"{marker} outcome=started", flush=True)
    try:
        check(*args)
    except Exception:
        traceback.print_exc()
        print(f"{marker} outcome=failed", flush=True)
        return False
    print(f"{marker} outcome=passed", flush=True)
    return True


def fixture(binary, schema, root, version, backend, combined_only):
    root.mkdir()
    (root / "home").mkdir()
    source, work, bodies = build_source(root, schema, backend)
    archived = seed_full_history(source, work)
    original = snapshot(source)
    target = root / "native"
    checks = []
    if not combined_only:
        checks.append(run_group(version, "identity-rejections", rejects_unverified, binary, root, source))
        for scenario in ("head", "dirty", "duplicate-numeric-id"):
            checks.append(run_group(version, f"consistency-{scenario}",
                                    rejects_inconsistent_ledger, binary, root, source, scenario))
        checks.append(run_group(version, "owner-confirmations", rejects_owner_history, binary, root, source))
        checks.append(run_group(version, "receipt-confirmations", rejects_receipt_history, binary, root, source))
        checks.append(run_group(version, "reclaim-history", verify_reclaimed, binary, root, source))
    published = run_group(version, "publish-and-backup", verify_publication, binary, root, source, target, original)
    history_checks = (
        ("controller-ack-and-high-water", verify_controller_ledger, (bodies, work)),
        ("combined-numbering", verify_combination, (source, work)),
    )
    if not combined_only:
        history_checks = (
            ("resolution-history", verify_resolution_history, (work,)),
            ("archive-direction", verify_archive_direction, (archived, work)),
        ) + history_checks
    for group, check, args in history_checks:
        if published:
            checks.append(run_group(version, group, check, binary, root, target, *args))
        else:
            print(f"history schema={version} group={group} outcome=blocked", flush=True)
    checks.append(run_group(version, "source-integrity", verify_source_integrity, source, original))
    return published and all(checks)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--combined-numbering-only", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    schema = Path(__file__).resolve().parents[1] / "crates/legacy-state/src/schema.sql"
    with tempfile.TemporaryDirectory(prefix="wfc-legacy-history-") as directory:
        passed = [fixture(binary, schema, Path(directory) / version, version, backend, args.combined_numbering_only)
                  for version, backend in (("39", "tmux"), ("40", "herdr"))]
    assert not Path(directory).exists()
    raise SystemExit(0 if all(passed) else 1)
