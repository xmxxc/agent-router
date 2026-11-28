"""Windows public CLI lifecycle with a controlled substitute for Codex."""
import ctypes as ct
from ctypes import wintypes as wt
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import uuid


ENCLOSING_JOB_EXIT = 69
ENCLOSING_JOB_STDERR = (
    "Error: enclosing Windows Job prevents detachment\n\nCaused by:\n    permission denied\n"
)


class Process:
    api = ct.WinDLL("kernel32", use_last_error=True)
    api.OpenProcess.argtypes = (wt.DWORD, wt.BOOL, wt.DWORD)
    api.OpenProcess.restype = wt.HANDLE
    api.GetProcessTimes.argtypes = (wt.HANDLE,) + (ct.POINTER(wt.FILETIME),) * 4
    api.WaitForSingleObject.argtypes = (wt.HANDLE, wt.DWORD)
    api.WaitForSingleObject.restype = wt.DWORD
    api.TerminateProcess.argtypes = (wt.HANDLE, wt.UINT)
    api.CloseHandle.argtypes = (wt.HANDLE,)

    def __init__(self, pid, creation=None, own_runtime=False):
        self.handle = self.api.OpenProcess(0x101000 | int(own_runtime), False, pid)
        if not self.handle:
            raise ct.WinError(ct.get_last_error())
        try:
            times = [wt.FILETIME() for _ in range(4)]
            if not self.api.GetProcessTimes(self.handle, *(ct.byref(t) for t in times)):
                raise ct.WinError(ct.get_last_error())
            actual = str((times[0].dwHighDateTime << 32) | times[0].dwLowDateTime)
            assert creation is None or creation == actual, (pid, creation, actual)
        except BaseException:
            self.close()
            raise

    def exited(self, timeout=0):
        result = self.api.WaitForSingleObject(self.handle, timeout)
        if result not in (0, 258):
            raise ct.WinError(ct.get_last_error())
        return result == 0

    def close(self):
        self.api.CloseHandle(self.handle)


def until(read, limit=10):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        result = read()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError(f"condition did not become true in {limit}s")


def queue_outcomes(cli, root, host, task, decoy):
    state = root / "queue state"
    calls = root / "queue outcomes.jsonl"
    opts = dict(WFC_STATE=str(state), WFC_FIXTURE_QUEUE=str(calls))

    def publish(scope):
        assignment = cli("dispatch", "--scope", scope, "--task", task, "--report", task, "--cwd", task.parent,
                         "--role", "队列𐐷", "--", host / "codex.exe", "raw", changes=opts)
        receipt = cli("done", "--attempt", assignment["attempt"], "--report", task,
                      changes=dict(opts, WFC_DISPATCH=assignment["dispatch"]))
        return assignment["dispatch"], receipt["messageId"]

    def relay(scope, **changes):
        result = cli("relay", "--codex", "--scope", scope, "--for", "1.2",
                     check=False, changes=dict(opts, **changes))
        assert result.returncode == 0, result
        assert "fixture-private-receipt" not in result.stderr, result.stderr
        return result.stderr

    def receipt_status(dispatch, message_id, state_name):
        row, = cli("status", "--dispatch", dispatch, changes=opts)["dispatches"]
        receipt, = row["messages"]
        assert (receipt["messageId"], receipt["state"]) == (message_id, state_name), receipt
        return receipt

    dispatch, message_id = publish("retry")
    empty_path = root / "empty path"
    empty_path.mkdir()
    assert "os error 2" in relay("retry", PATH=str(empty_path))
    retry = receipt_status(dispatch, message_id, "unsent")
    assert retry["statusVersion"] > 1, retry
    assert not calls.exists() and not decoy.exists()
    assert not relay("retry")
    receipt_status(dispatch, message_id, "received")
    assert cli("ack", message_id, changes=opts)["messageId"] == message_id

    expected = [message_id]
    for mode, diagnostic in [("utf8", "os error 5"), ("invalid", "not UTF-8")]:
        dispatch, message_id = publish(mode)
        result = relay(mode, WFC_FIXTURE_QUEUE_MODE=mode)
        assert "unconfirmed" in result and diagnostic in result and message_id not in result, result
        receipt_status(dispatch, message_id, "unknown")
        assert not relay(mode)
        receipt_status(dispatch, message_id, "unknown")
        assert cli("ack", message_id, check=False, changes=opts).returncode != 0
        expected.append(message_id)

    deliveries = calls.read_text(encoding="utf-8").splitlines()
    assert len(deliveries) == len(expected)
    for delivery, expected_id in zip(deliveries, expected):
        assert expected_id in delivery and "队列𐐷" in delivery
    assert not decoy.exists()
    assert not list(state.glob("wfc-queue-*"))
    assert not cli("status", changes=opts)["waiters"]
    print("Windows queue fixture: PATH, Unicode, exact ACK, unsent retry, unknown without replay")


def main(candidate, root, queue_only=False):
    root = Path(os.path.abspath(root)) / f"wfc native {uuid.uuid4().hex[:8]}"
    host = root / "host tools"
    host.mkdir(parents=True)
    binary = host / "wfc.exe"
    shutil.copyfile(candidate, binary)
    path_bin = root / "path 工具"
    path_bin.mkdir()
    source = Path(__file__).with_name("fixtures") / "windows_codex.rs"
    for destination, flags in [(host, ["--cfg", "fixture_decoy"]), (path_bin, [])]:
        subprocess.run(["rustc", "--edition", "2021", "--crate-name", "codex", *flags, source,
                        "-o", destination / "codex.exe"], check=True, timeout=60)
    thread = str(uuid.uuid4())
    home = root / "codex home"
    sessions = home / "sessions"
    sessions.mkdir(parents=True)
    (sessions / f"rollout-fixture-{thread}.jsonl").write_text(
        json.dumps({"type": "session_meta", "payload": {"id": thread, "source": "vscode"}})
        + "\n", encoding="utf-8")
    state = root / "router state"
    queue = root / "queue.jsonl"
    decoy = root / "wrong codex.txt"
    cwd = root / "worker space"
    cwd.mkdir()
    task = cwd / "real task.md"
    task.write_text("fixture task", encoding="utf-8")
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith("WFC_")}
    env.update(CODEX_THREAD_ID=thread, CODEX_HOME=str(home), WFC_STATE=str(state),
               WFC_FIXTURE_QUEUE=str(queue), WFC_FIXTURE_DECOY=str(decoy),
               WFC_FIXTURE_EXPECTED_CODEX=str(path_bin / "codex.exe"), PATH=os.pathsep.join(
                   (str(path_bin), str(Path(os.environ["SystemRoot"]) / "System32"))))
    notes = []
    processes = []
    relay = None

    def cli(*args, check=True, changes=None):
        output = subprocess.run([binary, *args], env=dict(env, **(changes or {})), cwd=cwd, capture_output=True,
                                text=True, encoding="utf-8", timeout=25)
        if check:
            assert output.returncode == 0, (args, output.stderr, output.stdout)
            return json.loads(output.stdout)
        return output

    def dispatch(name, scope, mode, program=None, check=True):
        report = cwd / f"{name} report.md"
        cfg = cwd / f"{name} launch.json"
        cfg.write_text(json.dumps({"hosts": {"codex": {"command": [
            str(program or host / "codex.exe"), "worker", mode]}}}), encoding="utf-8")
        output = cli("dispatch", "--host", "codex", "--scope", scope,
                     "--cwd", cwd, "--task", task, "--report", report,
                     "--launch-config", cfg, check=check)
        if check:
            assert output["backend"] == "bundled", output
            notes.append(output["dispatch"])
        return output, report

    def row(dispatch_id):
        return cli("status", "--dispatch", dispatch_id)["dispatches"][0]

    def pin(identity, own_runtime=False):
        proc = Process(identity["pid"], identity["creation"], own_runtime)
        processes.append(proc)
        return proc

    queue_outcomes(cli, root, host, task, decoy)
    if queue_only:
        return

    try:
        detach_args = ("relay", "--codex", "--scope", "original", "--detach", "--for", "18")
        detach_result = cli(*detach_args, check=False)
        if (detach_result.returncode == 1 and detach_result.stderr == ENCLOSING_JOB_STDERR
                and detach_result.stdout == ""):
            sys.stderr.write(detach_result.stderr)
            # This diagnostic follows verified staging-process exit before relay activation.
            status = cli("status")
            if (status["waiters"] != [] or status["dispatches"] != []
                    or queue.exists() or (state / "windows-bundled").exists()):
                raise AssertionError("Unexpected lifecycle activity after refused relay detachment")
            print(json.dumps({
                "status": "enclosing-windows-job",
                "command": ["wfc.exe", *detach_args],
                "exit_code": detach_result.returncode,
                "stderr": detach_result.stderr,
                "stdout": detach_result.stdout,
                "waiters": status["waiters"],
                "dispatches": status["dispatches"],
                "queue_fixture": "passed",
                "detached_lifecycle": "not_run",
            }))
            return ENCLOSING_JOB_EXIT
        if detach_result.returncode != 0:
            raise AssertionError((
                detach_args, detach_result.returncode, detach_result.stderr, detach_result.stdout,
            ))
        relay = json.loads(detach_result.stdout)
        relay_proc = pin(relay["process"], own_runtime=True)
        assert cli("status")["waiters"][0]["process"] == relay["process"]
        primary, report = dispatch("first", "original", "exit")
        other, other_report = dispatch("second", "unrelated", "hold")
        identity = primary["create"]["identity"]
        runtime = pin(identity["runtime"], own_runtime=True)
        kernel = pin(identity["kernel"])
        worker = pin(identity["worker"])
        leaf_pid = until(lambda: report.with_suffix(".pid").read_text()
                         if report.with_suffix(".pid").exists() else None)
        leaf = Process(int(leaf_pid))
        processes.append(leaf)
        assert not leaf.exited()
        assert worker.exited(10000)
        actual_cwd = report.read_text(encoding="utf-8").removeprefix("window fixture ran in ")
        assert Path(actual_cwd).samefile(cwd), actual_cwd
        until(lambda: other_report.exists())
        completed = row(primary["dispatch"])
        assert completed["worker"]["state"] == "exited", completed
        assert completed["backendAlive"] is True, completed
        assert len(completed["messages"]) == 1, completed
        msg = completed["messages"][0]["messageId"]
        until(lambda: queue.exists() and msg in queue.read_text(encoding="utf-8"))
        until(lambda: row(primary["dispatch"])["messages"][0]["state"] == "received")
        ack = cli("ack", msg)
        assert ack["messageId"] == msg, ack
        seen = row(primary["dispatch"])["messages"]
        assert [m["messageId"] for m in seen] == [msg] and seen[0]["state"] == "processed", seen
        isolated = until(lambda: row(other["dispatch"])["messages"])
        assert isolated[0]["state"] == "unsent" and isolated[0]["messageId"] != msg
        closed = cli("close", primary["dispatch"])
        assert closed["result"] == "completed" and closed["jobActiveProcesses"] == 0, closed
        assert closed["identity"] == identity, closed
        assert leaf.exited() and kernel.exited(), closed
        assert runtime.exited(5000)
        assert cli("close", primary["dispatch"]) == closed
        assert row(other["dispatch"])["backendAlive"] is True
        assert cli("close", other["dispatch"])["jobActiveProcesses"] == 0
        fast, _ = dispatch("fast exit", "fast", "fast")
        fast_exit = until(lambda: row(fast["dispatch"])["worker"]
                          if row(fast["dispatch"])["backendAlive"] is False else None)
        assert fast_exit["state"] == "exited" and fast_exit["exit_code"] == 259, fast_exit
        lost, lost_report = dispatch("lost runtime", "lost", "hold")
        identity_lost = lost["create"]["identity"]
        lost_runtime = pin(identity_lost["runtime"], own_runtime=True)
        lost_kernel = pin(identity_lost["kernel"])
        lost_worker = pin(identity_lost["worker"])
        lost_leaf_pid = until(lambda: lost_report.with_suffix(".pid").read_text()
                              if lost_report.with_suffix(".pid").exists() else None)
        lost_leaf = Process(int(lost_leaf_pid))
        processes.append(lost_leaf)
        assert lost_runtime.api.TerminateProcess(lost_runtime.handle, 1)
        assert lost_runtime.exited(5000) and lost_worker.exited(5000)
        assert lost_kernel.exited(5000) and lost_leaf.exited(5000)
        still_unknown = row(lost["dispatch"])
        assert still_unknown["backendAlive"] is None and still_unknown["backendResult"] == "uncertain"
        lost_close = cli("close", lost["dispatch"])
        assert lost_close["result"] == "uncertain" and lost_close["closePending"] is True
        assert all(lost_close[key] == lost[key] for key in ("dispatch", "round", "attempt"))
        lost_status = row(lost["dispatch"])
        assert lost_status["closePending"] is True and lost_status["backendAlive"] is None
        assert lost_status["backendObservation"]["diagnostic"], lost_status
        ready = state / "windows-bundled" / lost["windowsExecution"] / "ready.json"
        ready.rename(ready.with_suffix(".removed"))
        missing_ready = row(lost["dispatch"])
        assert missing_ready["closePending"] is True and missing_ready["backendAlive"] is None
        assert "missing Windows launch evidence" in missing_ready["backendObservation"]["diagnostic"]
        missing_ready_close = cli("close", lost["dispatch"])
        assert missing_ready_close["result"] == "uncertain" and missing_ready_close["closePending"] is True
        assert cli("close", lost["dispatch"]) == missing_ready_close
        notes.remove(lost["dispatch"])
        bad, _ = dispatch("missing", "broken", "exit", host / "missing.exe", check=False)
        assert bad.returncode != 0 and "definitely-not-sent" in bad.stderr and "os error 2" in bad.stderr, bad
        bad_row = next(item for item in cli("status")["dispatches"] if item["scope"] == "broken")
        assert bad_row["worker"]["state"] == "not_started" and bad_row["backendAlive"] is False
        assert bad_row["backendObservation"]["category"] == "executable_missing", bad_row
        assert relay_proc.exited(22000)
        assert not cli("status")["waiters"]
        deliveries = queue.read_text(encoding="utf-8").splitlines()
        assert len(deliveries) == 1 and msg in deliveries[0], deliveries
        print(f"Windows dispatch {primary['dispatch']}, exact identities {identity}, message {msg}, Job zero")
    finally:
        for note in notes:
            cli("close", note, check=False)
        if relay is not None:
            # Let the relay finish its bounded registration on a failed check.
            with_handle = Process(relay["pid"], relay["process"]["creation"], True) if not processes else processes[0]
            if not with_handle.exited(22000):
                with_handle.api.TerminateProcess(with_handle.handle, 1)
                assert with_handle.exited(5000)
            if not processes:
                with_handle.close()
        for process in processes:
            process.close()


if __name__ == "__main__":
    args = [arg for arg in sys.argv[1:] if arg != "--queue-only"]
    sys.exit(main(Path(args[0]).resolve(), Path(args[1] if len(args) > 1 else "target"),
                  queue_only="--queue-only" in sys.argv[1:]))
