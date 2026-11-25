"""Use one WFC ELF and two isolated synthetic hosts for a default bundled handoff."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


HOST = r"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

name = sys.argv[1]
if name == "opencode":
    previous = json.loads(Path("original.json").read_text())
    assert not Path(previous["group"]).exists()
    try:
        fields = Path(f"/proc/{previous['worker']['pid']}/stat").read_text().rsplit(") ", 1)[1].split()
    except FileNotFoundError:
        pass
    else:
        assert fields[0] in ("Z", "X") or int(fields[19]) != previous["worker"]["start_ticks"]
    assert "--prompt" in sys.argv
    brief = sys.argv[sys.argv.index("--prompt") + 1]
    assert "handed off to a new host" in brief and os.environ["WFC_TASK"] in brief
assert "WFC_CONTROLLER_TOKEN" not in os.environ
with Path(f"{name}-starts").open("a") as log:
    log.write(os.environ["WFC_ATTEMPT"] + "\n")
for _ in range(600):
    if Path("finish").exists():
        if name == "opencode":
            Path(os.environ["WFC_REPORT"]).write_text("original task completed by the replacement\n")
            result = subprocess.check_output([
                os.environ["WFC_BIN"], "done", "--report", os.environ["WFC_REPORT"],
                "--attempt", os.environ["WFC_ATTEMPT"], "--message-id", "handoff-done",
            ])
            Path("receipt.tmp").write_bytes(result)
            Path("receipt.tmp").replace("receipt.json")
        break
    time.sleep(.05)
"""


def until(check):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if value := check():
            return value
        time.sleep(.04)
    raise AssertionError("bundled handoff fixture timed out")


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def classification(value, *allowed):
    return value if value in allowed else "missing" if value is None else "other"


def log_close(phase, answer, ticket, round_number):
    proof = answer.get("proof") or {}
    observed = answer.get("ticket")
    print("BUNDLED_HANDOFF_CLOSE", json.dumps({
        "phase": phase, "round": round_number, "ticket_sha256": digest(ticket),
        "observed_ticket_sha256": None if observed is None else digest(observed),
        "result": classification(answer.get("result"), "accepted", "completed", "uncertain", "timeout"),
        "cleanup": classification(answer.get("cleanup"), "confirmed", "unknown"),
        "scope": classification(answer.get("scope"), "execution"),
        "category": classification(answer.get("category"), "bundled_restart"),
        "proof_state": classification(proof.get("state"), "closed", "exited", "faulted", "startup-aborted"),
        "proof_sha256": digest(proof) if proof else None,
    }, sort_keys=True), flush=True)


def terminal_proof(answer, ticket, identity):
    assert answer.get("scope") is None and answer.get("execution_proof") is None, "partial cleanup"
    assert answer.get("ticket") in (None, ticket), "close ticket mismatch"
    if answer["result"] == "completed":
        assert answer["ticket"] == ticket and answer["cleanup"] == "confirmed", "unbound cleanup"
        proof = answer["proof"]
        assert proof["state"] in ("closed", "exited"), "unexpected terminal state"
        assert proof["proves"] == "execution-empty" and proof["identity"] == identity, "wrong cleanup proof"
        return proof
    assert answer.get("proof") is None and answer.get("cleanup") in (None, "unknown"), "invalid pending cleanup"
    return None


def poll_budget():
    deadline = time.monotonic() + 12
    for _ in range(40):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        yield remaining
        time.sleep(min(.3, max(0, deadline - time.monotonic())))


def close_target(cli, dispatch, handoff, launch):
    ticket = handoff["ticket"]
    identity = launch["identity"]
    round_number = handoff["round"]
    attempt = handoff["attempt"]
    print("BUNDLED_HANDOFF_BEFORE_CLOSE", json.dumps({
        "round": round_number, "ticket_sha256": digest(ticket),
        "worker_stopped": stopped(identity["worker"]), "runtime_stopped": stopped(identity["service"]),
        "group_removed": path_absent(Path(identity["containment"]["path"])),
    }, sort_keys=True), flush=True)
    first = cli("close", dispatch)
    log_close("initial", first, ticket, round_number)
    assert (first["operation"], first["dispatch"], first["round"], first["attempt"]) == (
        "close", dispatch, round_number, attempt), "close assignment mismatch"
    assert first["result"] == "completed" or (
        first["result"] == "uncertain" and first.get("cleanup") == "unknown"), "unexpected close result"
    first_proof = terminal_proof(first, ticket, identity)

    for remaining in poll_budget():
        # An unsent close may have withdrawn its marker. Only status can
        # safely wait here without issuing a fresh close request.
        try:
            current, = cli("status", "--dispatch", dispatch, timeout=min(remaining, 3))["dispatches"]
        except subprocess.TimeoutExpired:
            log_close("read", {"result": "timeout"}, ticket, round_number)
        else:
            read = current.get("backendObservation", {})
            log_close("read", read, ticket, round_number)
            assert (current["dispatch"], current["round"], current["attempt"], current["executionId"],
                    current["backend"], current["host"]) == (
                dispatch, round_number, attempt, attempt, "bundled", "opencode"), "status assignment mismatch"
            assert current["create"] == launch and current["backendHandle"] == ticket, "status binding changed"
            assert read["ticket"] == ticket and read["result"] in ("accepted", "uncertain", "completed"), (
                "unexpected receipt observation")
            proof = terminal_proof(read, ticket, identity)
            if proof:
                assert first_proof is None or proof == first_proof, "terminal proof changed"
                assert current["backendAlive"] is False, "terminal receipt is still live"
                return
            assert first_proof is None, "terminal receipt disappeared"
    raise AssertionError("replacement signed close receipt unavailable")


def path_absent(path):
    try:
        path.stat()
    except FileNotFoundError:
        return True
    return False


def stopped(process):
    try:
        fields = Path(f"/proc/{process['pid']}/stat").read_text().rsplit(") ", 1)[1].split()
    except FileNotFoundError:
        return True
    return fields[0] in ("Z", "X") or int(fields[19]) != process["start_ticks"]


def replacement_for_cleanup(cli, dispatch, scope, old, handoff):
    """Recover this fixture's replacement even if the first status failed."""
    for remaining in poll_budget():
        try:
            row, = cli("status", "--dispatch", dispatch, timeout=min(remaining, 3))["dispatches"]
            found = cleanup_candidate(row, dispatch, scope, old, handoff)
            if found is not None:
                return found
        except (AssertionError, ValueError, subprocess.TimeoutExpired):
            pass
    return None


def cleanup_candidate(row, dispatch, scope, old, handoff):
    if (row["dispatch"], row["scope"], row["round"], row["backend"], row["host"]) != (
            dispatch, scope, old["round"], "bundled", "opencode"):
        return None
    attempt = row["attempt"]
    event = next((event for event in reversed(row["audit"]) if event["operation"] == "handoff"), None)
    if (not event or event["priorCreate"] != old or event["toAttempt"] != attempt
            or row["executionId"] != attempt or attempt == old["attempt"]):
        return None
    read = row.get("backendObservation") or {}
    ticket = read.get("ticket") or {}
    if (ticket.get("version"), ticket.get("unit"), ticket.get("attempt")) != (2, dispatch, attempt):
        return None
    if handoff is not None and (handoff["attempt"] != attempt or handoff.get("ticket") not in (None, ticket)):
        return None
    launch = row.get("create") or {}
    if launch.get("result") == "accepted" and launch.get("ticket") == ticket:
        return launch
    proof = read.get("proof") or {}
    if (read.get("result") == "completed" and read.get("cleanup") == "confirmed"
            and read.get("scope") is None and proof.get("proves") == "execution-empty"):
        return {"round": row["round"], "ticket": ticket, "identity": proof["identity"]}
    return None


def check_retired(launches):
    checks = []
    for _ in poll_budget():
        checks = []
        for launch in launches:
            identity = launch["identity"]
            group = Path(identity["containment"]["path"])
            unit = f"wfc-bundled-{launch['ticket']['generation']}.service"
            assert identity["userService"]["unit"] == unit == group.parent.name, "service binding changed"
            checks.append({"round": launch["round"], "ticket_sha256": digest(launch["ticket"]),
                           "group_removed": path_absent(group), "service_cgroup_removed": path_absent(group.parent),
                           "processes_stopped": all(stopped(identity[key]) for key in (
                               "guardian", "service", "kernel", "worker"))})
        if all(retired(check) for check in checks):
            break
    for check in checks:
        print("BUNDLED_HANDOFF_CLEANUP", json.dumps(check, sort_keys=True), flush=True)
    return all(retired(check) for check in checks)


def retired(check):
    return check["group_removed"] and check["service_cgroup_removed"] and check["processes_stopped"]


def exercise(binary, root):
    state = root / "state"
    home = root / "home"
    home.mkdir()
    task = root / "task.md"
    task.write_text("a controlled original task\n")
    report = root / "report.md"
    (root / "worker.py").write_text(HOST)
    config = root / "launch.json"
    config.write_text(json.dumps({"hosts": {host: {"command": [
        sys.executable, str(root / "worker.py"), host
    ]} for host in ("codex", "opencode")}}))
    env = {"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8", "HOME": str(home), "WFC_STATE": str(state)}
    scope = "handoff-fixture"

    def cli(*args, code=0, timeout=35):
        result = subprocess.run([binary, *map(str, args)], cwd=root, env=env,
                                capture_output=True, text=True, timeout=timeout)
        assert result.returncode == code, (args[0], "exit", result.returncode)
        return json.loads(result.stdout) if code == 0 else result.stderr

    owner = cli("acquire", "--scope", scope, "--owner", "fixture", "--lease", "180")
    env["WFC_CONTROLLER_TOKEN"] = owner["token"]
    original = cli("dispatch", "--host", "codex", "--scope", scope, "--cwd", root, "--task", task,
                   "--report", report, "--launch-config", config)
    dispatch = original["dispatch"]
    old = original["create"]
    old_group = old["identity"]["containment"]["path"]
    old_worker = old["identity"]["worker"]
    launches = [old]
    handoff_requested = False
    handoff = None
    try:
        until(lambda: (root / "codex-starts").is_file() and (root / "codex-starts").stat().st_size)
        (root / "original.json").write_text(json.dumps({"group": old_group, "worker": old_worker}))
        args = ("handoff", dispatch, "--host", "opencode", "--backend", "bundled")
        assert "without --task or --report" in cli(*args, "--task", task, "--report", report, code=1)
        assert not (root / "opencode-starts").exists()
        handoff_requested = True
        handoff = cli(*args)
        assert (handoff["result"], handoff["round"], handoff["fromAttempt"]) == (
            "accepted", 1, original["attempt"]), "handoff rejected or changed the original round"
        assert handoff["attempt"] != original["attempt"]
        current, = cli("status", "--dispatch", dispatch)["dispatches"]
        launch = current["create"]
        assert launch["ticket"] == handoff["ticket"] and current["backendHandle"] == launch["ticket"]
        assert launch["result"] == "accepted" and launch["round"] == handoff["round"]
        assert launch["ticket"] != old["ticket"] and launch["identity"] != old["identity"]
        launches.append(launch)
        assert cli(*args) == {**handoff, "duplicate": True}
        until(lambda: (root / "opencode-starts").is_file() and (root / "opencode-starts").stat().st_size)
        assert (root / "codex-starts").read_text().splitlines() == [original["attempt"]]
        assert (root / "opencode-starts").read_text().splitlines() == [handoff["attempt"]]
        assert current["scope"] == scope and current["task"] == str(task) and current["report"] == str(report)
        audit = current["audit"][handoff["audit"] - 1]
        assert audit["phase"] == "complete" and audit["priorCreate"] == old
        assert audit["close"]["ticket"] == old["ticket"]
        assert audit["close"]["proof"] == audit["writerStopEvidence"]
        assert audit["writerStopEvidence"]["proves"] == "execution-empty"
        assert not Path(old_group).exists()
        (root / "finish").touch()
        until((root / "receipt.json").is_file)
        receipt = json.loads((root / "receipt.json").read_text())
        received, = cli("wait", "--scope", scope, "--dispatch", dispatch, "--timeout", 3)["messages"]
        assert (received["id"], received["messageId"], received["attempt"]) == (
            receipt["message"], receipt["messageId"], handoff["attempt"])
        assert cli("ack", received["messageId"])["state"] == "processed"
        assert report.read_text() == "original task completed by the replacement\n"
        assert cli(*args) == {**handoff, "duplicate": True}
        close_target(cli, dispatch, handoff, launch)
    finally:
        (root / "finish").touch()
        try:
            if handoff_requested and len(launches) == 1:
                replacement = replacement_for_cleanup(cli, dispatch, scope, old, handoff)
                if replacement is not None:
                    launches.append(replacement)
            cleaned = check_retired(launches)
        except (AssertionError, KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError):
            print("BUNDLED_HANDOFF_CLEANUP_UNVERIFIED status=probe_failed", flush=True)
            cleaned = False
        if handoff_requested and len(launches) != 2:
            print("BUNDLED_HANDOFF_CLEANUP_UNVERIFIED status=replacement_unknown", flush=True)
    assert cleaned and len(launches) == 2, "fixture cgroup or process survived"
    assert (root / "opencode-starts").read_text().splitlines() == [handoff["attempt"]]
    print(f"BUNDLED_CODEX_OPENCODE_HANDOFF_PASS round={handoff['round']} ticket_sha256={digest(handoff['ticket'])}")


if __name__ == "__main__":
    executable = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="wfc-bundled-handoff-") as work:
        exercise(executable, Path(work).resolve())
