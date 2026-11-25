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


def save(path, value):
    path.write_text(json.dumps(value), encoding="utf-8")


def native_fixture(root, state):
    dispatches = state / "dispatches"
    dispatches.mkdir(parents=True)
    launches = state / "launches"
    launches.mkdir()
    messages = []
    cases = []
    for offset, dispatch in enumerate(("d-11111111", "d-22222222")):
        identity = dict(dispatch=dispatch, round=1, attempt=ATTEMPT, role="fixture", scope=dispatch)
        task = root / f"task-{offset}.md"
        report = root / f"report-{offset}.md"
        task.write_text("historical task", encoding="utf-8")
        report.write_text("historical report", encoding="utf-8")
        row = dict(identity, task=str(task), report=str(report), cwd=str(root),
                   tmux=f"wfc-{dispatch}", tmuxSocket=str(root / "missing.sock"))
        if offset == 0:
            row.update(backend="tmux", host="codex", executionId=ATTEMPT)
            save(dispatches / f"{dispatch}.{ATTEMPT}.closed",
                 dict(identity, closeOperationId=f"close:{ATTEMPT}", closeEffect="session_closed",
                      resourceCleanup="confirmed", worker={"state": "cancelled"}))
        else:
            row["create"] = dict(identity, result="accepted", receipt=str(launches / f"{LAUNCH}.json"))
            save(launches / f"{LAUNCH}.json",
                 dict(category="process_exited", launchId=LAUNCH, exitCode=23))
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


def run(binary, root, state, *args, code=0, extra=None, payload=None):
    env = {"PATH": str(root / "no-commands"), "HOME": str(root),
           "WFC_STATE": str(state), "WFC_TMUX_SOCKET": str(root / "missing.sock")}
    env.update(extra or {})
    result = subprocess.run([binary, *map(str, args)], cwd=root, env=env,
                            text=True, input=payload, capture_output=True, timeout=8)
    assert result.returncode == code, (args, result)
    if code == 1:
        assert not result.stdout, result
        return result.stderr
    return json.loads(result.stdout)


def verify_native(binary, root):
    state = root / "native"
    cases = native_fixture(root, state)
    before = snapshot(root)
    rows = run(binary, root, state, "status")["dispatches"]
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
        else:
            assert selected["worker"]["state"] == "exited"
            assert selected["worker"]["exit_code"] == 23
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
        for request in (dict(op="inspect", readOnly=True),
                        dict(op="create", freshDispatch=True, cwd=str(root), argv=["fixture"])):
            assert HISTORY_ONLY in run(binary, root, state, "--tmux-backend", state, code=1,
                                        payload=json.dumps(dict(request, dispatch=dispatch)))
            assert snapshot(root) == before
        assert HISTORY_ONLY in run(binary, root, state, "--tmux-host", LAUNCH, code=1,
                                    extra={"WFC_DISPATCH": dispatch})
        assert HISTORY_ONLY in run(binary, root, state, "--tmux-attach", state, dispatch,
                                    root / "missing.sock", LAUNCH, code=1)
        assert snapshot(root) == before
    receipt = state / "launches" / f"{LAUNCH}.json"
    original = receipt.read_bytes()
    save(receipt, dict(category="process_exited", launchId="0" * 32, exitCode=23))
    selected, = run(binary, root, state, "status", "--dispatch", cases[1][0]["dispatch"])["dispatches"]
    assert selected["worker"]["state"] == "unknown" and selected["backendAlive"] is None
    receipt.write_bytes(original)
    assert snapshot(root) == before
    launch = state / "launches" / f"{LAUNCH}.submission"
    save(launch, dict(dispatch=cases[0][0]["dispatch"], launchId=LAUNCH, phase="InFlight"))
    before = snapshot(root)
    assert HISTORY_ONLY in run(binary, root, state, "--tmux-record-session", state, LAUNCH,
                                "2", "$1", "%1", root / "missing.sock", "1", LAUNCH, code=1)
    assert snapshot(root) == before
    # A global wait remains available, but the historical question is not claimed.
    assert run(binary, root, state, "wait", "--timeout", "0", code=3)["messages"] == []
    assert json.loads((state / "messages.json").read_bytes())[1]["statusVersion"] == 1
    messages = json.loads((state / "messages.json").read_bytes())
    for dispatch, number, backend in (("d-33333333", 21, "herdr"), ("d-44444444", 22, None)):
        row, _, question = cases[0]
        row_path = state / "dispatches" / f"{dispatch}.json"
        if backend:
            save(row_path, dict(row, dispatch=dispatch, scope=dispatch, backend=backend))
        else:
            row_path.write_text("broken JSON", encoding="utf-8")
        messages.append(dict(question, dispatch=dispatch, scope=dispatch, id=number,
                             messageId=f"m-{number:032x}"))
    save(state / "messages.json", messages)
    before = snapshot(root)
    assert "tmux backend is retired" in run(binary, root, state, "handoff", "d-33333333",
                                              "--host", "claude", "--backend", "tmux", code=1)
    assert "tmux backend is retired" in run(binary, root, state, "dispatch", "--host", "codex",
                                              "--backend", "tmux", "--task", cases[0][0]["task"],
                                              "--cwd", root, "--report", root / "new.md", code=1)
    assert snapshot(root) == before
    received, = run(binary, root, state, "wait", "--timeout", "0")["messages"]
    assert received["id"] == 21 and received["state"] == "received"
    retained = json.loads((state / "messages.json").read_bytes())
    assert retained[-1]["state"] == "unsent" and retained[1]["state"] == "unsent"
    print("native: explicit and inferred history, preserved receipts and no execution target")


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
    request = json.dumps(dict(op="inspect", dispatch=item["dispatch"], readOnly=True))
    assert "legacy state write blocked" in run(binary, root, state, "--tmux-backend", state,
                                                code=1, payload=request)
    assert snapshot(state) == before
    print("SQLite-only: history visible, live state unknown, original bytes preserved")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="wfc-tmux-archive-") as directory:
        root = Path(directory).resolve()
        (root / "no-commands").mkdir()
        verify_native(binary, root)
        verify_sqlite(binary, root)
