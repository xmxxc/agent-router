#!/usr/bin/env python3
"""Exercise bundled Codex resume on a real user guardian with a synthetic profile."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from typing import NamedTuple
import uuid

from check_codex import (
    CGROUP, PROCESSES, check, check_assignment, check_ticket, file_digest, manager_env,
    owned_resources, read_fixture, retired, run_command, same_process, ticket_digest, until,
)


CHECKPOINT = b"synthetic source report checkpoint\n"
FINAL = b"synthetic replacement report finished\n"
DONE_KEY = "bundled-resume-fixture-done"


def worker(argv):
    root = Path(os.environ["RESUME_FIXTURE_ROOT"])
    task = Path(os.environ["WFC_TASK"])
    report = Path(os.environ["WFC_REPORT"])
    profile = Path(os.environ["CODEX_HOME"])
    binary = Path(os.environ["WFC_BIN"])
    dispatch = os.environ["WFC_DISPATCH"]
    attempt = os.environ["WFC_ATTEMPT"]
    session = os.environ["RESUME_FIXTURE_SESSION"]
    resumed = argv[0] == "resume"
    check(profile == root / "profile" and Path.cwd() == root / "work"
          and task == root / "work" / "task" and report == root / "work" / "report"
          and binary == Path(os.environ["RESUME_FIXTURE_ELF"])
          and os.environ["WFC_ROUND"] == "1" and all(os.isatty(fd) for fd in (0, 1, 2))
          and not any(os.environ.get(key) for key in ("WFC_CONTROLLER_TOKEN", "WFC_OWNER_TOKEN")),
          "worker environment or PTY mismatch")
    rollout = profile / "sessions" / f"rollout-{session}.jsonl"
    if resumed:
        check(argv[-2] == session and argv.count("resume") == 1 and rollout.is_file()
              and read_fixture(report) == CHECKPOINT, "replacement lost the original context")
    else:
        check(argv[0] == "--no-alt-screen", "unexpected initial Codex invocation")
        with rollout.open("x", encoding="utf-8") as stream:
            for event in (
                {"type": "session_meta", "payload": {"id": session, "cwd": str(Path.cwd()), "source": "cli"}},
                {"type": "event_msg", "payload": {"type": "user_message", "message": argv[-1]}},
            ):
                stream.write(json.dumps(event) + "\n")
        report.write_bytes(CHECKPOINT)
    # One append per process. The parent checks both PTY identities and the
    # exact number of launches without logging the command line or profile.
    with (root / "starts").open("a", encoding="utf-8") as stream:
        stream.write(json.dumps({"resume": resumed, "attempt": attempt, "dispatch": dispatch,
                                 "session": session, "pid": os.getpid()}) + "\n")
    if not resumed:
        until((root / "stop-source").exists, 65, "source exit signal was not received")
        return
    until(lambda: (root / "publish-done").exists() or (root / "finish").exists(),
          65, "replacement completion signal was not received")
    if (root / "finish").exists():
        return
    report.write_bytes(FINAL)
    for name in ("done", "repeat"):
        rc, output = run_command(
            [binary, "done", "--report", report, "--attempt", attempt, "--message-id", DONE_KEY],
            cwd=Path.cwd(), env=os.environ, timeout=10,
        )
        check(rc == 0, "replacement done failed")
        path = root / name
        tmp = path.with_suffix(".tmp")
        tmp.write_text(output, encoding="utf-8")
        tmp.rename(path)
    until((root / "finish").exists, 65, "replacement close did not arrive")


class Execution(NamedTuple):
    ticket: dict
    identity: dict
    group: Path
    unit: str
    unit_group: Path


class Fixture:
    def __init__(self, binary, root, cancelled):
        self.binary = binary
        self.root = root
        self.work = root / "work"
        self.state = root / "state"
        self.task = self.work / "task"
        self.report = self.work / "report"
        self.scope = f"resume-fixture-{uuid.uuid4().hex}"
        self.session = str(uuid.uuid4())
        self.config = root / "launch.json"
        self.cancelled = cancelled
        self.cleaning = False
        self.stage = "prepare"
        self.dispatch = None
        self.known = {}
        self.retired = set()
        self.dispatch_requested = False
        for path in (self.work, root / "home", root / "runtime", root / "profile" / "sessions"):
            path.mkdir(mode=0o700, parents=True)
        self.task.write_text("ordinary first-round fixture task\n", encoding="utf-8")
        self.task_digest = file_digest(self.task)
        self.env = {
            "PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8", "TERM": "xterm-256color",
            "HOME": str(root / "home"), "CODEX_HOME": str(root / "profile"),
            "TMPDIR": str(root / "runtime"), "XDG_RUNTIME_DIR": str(root / "runtime"),
            "WFC_STATE": str(self.state),
        }
        self.config.write_text(json.dumps({"hosts": {"codex": {
            "command": ["/usr/bin/python3", "-B", str(Path(__file__).resolve()), "--worker"],
            "env": {"RESUME_FIXTURE_ROOT": str(root), "RESUME_FIXTURE_ELF": str(binary),
                    "RESUME_FIXTURE_SESSION": self.session},
        }}}), encoding="utf-8")

    def cli(self, *args, timeout=18):
        check(self.cleaning or not self.cancelled(), "fixture interrupted")
        rc, output = run_command([self.binary, *args], cwd=self.work, env=self.env, timeout=timeout,
                                 cancel=None if self.cleaning else self.cancelled)
        check(rc == 0, "public command failed")
        return json.loads(output)

    def wait_until(self, read, seconds, reason):
        def check_state():
            check(self.cleaning or not self.cancelled(), "fixture interrupted")
            return read()

        return until(check_state, seconds, reason)

    def status(self):
        row, = self.cli("status", "--dispatch", self.dispatch)["dispatches"]
        return row

    def remember(self, row, phase=None):
        check_assignment(row, self.scope, self.work, self.task, self.report)
        create = row["create"]
        ticket = create["ticket"]
        check_ticket(row, ticket)
        check(row["dispatch"] == self.dispatch and create["result"] == "accepted"
              and ticket["startupProtocol"] == 1, "managed launch receipt does not match the fixture")
        identity = create["identity"]
        execution = Execution(ticket, identity, *owned_resources(ticket, identity, None))
        # Read-only native lookup checks the signed, current registration.
        # Public close is permitted only after this exact match.
        request = {"op": "lookup", "unit": self.dispatch, "attempt": row["attempt"]}
        lookup = subprocess.run(
            [self.binary, "--bundled", self.state], input=json.dumps(request), text=True,
            cwd=self.work, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=5,
        )
        check(lookup.returncode == 0, "cannot verify native registration")
        registration = json.loads(lookup.stdout)
        check(registration["result"] == "accepted" and registration["ticket"] == ticket
              and registration["phase"] in ("ready", "cleaned")
              and (phase is None or registration["phase"] == phase), "native ticket is not current")
        self.known[ticket["attempt"]] = execution
        return execution

    def receipt_digest(self, execution, filename, kind, data):
        ticket = execution.ticket
        generation = str(uuid.UUID(ticket["generation"]))
        path = self.state / "bundled" / self.dispatch / generation / filename
        contents = read_fixture(path)
        envelope = json.loads(contents)
        check(envelope["body"] == {"version": 1, "ticket": ticket, "kind": kind, "data": data}
              and re.fullmatch(r"[0-9a-f]{64}", envelope["signature"]), "signed receipt shape changed")
        return hashlib.sha256(contents).hexdigest()

    def live(self, execution):
        identity, group = execution.identity, execution.group
        check(all(same_process(identity[name]) for name in PROCESSES), "worker or guardian exited early")
        group_stat = group.stat()
        check((group_stat.st_dev, group_stat.st_ino) ==
              (identity["containment"]["device"], identity["containment"]["inode"]),
              "running cgroup differs from the signed identity")
        for name in ("guardian", "service", "kernel"):
            check(Path(f"/proc/{identity[name]['pid']}/exe").samefile(self.binary), "execution uses another ELF")
        service = subprocess.run(
            ["/usr/bin/systemctl", "--user", "--no-pager", "show", "--property=MainPID",
             "--property=InvocationID", "--property=ControlGroup", "--property=Delegate", "--", execution.unit],
            env=manager_env(), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, timeout=5,
        )
        check(service.returncode == 0, "could not read the signed user unit")
        properties = dict(line.split("=", 1) for line in service.stdout.splitlines() if "=" in line)
        cgroup = f"/{execution.unit_group.relative_to(CGROUP)}"
        guardian_proc = Path(f"/proc/{identity['guardian']['pid']}")
        check(properties["MainPID"] == str(identity["guardian"]["pid"])
              and properties["InvocationID"] == identity["userService"]["invocation"]
              and properties["ControlGroup"] == cgroup and properties["Delegate"] == "yes"
              and f"0::{cgroup}" in (guardian_proc / "cgroup").read_text().splitlines(),
              "user unit differs from the signed guardian")
        worker_proc = Path(f"/proc/{identity['worker']['pid']}")
        check((worker_proc / "cwd").resolve(strict=True) == self.work
              and f"0::/{group.relative_to(CGROUP)}" in (worker_proc / "cgroup").read_text().splitlines(),
              "worker is outside the signed working directory or group")
        return self.receipt_digest(execution, "startup-decision.json", "ready", identity)

    def stopped(self, execution, proof, expected):
        check(proof["proves"] == "execution-empty" and proof["identity"] == execution.identity
              and (expected is None or proof["state"] == expected), "stop proof does not match signed ready")
        digest = self.receipt_digest(execution, "cleanup.json", "cleanup", proof)
        self.wait_until(
            lambda: retired(execution.identity, execution.group, execution.unit, execution.unit_group),
            12, "a signed fixture process or user service still exists",
        )
        self.retired.add(execution.ticket["attempt"])
        return digest

    def observe_retirement(self, row):
        observation = row["backendObservation"]
        if observation.get("cleanup") != "confirmed" or row["backendAlive"] is not False:
            return False
        proof = observation["proof"]
        if proof["state"] not in ("closed", "exited", "faulted"):
            return False
        # The public launch response may have been lost. Reuse the ownership
        # check against the verified stop observation for read-only retirement.
        # This branch must never send close on reconstructed launch evidence.
        observed = {**row, "create": {"result": "accepted", "ticket": observation["ticket"],
                                      "identity": proof["identity"]}}
        execution = self.remember(observed, phase="cleaned")
        self.receipt_digest(execution, "startup-decision.json", "ready", execution.identity)
        self.stopped(execution, proof, None)
        return True

    def close(self, execution, expected=None):
        ticket = execution.ticket

        def confirmed():
            row = self.status()
            check(row["attempt"] == ticket["attempt"] and row["create"]["ticket"] == ticket
                  and row["backendObservation"].get("result") != "refused",
                  "current assignment does not match the cleanup ticket")
            result = self.cli("close", self.dispatch, timeout=12)
            check((result["dispatch"], result["round"], result["attempt"]) ==
                  (self.dispatch, 1, ticket["attempt"]), "close selected another assignment")
            check(result.get("ticket", ticket) == ticket, "close selected another ticket")
            if result["result"] == "uncertain":
                return None
            check(result["operation"] == "close" and result["result"] == "completed"
                  and result["cleanup"] == "confirmed" and result["ticket"] == ticket,
                  "close did not return a confirmed receipt")
            return result["proof"]

        proof = self.wait_until(confirmed, 18, "close receipt did not settle")
        digest = self.stopped(execution, proof, expected)
        # Read back the same close result after the user service is gone.
        check(confirmed() == proof, "close replay changed the signed proof")
        return digest

    def starts(self):
        return [json.loads(row) for row in read_fixture(self.root / "starts").splitlines()]

    def run(self):
        self.stage = "dispatch"
        owner = self.cli("acquire", "--scope", self.scope, "--lease", "180")
        self.env["WFC_CONTROLLER_TOKEN"] = owner["token"]
        self.dispatch_requested = True
        old = self.cli("dispatch", "--host", "codex", "--scope", self.scope, "--cwd", self.work,
                       "--task", self.task, "--report", self.report, "--launch-config", self.config)
        self.dispatch = old["dispatch"]
        source = self.remember(old, phase="ready")
        old_ready = self.live(source)
        rollout = self.root / "profile" / "sessions" / f"rollout-{self.session}.jsonl"
        self.wait_until(lambda: self.starts() if (self.root / "starts").exists() else None,
                        8, "source did not start")
        check(self.starts() == [{"resume": False, "attempt": old["attempt"], "dispatch": self.dispatch,
                                 "session": self.session, "pid": source.identity["worker"]["pid"]}],
              "unexpected source process or attempt")
        check(read_fixture(self.report) == CHECKPOINT, "missing source report checkpoint")
        rollout_digest = file_digest(rollout)

        self.stage = "source-stop"
        (self.root / "stop-source").touch()

        def source_proof():
            current = self.status()
            observation = current["backendObservation"]
            if observation.get("cleanup") != "confirmed":
                return None
            check(current["backendAlive"] is False and observation["ticket"] == source.ticket
                  and current["attempt"] == old["attempt"] and current["state"] != "done"
                  and not any(message["type"] == "done" for message in current.get("messages", [])),
                  "source must stop without a completed round")
            return observation["proof"]

        proof = self.wait_until(source_proof, 18, "original stop receipt was not confirmed")
        old_cleanup = self.stopped(source, proof, "exited")
        print("BUNDLED_RESUME_SOURCE_STOPPED", ticket_digest(source.ticket), old_ready, old_cleanup, flush=True)

        self.stage = "resume"
        resumed = self.cli("resume", self.dispatch)
        check(resumed["result"] == "accepted" and resumed["round"] == 1
              and resumed["resumed"] == self.dispatch and resumed["session"] == self.session
              and resumed["attempt"] != old["attempt"], "public resume returned the wrong attempt or session")
        status = self.status()
        target = self.remember(status, phase="ready")
        target_ready = self.live(target)
        check(resumed["ticket"] == target.ticket and target.ticket != source.ticket
              and target.identity == resumed["identity"] and target.unit != source.unit
              and status["task"] == old["task"] and status["report"] == old["report"],
              "replacement does not match the public resume receipt")

        def check_history(current):
            audit, = [event for event in current["audit"] if event["operation"] == "resume"]
            check(audit["id"] == resumed["audit"] and audit["result"] == "accepted"
                  and audit["attempt"] == target.ticket["attempt"] and audit["fromAttempt"] == old["attempt"]
                  and audit["ownerGeneration"] == owner["generation"]
                  and audit["priorBackendHandle"] == source.ticket and audit["priorCreate"] == old["create"]
                  and audit["priorInspection"]["proof"] == proof
                  and current["rounds"][0]["attempt"] == old["attempt"]
                  and current["rounds"][0]["report"] == old["report"], "original lifecycle history changed")

        check_history(status)
        duplicate = self.cli("resume", self.dispatch)
        check(duplicate["duplicate"] is True and duplicate["result"] == "accepted"
              and all(duplicate[key] == resumed[key] for key in ("dispatch", "round", "attempt", "audit", "session")),
              "resume did not replay its original audit")
        self.wait_until(lambda: len(self.starts()) >= 2, 8, "replacement did not enter its PTY")
        expected_start = {"resume": True, "attempt": resumed["attempt"], "dispatch": self.dispatch,
                          "session": self.session, "pid": target.identity["worker"]["pid"]}
        check(len(self.starts()) == 2 and self.starts()[1] == expected_start
              and self.status()["backendHandle"] == target.ticket and read_fixture(self.report) == CHECKPOINT,
              "replacement launched twice or lost the report checkpoint")
        print("BUNDLED_RESUME_REPLACEMENT_ONCE", ticket_digest(target.ticket), target_ready, flush=True)

        self.stage = "done-ack"
        (self.root / "publish-done").touch()
        msg, = self.cli("wait", "--scope", self.scope, "--dispatch", self.dispatch, "--timeout", "12")["messages"]
        self.wait_until((self.root / "repeat").is_file, 8, "worker did not publish its repeated done")
        done = json.loads(read_fixture(self.root / "done"))
        repeat = json.loads(read_fixture(self.root / "repeat"))
        message_id = msg["messageId"]
        check(msg["type"] == "done" and msg["state"] == "received" and msg["attempt"] == resumed["attempt"]
              and msg["dispatch"] == self.dispatch and msg["round"] == 1 and msg["scope"] == self.scope
              and msg["report"] == str(self.report) and re.fullmatch(r"m-[0-9a-f]{32}", message_id)
              and (done["message"], done["messageId"], done["duplicate"]) == (msg["id"], message_id, False)
              and (repeat["message"], repeat["messageId"], repeat["duplicate"]) == (msg["id"], message_id, True),
              "replacement done did not preserve a single original receipt")
        ack = self.cli("ack", message_id)
        check((ack["message"], ack["messageId"], ack["state"], ack["statusVersion"]) ==
              (msg["id"], message_id, "processed", msg["statusVersion"] + 1), "ACK changed the done receipt")
        current = self.status()
        saved, = current["messages"]
        check((saved["id"], saved["messageId"], saved["attempt"], saved["state"]) ==
              (msg["id"], message_id, resumed["attempt"], "processed")
              and current["rounds"][-1]["result"]["messageId"] == message_id
              and current["backendHandle"] == target.ticket and current["backendAlive"] is True
              and read_fixture(self.report) == FINAL, "replacement result was not saved on its attempt")
        check_history(current)
        print("BUNDLED_RESUME_DONE_ACK", msg["id"], ack["statusVersion"], flush=True)

        self.stage = "replacement-close"
        target_cleanup = self.close(target, "closed")
        current = self.status()
        check(current["backendHandle"] == target.ticket and current["backendAlive"] is False
              and current["backendObservation"]["cleanup"] == "confirmed"
              and current["messages"] == [saved] and len(self.starts()) == 2
              and file_digest(rollout) == rollout_digest and file_digest(self.task) == self.task_digest
              and self.receipt_digest(source, "startup-decision.json", "ready", source.identity) == old_ready
              and self.receipt_digest(source, "cleanup.json", "cleanup", proof) == old_cleanup
              and self.receipt_digest(target, "startup-decision.json", "ready", target.identity) == target_ready,
              "close altered a receipt, source record, or launch count")
        check_history(current)
        print("BUNDLED_RESUME_REPLACEMENT_CLOSED", ticket_digest(target.ticket), target_cleanup, flush=True)

    def cleanup(self):
        self.cleaning = True
        if not self.dispatch_requested:
            return
        (self.root / "stop-source").touch()
        (self.root / "finish").touch()

        def retire_current():
            rows = self.cli("status")["dispatches"]
            if not rows:
                return not self.known and not (self.state / "bundled").exists()
            row, = rows
            self.dispatch = self.dispatch or row["dispatch"]
            if (row.get("create") or {}).get("result") != "accepted":
                return self.observe_retirement(row)
            try:
                current = self.remember(row)
            except (KeyError, OSError, RuntimeError, subprocess.SubprocessError, ValueError):
                return False
            if current.ticket["attempt"] not in self.retired:
                self.close(current)
            return True

        # A timed out CLI may leave a user service briefly live. The worker
        # has its own exit signal and deadline. Wait for exact signed binding
        # before any public close, and leave the private state if it is unknown.
        self.wait_until(retire_current, 18, "cleanup could not verify the private current ticket")
        check(set(self.known) <= self.retired, "a known fixture execution was not retired")


def main():
    if sys.argv[1:2] == ["--worker"]:
        try:
            worker(sys.argv[2:])
        except Exception:
            return 1
        return 0
    fixture = None
    root = None
    succeeded = False
    cleanup = False
    stage = "prepare"
    interrupted = False

    def stop(_signum, _frame):
        nonlocal interrupted
        interrupted = True

    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, stop)
    try:
        binary = Path(sys.argv[1]).resolve(strict=True)
        check(os.geteuid() != 0 and "WFC_BUNDLED_CGROUP_ROOT" not in os.environ, "requires default user manager")
        stage = "manager"
        runtime = Path(manager_env()["XDG_RUNTIME_DIR"])
        check((runtime / "bus").is_socket(), "current user systemd bus missing")
        manager = subprocess.run(
            ["/usr/bin/systemctl", "--user", "--no-pager", "show", "--property=Version", "--value"],
            env=manager_env(), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            timeout=5,
        )
        check(manager.returncode == 0 and manager.stdout.strip(), "user manager is unavailable")
        stage = "prepare"
        root = Path(tempfile.mkdtemp(prefix="wfc-bundled-resume-")).resolve()
        fixture = Fixture(binary, root, lambda: interrupted)
        fixture.run()
        succeeded = True
    except Exception:
        print(f"BUNDLED_RESUME_FIXTURE_FAILED {fixture.stage if fixture else stage}", flush=True)
    finally:
        try:
            if fixture is not None:
                fixture.cleanup()
            if root is not None:
                shutil.rmtree(root)
            cleanup = True
        except Exception:
            print("BUNDLED_RESUME_CLEANUP_UNCONFIRMED", flush=True)
    if succeeded and cleanup and not interrupted:
        print("BUNDLED_RESUME_PUBLIC_PASS", flush=True)
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
