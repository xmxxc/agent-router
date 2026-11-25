"""Exercise the tmux archive cutover with a built WFC and synthetic files only."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile

from legacy_public import seed_legacy, snapshot


HISTORY_ONLY = "tmux assignment history is read-only"
ATTEMPT = "a-000000000031"
LAUNCH = "1" * 32
CLOSED_LAUNCH = "2" * 32
R2_LAUNCH = "3" * 32


def save(path, value):
    path.write_text(json.dumps(value), encoding="utf-8")


def native_fixture(root, state):
    dispatches = state / "dispatches"
    dispatches.mkdir(parents=True)
    launches = state / "launches"
    launches.mkdir()
    messages = []
    cases = []
    for offset, dispatch in enumerate(("d-11111111", "d-22222222", "d-77777777")):
        identity = dict(dispatch=dispatch, round=1, attempt=ATTEMPT, role="fixture", scope=dispatch)
        task = root / f"task-{offset}.md"
        report = root / f"report-{offset}.md"
        task.write_text("historical task", encoding="utf-8")
        report.write_text("historical report", encoding="utf-8")
        row = dict(identity, task=str(task), report=str(report), cwd=str(root),
                   tmux=f"wfc-{dispatch}", tmuxSocket=str(root / "missing.sock"))
        if offset == 0:
            row.update(backend="tmux", host="codex", legacyLaunchId=CLOSED_LAUNCH)
            save(dispatches / f"{dispatch}.{ATTEMPT}.closed",
                 dict(identity, closeOperationId=f"close:{ATTEMPT}", closeEffect="session_closed",
                      resourceCleanup="confirmed", worker={"state": "cancelled"},
                      legacyExecutionId=ATTEMPT, launchId=CLOSED_LAUNCH))
            save(launches / f"{CLOSED_LAUNCH}.json",
                 dict(category="process_exited", launchId=CLOSED_LAUNCH, exitCode=37))
        elif offset == 1:
            row["create"] = dict(identity, result="accepted", receipt=str(launches / f"{LAUNCH}.json"))
            save(launches / f"{LAUNCH}.json",
                 dict(category="process_exited", launchId=LAUNCH, exitCode=23))
        else:
            row["create"] = dict(result="accepted", receipt=str(launches / f"{R2_LAUNCH}.json"))
            save(launches / f"{R2_LAUNCH}.json",
                 dict(category="process_exited", launchId=R2_LAUNCH, exitCode=17))
        save(dispatches / f"{dispatch}.json", row)
        number = offset * 10 + 1
        completion = dict(identity, id=number, messageId=f"m-{number:032x}", report=str(report),
                          state="received", statusVersion=3, head=None, dirty=None)
        question = dict(identity, id=number + 1, messageId=f"m-{number + 1:032x}",
                        question_payload={"text": "historical question", "options": []},
                        state="unsent", statusVersion=1)
        messages.extend((completion, question))
        cases.append((row, completion, question))
    save(state / "messages.json", messages)
    return cases


def run(binary, root, state, *args, code=0, extra=None):
    env = {"PATH": str(root / "no-commands"), "HOME": str(root),
           "WFC_STATE": str(state), "WFC_TMUX_SOCKET": str(root / "missing.sock")}
    env.update(extra or {})
    result = subprocess.run([binary, *map(str, args)], cwd=root, env=env,
                            text=True, capture_output=True, timeout=8)
    assert result.returncode == code, (args, result)
    if code == 1:
        assert not result.stdout, result
        return result.stderr
    return json.loads(result.stdout)


def verify_native(binary, root):
    state = root / "native"
    cases = native_fixture(root, state)
    before = snapshot(root)
    listed = run(binary, root, state, "status")
    assert listed["undelivered"] == []
    rows = listed["dispatches"]
    assert [row["dispatch"] for row in rows] == [row["dispatch"] for row, _, _ in cases]
    assert run(binary, root, state, "status", "--active")["reclaimable"] == []
    for row, done, question in cases:
        dispatch = row["dispatch"]
        selected, = run(binary, root, state, "status", "--dispatch", dispatch)["dispatches"]
        assert selected["backend"] == "tmux" and selected["historyOnly"] is True
        assert selected["backendAlive"] is None and selected["tmuxAlive"] is None
        assert selected["backendDiagnostic"] == HISTORY_ONLY
        assert selected["rounds"][0]["result"]["messageId"] == done["messageId"]
        assert [(m["id"], m["messageId"], m["state"], m["statusVersion"])
                for m in selected["messages"]] == [
                    (m["id"], m["messageId"], m["state"], m["statusVersion"])
                    for m in (done, question)]
        if row.get("backend") == "tmux":
            assert selected["closeOperationId"] == f"close:{ATTEMPT}"
            assert selected["resourceCleanup"] == "confirmed"
            assert selected["worker"]["state"] == "exited"
            assert selected["worker"]["exit_code"] == 37
        else:
            assert selected["worker"]["state"] == "exited"
            assert selected["worker"]["exit_code"] == (23 if dispatch == "d-22222222" else 17)
        item, = run(binary, root, state, "reclaim", "--dispatch", dispatch)["sessions"]
        assert item["result"] == "unknown" and item["reclaimable"] is False
        for args in (
            ("close", dispatch), ("reclaim", "--yes", "--dispatch", dispatch),
            ("continue", dispatch, "--task", row["task"], "--report", root / "next.md"),
            ("resume", dispatch), ("handoff", dispatch, "--host", "claude"),
            ("nudge", dispatch, "--text", "fixture"),
            ("reply", question["id"], "fixture"), ("ack", done["messageId"]),
            ("wait", "--dispatch", dispatch, "--timeout", "0"),
            ("ask", "fixture", "--attempt", ATTEMPT),
            ("done", "--report", row["report"], "--attempt", ATTEMPT),
        ):
            assert HISTORY_ONLY in run(binary, root, state, *args, code=1,
                                        extra={"WFC_DISPATCH": dispatch}), args
            assert snapshot(root) == before, args
    receipt = state / "launches" / f"{LAUNCH}.json"
    original = receipt.read_bytes()
    save(receipt, dict(category="process_exited", launchId="0" * 32, exitCode=23))
    selected, = run(binary, root, state, "status", "--dispatch", cases[1][0]["dispatch"])["dispatches"]
    assert selected["worker"]["state"] == "unknown" and selected["backendAlive"] is None
    receipt.write_bytes(original)
    assert snapshot(root) == before
    # A bound close receipt can enrich worker state without proving liveness.
    dispatch = cases[0][0]["dispatch"]
    closed = state / "dispatches" / f"{dispatch}.{ATTEMPT}.closed"
    original = closed.read_bytes()
    save(closed, dict(json.loads(original), legacyExecutionId="a-000000000099"))
    item, = run(binary, root, state, "status", "--dispatch", dispatch)["dispatches"]
    assert item["worker"]["state"] == "cancelled" and item["backendAlive"] is None
    closed.write_bytes(original)
    assert snapshot(root) == before
    assert run(binary, root, state, "wait", "--timeout", "0", code=3)["messages"] == []
    assert json.loads((state / "messages.json").read_bytes())[1]["statusVersion"] == 1
    print("native: explicit and inferred history, preserved receipts and no execution target")


def verify_mixed(binary, root):
    state = root / "mixed"
    dispatches = state / "dispatches"
    dispatches.mkdir(parents=True)
    dispatch, bundled = "d-33333333", "d-44444444"
    broken, unverified = "d-55555555", "d-66666666"
    current = "a-000000000032"
    first = "a-000000000030"
    identity = dict(dispatch=dispatch, round=1, attempt=current, role="fixture", scope=dispatch)
    audit = [dict(dispatch=dispatch, operation="resume", fromAttempt=first, round=1,
                  attempt=ATTEMPT, toAttempt=ATTEMPT, result="accepted"),
             dict(dispatch=dispatch, operation="handoff", fromBackend="tmux",
                  fromRound=1, fromAttempt=ATTEMPT, round=1, attempt=current,
                  toAttempt=current, result="accepted", phase="complete",
                  target={"backend": "herdr"})]
    rounds = [dict(round=1, attempt=attempt, state="unknown") for attempt in (first, ATTEMPT)]
    save(dispatches / f"{dispatch}.json", dict(identity, backend="herdr", host="codex",
                                              audit=audit, rounds=rounds))
    save(dispatches / f"{bundled}.json", dict(identity, dispatch=bundled, scope=bundled,
                                             backend="bundled"))
    (dispatches / f"{broken}.pending").write_text("{", encoding="utf-8")
    save(dispatches / f"{unverified}.pending",
         dict(identity, dispatch=unverified, scope=unverified, attempt="invalid", backend="herdr"))

    def message(number, **fields):
        return dict(identity, id=number, messageId=f"m-{number:032x}", **fields)

    raw = root / "task-0.md"
    pending = [
        message(21, received=False, report=str(raw), olderField={"keep": [1, None]}),
        message(22, received=True, report=str(raw)),
        message(23, state="unknown", statusVersion=2, report=str(raw)),
    ]
    for row in pending:
        row["attempt"] = ATTEMPT
        row.pop("scope")
    pending[0]["attempt"] = first
    healthy = [
        message(24, state="unsent", statusVersion=1, report=str(raw)),
        message(25, state="unsent", statusVersion=1, report=str(raw)),
    ]
    healthy[1].update(dispatch=bundled, scope=bundled)
    damaged = [
        message(26, received=False, report=str(raw)),
        message(27, received=False, report=str(raw)),
    ]
    for row, damaged_dispatch in zip(damaged, (broken, unverified), strict=True):
        row.update(dispatch=damaged_dispatch, scope=damaged_dispatch)
    ledger = state / "messages.json"
    save(ledger, pending + healthy + damaged)

    selected = run(binary, root, state, "status", "--dispatch", dispatch)
    assert [row["id"] for row in selected["dispatches"][0]["messages"]] == [21, 22, 23, 24]
    assert [row["id"] for row in selected["undelivered"]] == [24]
    for damaged_dispatch in (broken, unverified):
        view = run(binary, root, state, "status", "--dispatch", damaged_dispatch)
        assert view["undelivered"] == [] and view["dispatches"][0]["messages"]

    before = snapshot(state)
    assert HISTORY_ONLY in run(binary, root, state, "ack", 22, code=1)
    assert HISTORY_ONLY in run(binary, root, state, "resolve", 23, "--actor", "fixture",
                                "--reason", "fixture", "--evidence", "fixture",
                                "--body-hash", "0" * 64, code=1)
    for damaged_dispatch in (broken, unverified):
        assert "cannot verify assignment publication identity" in run(
            binary, root, state, "wait", "--dispatch", damaged_dispatch, "--timeout", "0", code=1)
    assert snapshot(state) == before

    received = run(binary, root, state, "wait", "--timeout", "0")["messages"]
    assert [(m["id"], m["state"]) for m in received] == [(24, "received"), (25, "received")]
    ack = run(binary, root, state, "ack", 24)
    assert ack["state"] == "processed"
    retained = json.loads(ledger.read_bytes())
    assert retained[:3] == pending and retained[-2:] == damaged
    assert run(binary, root, state, "wait", "--timeout", "0", code=3)["messages"] == []
    print("mixed: retired attempts and damaged reservations preserved, active receipts delivered")


def verify_sqlite(binary, root):
    state = root / "sqlite-only"
    seed_legacy(state)
    before = snapshot(state)
    item, = run(binary, root, state, "status", "--dispatch", "d-00000030")["dispatches"]
    assert item["backendAlive"] is None and item["messages"][0]["id"] == 30
    listed, = run(binary, root, state, "status")["dispatches"]
    assert listed["dispatch"] == item["dispatch"] and listed["backendAlive"] is None
    for args in (("close", item["dispatch"]), ("reclaim", "--yes", "--dispatch", item["dispatch"]),
                 ("resume", item["dispatch"]), ("handoff", item["dispatch"], "--host", "claude"),
                 ("continue", item["dispatch"], "--task", root / "task-0.md", "--report", root / "new.md"),
                 ("nudge", item["dispatch"], "--text", "fixture")):
        assert "legacy state write blocked" in run(binary, root, state, *args, code=1)
    assert snapshot(state) == before
    print("SQLite-only: history visible, live state unknown, original bytes preserved")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="wfc-tmux-archive-") as directory:
        root = Path(directory).resolve()
        (root / "no-commands").mkdir()
        verify_native(binary, root)
        verify_mixed(binary, root)
        verify_sqlite(binary, root)
