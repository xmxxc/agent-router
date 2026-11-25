#!/usr/bin/env python3
"""Check startup decisions using only this fixture's public dispatches."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


WORKER = """#!/bin/sh
set -eu
printf '%s\n' "$$" > worker-started
for i in $(seq 1200); do
    if [ -f try-done ] && [ ! -f tried-done ]; then
        printf 'fixture callback\n' > "$WFC_REPORT"
        if "$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" \\
            --message-id fixture-before-ready > callback-out 2> callback-err; then
            echo accepted > tried-done
        else
            echo rejected > tried-done
        fi
    fi
    [ ! -f finish ] || exit 0
    sleep .05
done
"""


def until(read, description, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = read()
        if value:
            return value
        time.sleep(.03)
    raise AssertionError(description)


def same_process(identity):
    pid = identity["pid"]
    try:
        state, *stat = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    except FileNotFoundError:
        return False
    return state not in ("Z", "X") and int(stat[18]) == identity["start_ticks"]


def exercise(binary, root, manager_env):
    home = root / "home"
    home.mkdir()
    state = root / "state"
    host = root / "host"
    host.write_text(WORKER)
    config = root / "launch.json"
    config.write_text(json.dumps({"hosts":{"codex":{"command":["/bin/sh", str(host)]}}}))
    base_env = {"PATH":"/usr/bin:/bin", "HOME":str(home), "WFC_STATE":str(state),
                "WFC_LAUNCH_CONFIG":str(config)}

    def cli(*args, env=base_env, code=0):
        result = subprocess.run([binary, *args], env=env, capture_output=True, text=True, timeout=10)
        assert result.returncode == code, (args, result.returncode, result.stderr)
        return json.loads(result.stdout)

    def owner(scope):
        return {**base_env, "WFC_CONTROLLER_TOKEN": cli("acquire", "--scope", scope, "--lease", "90")["token"]}

    def row(scope):
        records = [r for r in cli("status")["dispatches"] if r["scope"] == scope]
        assert len(records) <= 1, records
        return records[0] if records else {}

    def dispatch(scope, output, env):
        work = root / scope
        work.mkdir()
        (work / "task").write_text("controlled bundled startup\n")
        return subprocess.Popen(
            [binary, "dispatch", "--host", "codex", "--cwd", work, "--task", work / "task",
             "--report", work / "report", "--scope", scope],
            env=env, stdout=output, stderr=subprocess.DEVNULL,
        )

    def close(scope, env):
        current = row(scope)
        if current:
            return cli("close", current["dispatch"], env=env)
        return {}

    neighbor_scope = "startup-committed"
    subject_scope = "startup-interrupted"
    neighbor_env = owner(neighbor_scope)
    subject_env = owner(subject_scope)
    launcher = None
    try:
        # The public result has nowhere to go. The signed commit must still
        # be discoverable after the original CLI exits at its output boundary.
        reader, writer = os.pipe()
        os.close(reader)
        try:
            launcher = dispatch(neighbor_scope, writer, neighbor_env)
        finally:
            os.close(writer)
        assert launcher.wait(timeout=15) != 0
        neighbor = until(lambda: row(neighbor_scope), "missing committed dispatch")
        assert neighbor["create"]["result"] == "accepted", neighbor
        assert neighbor["backendAlive"] is True, neighbor
        assert neighbor["worker"]["state"] == "running", neighbor
        neighbor_ticket = neighbor["backendHandle"]
        print("BUNDLED_STARTUP_COMMITTED_AFTER_OUTPUT_LOSS", neighbor_ticket["unit"])

        # Initial reservation uses the message lock. The existing scope lock
        # is needed only to commit ready, so we can hold it before launch.
        # This fixes the ordering without relying on timing or a debug hook.
        scope_hash = hashlib.sha256(subject_scope.encode()).hexdigest()
        with (state / f"relay-{scope_hash}.lock").open("rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            launcher = dispatch(subject_scope, subprocess.DEVNULL, subject_env)

            def prepared_row():
                current = row(subject_scope)
                return current if current.get("backendObservation", {}).get("startup") == "prepared" else None

            prepared = until(prepared_row, "startup never reached signed preparation")
            ticket = prepared["backendHandle"]
            observation = prepared["backendObservation"]
            identity = observation["startupIdentity"]
            assert prepared["createPending"] is True and prepared["backendAlive"] is None, prepared
            assert ticket["unit"] == prepared["dispatch"] and ticket["attempt"] == prepared["attempt"], ticket
            assert all(same_process(identity[key]) for key in ("guardian", "service", "kernel", "worker"))
            unit = identity["userService"]["unit"]
            assert unit == f"wfc-bundled-{ticket['generation']}.service", unit
            work = root / subject_scope
            (work / "try-done").touch()
            tried = work / "tried-done"
            until(lambda: tried.is_file() and tried.stat().st_size, "provisional callback did not return", 3)
            assert tried.read_text().strip() == "rejected"
            assert "startup has not committed ready" in (work / "callback-err").read_text()
            launcher.terminate()
            assert launcher.wait(timeout=4) == -signal.SIGTERM

        def cancelled_row():
            current = row(subject_scope)
            proof = current.get("backendObservation", {}).get("proof") or {}
            return current if proof.get("state") == "startup-aborted" else None

        cancelled = until(cancelled_row, "missing signed pre-ready cleanup")
        observation = cancelled["backendObservation"]
        assert cancelled["backendHandle"] == ticket and cancelled["backendAlive"] is False, cancelled
        assert observation["cleanup"] == "confirmed" and observation["proof"]["identity"] == identity
        assert "create" not in cancelled or cancelled["create"] is None, cancelled
        named = cli("status", "--dispatch", cancelled["dispatch"])["dispatches"][0]
        assert not [m for m in named.get("messages", []) if m["type"] == "done"]
        first = close(subject_scope, subject_env)
        assert first["result"] == "completed" and first["ticket"] == ticket, first
        assert first["proof"] == close(subject_scope, subject_env)["proof"] == observation["proof"]

        contained = Path(identity["containment"]["path"])
        service_group = contained.parent

        def retired():
            if contained.exists() or service_group.exists():
                return False
            if any(same_process(identity[key]) for key in ("guardian", "service", "kernel", "worker")):
                return False
            state_query = subprocess.run(
                ["/usr/bin/systemctl", "--user", "--no-pager", "show", unit, "--property=LoadState", "--value"],
                env=manager_env, capture_output=True, text=True, timeout=3,
            )
            return state_query.stdout.strip() == "not-found"

        until(retired, "interrupted service, cgroup or process survived")
        survivor = row(neighbor_scope)
        assert survivor["backendHandle"] == neighbor_ticket and survivor["backendAlive"] is True, survivor
        assert same_process(neighbor["create"]["identity"]["worker"])
        neighbor_closed = close(neighbor_scope, neighbor_env)
        assert neighbor_closed["cleanup"] == "confirmed" and neighbor_closed["ticket"] == neighbor_ticket
        print("BUNDLED_STARTUP_INTERRUPTION_PASS", ticket["unit"], ticket["attempt"], unit)
    finally:
        if launcher is not None and launcher.poll() is None:
            launcher.terminate()
            try:
                launcher.wait(timeout=4)
            except subprocess.TimeoutExpired:
                launcher.kill()
                launcher.wait()
        for scope, env in [(subject_scope, subject_env), (neighbor_scope, neighbor_env)]:
            work = root / scope
            if work.exists():
                (work / "finish").touch()
                try:
                    close(scope, env)
                except (AssertionError, OSError, subprocess.SubprocessError) as error:
                    print(f"fixture cleanup for {scope}: {error}", file=sys.stderr)


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    runtime = Path(f"/run/user/{os.geteuid()}")
    if not (runtime / "bus").is_socket():
        print("BUNDLED_STARTUP_INTERRUPTION_UNVERIFIED no current-user manager socket")
        return 77
    manager_env = {"PATH":"/usr/bin:/bin", "XDG_RUNTIME_DIR":str(runtime),
                   "DBUS_SESSION_BUS_ADDRESS":f"unix:path={runtime}/bus"}
    with tempfile.TemporaryDirectory() as work:
        exercise(binary, Path(work), manager_env)
    return 0


if __name__ == "__main__":
    sys.exit(main())
