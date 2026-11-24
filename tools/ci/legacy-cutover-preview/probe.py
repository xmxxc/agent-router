"""Check public read-only diagnostics on private copies of an unfinished legacy ledger."""
from contextlib import closing
import json
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "tests"))
from legacy_history import post
from legacy_offline_import import (
    CURRENT, DISPATCH, FIRST, OLD, SCOPE, build_source, denied, digest, identifier, output, run,
)
from legacy_public import snapshot

IN_FLIGHT = "a-000000000041"
RELAY_PID = 2_147_483_000
RELAY_START = "fixture-relay-start"


def build_copies(root, backend):
    schema = REPO / "crates/legacy-state/src/schema.sql"
    source, work, bodies = build_source(root, schema, backend)
    report = work / "in-flight.md"
    report.write_text("synthetic report draft\n")
    receipt = work / "missing-launch-receipt.json"
    handle = None
    if backend == "herdr":
        handle = {"endpoint": str(root / "missing-herdr.sock"), "workspace": "fixture-workspace",
                  "pane": "fixture-pane", "terminal": "fixture-terminal", "nonce": "fixture-nonce",
                  "service": {"pid": RELAY_PID - 1, "creation": "fixture-service-start"}}
    with closing(sqlite3.connect(source / "router.db")) as db, db:
        db.execute("""UPDATE dispatch SET current_round=3,current_attempt=?,report=?,started=13,
                      launch_receipt=?,backend_handle=? WHERE id=?""",
                   (IN_FLIGHT, str(report), str(receipt), json.dumps(handle) if handle else None, DISPATCH))
        db.execute("""INSERT INTO dispatch_round (dispatch,round,attempt,task,report,state,created)
                      VALUES (?,3,?,?,?,'running',13)""", (DISPATCH, IN_FLIGHT, str(work / "task.md"), str(report)))
        db.execute("INSERT INTO waiter (pid,kind,since,process_start) VALUES (?,'relay',14,?)", (RELAY_PID, RELAY_START))

    pending = root / "pending-copy"
    pending.mkdir()
    shutil.copyfile(source / "router.db", pending / "router.db")
    with closing(sqlite3.connect(pending / "router.db")) as db, db:
        db.execute("UPDATE dispatch_round SET state='unknown' WHERE dispatch=? AND attempt=?", (DISPATCH, IN_FLIGHT))
        db.execute("""INSERT INTO lifecycle_event
                      (id,dispatch,operation,round,attempt,from_attempt,result,owner_generation,owner_token,created)
                      VALUES (7,?,'close',3,?,?,'pending',7,?,15)""", (DISPATCH, IN_FLIGHT, IN_FLIGHT, OLD))
        pending_body = post(db, 62, DISPATCH, "worker", "closed",
                            {"round": 3, "attempt": IN_FLIGHT, "result": "pending"})
    return source, pending, bodies, pending_body, report, receipt, handle


def verify_preview(binary, root, backend):
    root.mkdir()
    (root / "home").mkdir()
    empty_path = root / "no-commands"
    empty_path.mkdir()
    env = {"PATH": str(empty_path), "WFC_TMUX_SOCKET": str(root / "missing-tmux.sock")}
    source, pending, bodies, pending_body, report, receipt, handle = build_copies(root, backend)
    before = snapshot(root)
    for state, phase in ((source, "running"), (pending, "unknown")):
        view = output(run(binary, root, state, "status", "--dispatch", DISPATCH, extra=env))
        row, = view["dispatches"]
        assert (row["dispatch"], row["round"], row["attempt"], row["report"]) == (DISPATCH, 3, IN_FLIGHT, str(report))
        assert (row["backend"], row["backendHandle"], row["launchReceipt"]) == (backend, handle, str(receipt))
        assert row["status"] == phase and row["backendAlive"] is None
        assert "Verify the original backend identity and worker callback" in row["diagnostic"]
        assert row["controllerOwner"]["generation"] == 7 and OLD not in json.dumps(view)
        assert [round_["attempt"] for round_ in row["rounds"]] == [FIRST, CURRENT, IN_FLIGHT]
        relay, = view["waiters"]
        assert (relay["pid"], relay["kind"], relay["processStart"], relay["liveness"]) == (
            RELAY_PID, "relay", RELAY_START, "unknown")

        expected = {**bodies, 62: pending_body} if state == pending else bodies
        assert [message["id"] for message in row["messages"]] == sorted(expected)
        for message in row["messages"]:
            number = message["id"]
            raw = expected[number]
            assert (message["messageId"], message["body"], message["bodyHash"]) == (
                identifier(number), raw, digest(raw.encode()))
        assert [(message["id"], message["state"], message["statusVersion"], message["delivered"])
                for message in view["undelivered"]] == [(60, "unknown", 3, False), (61, "unsent", 1, False)]
        if state == pending:
            event, = row["audit"]
            assert (event["id"], event["operation"], event["attempt"], event["result"]) == (7, "close", IN_FLIGHT, "pending")
            assert row["messages"][-1]["result"] == "pending" and row["messages"][-1]["to"] == "worker"
        else:
            assert row["audit"] == []

        preview = output(run(binary, root, state, "reclaim", "--dispatch", DISPATCH, extra=env))
        item, = preview["sessions"]
        assert preview["yes"] is False and (item["dispatch"], item["result"], item["reclaimable"]) == (DISPATCH, "unknown", False)
        assert snapshot(root) == before
        for args in (
            ("wait", "--scope", SCOPE, "--timeout", "0"),
            ("relay", "--scope", SCOPE, "--codex-thread", "11111111-2222-4333-8444-555555555555", "--for", "1"),
            ("ack", "60"),
            ("reclaim", "--yes", "--dispatch", DISPATCH),
        ):
            denied(run(binary, root, state, *args, extra=env), "legacy state write blocked")
            assert snapshot(root) == before
        print(json.dumps({"backend": backend, "phase": phase, "attempt": IN_FLIGHT, "relayLiveness": "unknown",
                          "receipts": sorted(expected), "sourceUnchanged": True}), flush=True)


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="wfc-legacy-cutover-preview-") as directory:
        for backend in ("tmux", "herdr"):
            verify_preview(binary, Path(directory).resolve() / backend, backend)
    assert not Path(directory).exists()
