"""Combine the synthetic offline import and public bundled reply on one state."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

PRODUCT = Path.cwd()
sys.path.insert(0, str(PRODUCT / "tests"))
from legacy_offline_import import (  # Reuse the stopped history and owner rotation.
    CURRENT, DISPATCH, FIRST, NEW, OLD, SCOPE, build_source, digest,
    identifier, import_args, proof_for, run, snapshot,
)


def await_value(probe, label):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        value = probe()
        if value is not None:
            return value
        time.sleep(0.05)
    raise AssertionError(label)


def process_start(pid):
    try:
        # The executable name in /proc/PID/stat may itself contain spaces or ')'.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
        return int(fields[19])
    except FileNotFoundError:
        return None


def verify(binary, root, delegation):
    (root / "home").mkdir(mode=0o700)
    source, work, bodies = build_source(root, PRODUCT / "crates/legacy-state/src/schema.sql", "tmux")
    target, reports = root / "native", root / "reports"
    reports.mkdir(mode=0o700)
    report = reports / "result.md"
    config = root / "launch.json"
    config.write_text(json.dumps({"hosts": {"codex": {"command": [
        "/usr/bin/timeout", "45s", "/usr/bin/python3",
        str(PRODUCT / "crates/herdr/tests/composer.py"),
        "one", "--submit-first", "--reply-worker", "bundled-paste",
    ]}}}))
    empty_path = root / "bin"
    empty_path.mkdir()
    env = {"PATH": str(empty_path), "WFC_HERDR_ENDPOINT": str(root / "missing-herdr.sock")}

    def result(*args, token=NEW, code=0, diagnostic=None, extra=None):
        reply = run(binary, root, target, *args, token=token, extra={**env, **(extra or {})})
        assert reply.returncode == code, (args[0], reply.returncode)
        if diagnostic:
            assert not reply.stdout and diagnostic in reply.stderr, args[0]
            return
        assert not reply.stderr, args[0]
        return json.loads(reply.stdout)

    def status(dispatch):
        view = result("status", "--dispatch", dispatch, token=None)
        assert view["state"] == str(target)
        row, = view["dispatches"]
        assert row["dispatch"] == dispatch
        return row

    original = snapshot(source)
    source_sha = original["router.db"][1]
    proof = proof_for(root / "stop-proof.json", source / "router.db", target)
    imported = result(*import_args(source, proof), token=None)
    assert (imported["result"], imported["duplicate"], imported["messageHighWater"]) == (
        "completed", False, 90)
    assert (imported["dispatches"], imported["messages"], imported["sourceSha256"]) == (
        1, 4, source_sha)
    assert imported["stopProofSha256"] == digest(proof.read_bytes())
    assert imported["state"] == str(target)
    backup = target / "legacy-source/router.db"
    assert imported["backup"] == str(backup)
    assert target.stat().st_mode & 0o777 == 0o700
    assert backup.stat().st_mode & 0o777 == 0o600
    archived = snapshot(backup.parent)
    assert archived["router.db"] == original["router.db"]
    assert not any(name.startswith("router.db-") for name in archived)

    old = status(DISPATCH)
    assert (old["legacyArchived"], old["backendAlive"], old["scope"], old["attempt"]) == (
        True, False, SCOPE, CURRENT)
    assert old["controllerOwner"]["generation"] == 8
    assert [row["attempt"] for row in old["rounds"]] == [FIRST, CURRENT]
    expect_old = {41: ("received", 3), 49: ("processed", 4),
                  60: ("unknown", 3), 61: ("unsent", 1)}
    assert {m["id"]: (m["state"], m["statusVersion"]) for m in old["messages"]} == expect_old
    for message in old["messages"]:
        number = message["id"]
        raw = bodies[number]
        assert (message["messageId"], message["body"], message["bodyHash"]) == (
            identifier(number), raw, digest(raw.encode()))
        assert message.get("legacyDeliveryHeld", False) == (number == 61)
    result("ack", "41", token=OLD, code=1, diagnostic="stale_controller_owner")
    before_ledger = digest((target / "messages.json").read_bytes())

    created = result(
        "dispatch", "--host", "codex", "--label", "migrated-reply", "--scope", SCOPE,
        "--role", "worker", "--cwd", work, "--task", work / "task.md", "--report", report,
        "--launch-config", config, extra={"WFC_BUNDLED_CGROUP_ROOT": str(delegation)},
    )
    dispatch, attempt, round_number = (created[key] for key in ("dispatch", "attempt", "round"))
    close_confirmed = False
    try:
        assert (created["backend"], created["scope"], round_number) == ("bundled", SCOPE, 1)
        assert dispatch != DISPATCH and attempt not in (FIRST, CURRENT)
        create = created["create"]
        ticket, identity = create["ticket"], create["identity"]
        assert create["result"] == "accepted" and created["backendHandle"] == ticket
        assert (ticket["unit"], ticket["attempt"]) == (dispatch, attempt)
        containment = Path(identity["containment"]["path"])
        assert containment.resolve(strict=True).is_relative_to(delegation) and containment != delegation
        worker_pid, guardian_pid = (identity[role]["pid"] for role in ("worker", "guardian"))
        lives = {identity[role]["pid"]: identity[role]["start_ticks"]
                 for role in ("worker", "guardian")}
        assert len(lives) == 2 and all(process_start(pid) == start for pid, start in lives.items())
        assert Path(f"/proc/{guardian_pid}/exe").samefile(binary)
        assert f"0::/{containment.relative_to('/sys/fs/cgroup').as_posix()}\n" == (
            Path(f"/proc/{worker_pid}/cgroup").read_text())
        running = status(dispatch)
        assert running["backendAlive"] is True and running["worker"]["state"] == "running"
        assert running["backendHandle"] == ticket
        assert running["controllerOwner"]["generation"] == 8
        assert not (work / "question.json").exists()
        (work / "ask-ready").touch()
        question_path = work / "question.json"
        question = await_value(
            lambda: json.loads(question_path.read_text()) if question_path.is_file() else None,
            "controlled worker ask receipt",
        )
        assert question["message"] == 91 and question["scope"] == SCOPE

        def delivered(kind, number):
            view = result("wait", "--scope", SCOPE, "--dispatch", dispatch, "--timeout", "8")
            message, = view["messages"]
            assert (message["type"], message["id"], message["dispatch"], message["attempt"],
                    message["round"], message["scope"], message["state"]) == (
                        kind, number, dispatch, attempt, round_number, SCOPE, "received")
            return message

        asked = delivered("question", 91)
        assert asked["messageId"] == question["messageId"]
        assert asked["question"] == "One controlled question?"

        # This read-only viewport query waits for the existing composer to render.
        # The answer itself goes through the public reply command below.
        request = json.dumps({"op": "read", "ticket": ticket}) + "\n"

        def viewport():
            read = subprocess.run(
                [binary, "--bundled", target], input=request, cwd=root,
                env={**env, "HOME": str(root / "home")}, text=True, capture_output=True, timeout=5,
            )
            assert read.returncode == 0 and not read.stderr
            value = json.loads(read.stdout)
            assert value["result"] == "accepted" and value["ticket"] == ticket
            page = value["data"]
            assert (page["screen"], page["exit"], page["eof"]) == ("Primary", None, False)
            return value

        def prompt_sequence():
            value = viewport()
            if "Ask Codex to do anything" in value["data"]["text"]:
                return value["last_input"]
            return None

        sequence = await_value(prompt_sequence, "controlled composer prompt")
        result("reply", "91", "confirmed", token=OLD, code=1, diagnostic="stale_controller_owner")
        reply = result("reply", "91", "confirmed")
        assert reply == {"replied": 91, "dispatch": dispatch}
        assert viewport()["last_input"] == sequence + 2
        accepted = work / "accepted"
        await_value(lambda: True if accepted.is_file() and accepted.stat().st_size else None,
                    "controlled composer accepted the answer")
        expected_answer = b"[router] Answer to question 91: confirmed\n"
        assert accepted.read_bytes() == expected_answer
        assert int((work / "paste-enter-ns").read_text()) >= 450_000_000

        def reply_row():
            row, = [m for m in status(dispatch)["messages"] if m["type"] == "reply"]
            return row

        answer = reply_row()
        assert (answer["id"], answer["to"], answer["questionMessageId"], answer["inputResult"],
                answer["answer"], answer["dispatch"], answer["attempt"], answer["round"],
                answer["scope"]) == (
                    92, 91, question["messageId"], "accepted", "confirmed",
                    dispatch, attempt, round_number, SCOPE)
        result("reply", "91", "repeat", code=1, diagnostic="already answered")
        assert viewport()["last_input"] == sequence + 2
        assert reply_row() == answer and accepted.read_bytes() == expected_answer

        (work / "finish").touch()
        done = delivered("done", 93)
        assert done["report"] == str(report)
        assert report.read_bytes() == b"original worker received the answer\n"
        processed = result("ack", "93")
        assert (processed["message"], processed["messageId"], processed["state"],
                processed["statusVersion"], processed["duplicate"], processed["scope"]) == (
                    93, done["messageId"], "processed", done["statusVersion"] + 1, False, SCOPE)
        assert result("ack", "93") == {**processed, "duplicate": True}

        closed = result("close", dispatch)
        assert closed["proof"]["state"] == "closed"
        assert closed["proof"]["identity"] == identity
        assert (closed["result"], closed["cleanup"], closed["ticket"]) == (
            "completed", "confirmed", ticket)
        close_confirmed = True
        retry = result("close", dispatch)
        assert (retry["result"], retry["cleanup"], retry["ticket"], retry["proof"]) == (
            "completed", "confirmed", ticket, closed["proof"])
        assert not containment.exists()
        for pid, start in lives.items():
            await_value(lambda: True if process_start(pid) != start else None,
                        "execution process retired")

        final = status(dispatch)
        assert final["status"] == "completed" and final["backendAlive"] is False
        assert final["backendObservation"]["cleanup"] == "confirmed"
        assert final["rounds"][-1]["result"]["messageId"] == done["messageId"]
        assert final["controllerOwner"]["generation"] == 8
        records = {m["id"]: m for m in final["messages"]}
        assert set(records) == {91, 92, 93} and records[92] == answer
        assert records[93]["state"] == "processed"
        assert records[93]["messageId"] == done["messageId"]
        uids = {m["messageId"] for m in [*old["messages"], *final["messages"]]}
        assert len(uids) == 7 and all(uid.startswith("m-") for uid in uids)
        assert status(DISPATCH) == old
        assert snapshot(source) == original and snapshot(backup.parent) == archived
        after_ledger = digest((target / "messages.json").read_bytes())
        assert before_ledger != after_ledger
        return {
            "oldIdsAndStates": expect_old, "newIds": [91, 92, 93], "highWater": 90,
            "ownerGeneration": 8, "staleOwnerRejected": True, "sourceSha256": source_sha,
            "backupSha256": archived["router.db"][1], "stopProofSha256": imported["stopProofSha256"],
            "ledgerBefore": before_ledger, "ledgerAfter": after_ledger,
            "reportSha256": digest(report.read_bytes()), "replyTo": 91,
            "ackId": 93, "ackState": "processed", "retryKeptIds": True,
            "sourceUnchanged": True, "backupUnchanged": True, "executionRetired": True,
        }
    finally:
        # Only this dispatch was launched here. The runner also verifies its enclosing unit retires.
        if not close_confirmed:
            try:
                run(binary, root, target, "close", dispatch, token=NEW, extra=env)
            except (OSError, subprocess.TimeoutExpired):
                pass


if __name__ == "__main__":
    binary, delegation = (Path(arg).resolve(strict=True) for arg in sys.argv[1:])
    with tempfile.TemporaryDirectory(prefix="wfc-migrated-reply-") as directory:
        evidence = verify(binary, Path(directory).resolve(), delegation)
    assert not Path(directory).exists()
    print(json.dumps(evidence, sort_keys=True), flush=True)
    print("BUNDLED_MIGRATED_ASK_REPLY_PASS", flush=True)
