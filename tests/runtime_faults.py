"""Public receipt fixtures with the same signed binding used by bundled Linux.

The fixture writer holds the rollout open. No model, account, sockets or cgroup
delegation are needed to exercise the controller's read and publication path.
"""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import hmac
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from datetime import datetime, timezone

BINARY = Path(sys.argv.pop(1)).resolve()


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def now():
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def process_identity(pid):
    start = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[19]
    return {"pid": pid, "start_ticks": int(start),
            "boot": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}


class RuntimeFaults(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="wfc-runtime-faults-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.state = self.root / "state"
        self.home = self.root / "codex"
        self.dispatch = "d-00000014"
        self.attempt = "a-000000000014"
        self.session = str(uuid.uuid4())
        self.scope = "runtime-fixture"
        self.brief = "controlled opening"
        self.record = self.state / "dispatches" / (self.dispatch + ".json")
        self.record.parent.mkdir(parents=True)
        unit = self.state / "bundled" / self.dispatch
        unit.parent.mkdir(mode=0o700)
        unit.mkdir(mode=0o700)
        (unit / "lock").touch(mode=0o600)
        generation = str(uuid.uuid4())
        run = unit / generation
        run.mkdir(mode=0o700)
        self.key = "fixture-signing-key"
        self.ticket = {"version": 2, "unit": self.dispatch, "attempt": self.attempt,
                       "generation": generation, "nonce": str(uuid.uuid4()),
                       "capabilityHash": hashlib.sha256(self.key.encode()).hexdigest()}
        me = process_identity(os.getpid())
        index = {"ticket": self.ticket, "applied": False, "creator": me,
                 "pending_until_ms": int(time.time() * 1000) + 12000}
        index["indexSignature"] = self.sign(["current", index])
        self.private_json(unit / "current.json", index)
        self.private_json(run / "index.json", index)
        self.private_json(run / "key.json", self.key)
        self.ready_file = run / "ready.json"
        self.ready = {"worker": me, "service": me, "kernel": me, "guardian": me,
                      "containment": {"kind": "cgroup-v2", "path": str(run / "fixture-group"),
                                      "device": 1, "inode": 1},
                      "kernel_generation": str(uuid.uuid4()), "endpoint": 9}
        self.sign_ready()
        self.row = {"dispatch": self.dispatch, "attempt": self.attempt, "round": 1,
                    "scope": self.scope, "role": "worker", "host": "codex", "backend": "bundled",
                    "cwd": str(self.root), "task": str(self.root / "task"),
                    "report": str(self.root / "report"), "created": time.time() - .1,
                    "worker": {"state": "running"}, "codexHome": str(self.home),
                    "codexBriefHash": hashlib.sha256(self.brief.encode()).hexdigest(),
                    "backendHandle": self.ticket, "create": {"result": "accepted", "ticket": self.ticket,
                      "dispatch": self.dispatch, "attempt": self.attempt, "round": 1}}
        self.write_row()
        self.log = self.home / "sessions" / "2026" / "11" / "26" / f"rollout-test-{self.session}.jsonl"
        self.log.parent.mkdir(parents=True)
        self.writer = self.log.open("a")
        self.addCleanup(self.writer.close)
        self.append("session_meta", {"source": "cli", "originator": "codex-tui",
                                    "id": self.session, "cwd": str(self.root)})
        self.append("response_item", {"type": "message", "role": "user",
                                      "content": [{"text": self.brief, "type": "input_text"}]})
        self.env = {"PATH": os.environ["PATH"], "HOME": str(self.root),
                    "WFC_STATE": str(self.state), "TERM": "xterm"}

    def sign(self, data):
        return hmac.new(self.key.encode(), encode(data), "sha256").hexdigest()

    def private_json(self, path, data):
        path.write_bytes(encode(data))
        path.chmod(0o600)

    def sign_ready(self):
        body = {"version": 1, "kind": "ready", "ticket": self.ticket, "data": self.ready}
        self.private_json(self.ready_file, {"body": body, "signature": self.sign(body)})

    def sign_cleanup(self, state):
        data = {"proves": "execution-empty", "state": state, "identity": self.ready}
        body = {"version": 1, "kind": "cleanup", "ticket": self.ticket, "data": data}
        self.private_json(self.ready_file.with_name("cleanup.json"),
                          {"body": body, "signature": self.sign(body)})

    def write_row(self):
        self.private_json(self.record, self.row)

    def append(self, kind, payload):
        self.writer.write(json.dumps({"timestamp": now(), "type": kind, "payload": payload}) + "\n")
        self.writer.flush()

    def turn(self, event, turn=None, error=None):
        turn = turn or str(uuid.uuid4())
        self.append("event_msg", {"type": event, "turn_id": turn, "error": error})
        return turn

    def finish(self, info, turn=None):
        return self.turn("task_complete", turn or self.turn("task_started"),
                         {"codex_error_info": info, "message": "private-token https://private.invalid/secret"})

    def complete(self, turn=None):
        return self.turn("task_complete", turn or self.turn("task_started"))

    def progress(self, turn):
        self.append("event_msg", {"type": "item_completed", "turn_id": turn,
                                  "item": {"type": "AgentMessage", "id": "controlled",
                                           "content": [{"type": "Text", "text": "progress"}]}})

    def cli(self, *args, code=0, env=None):
        argv = [BINARY, *args]
        result = subprocess.run(argv, cwd=self.root, env={**self.env, **(env or {})},
                                capture_output=True, text=True, timeout=8)
        self.assertEqual(code, result.returncode, (args, result.stdout, result.stderr))
        return json.loads(result.stdout) if result.stdout else None

    def wait(self, code=0, env=None, timeout="0"):
        return self.cli("wait", "--dispatch", self.dispatch, "--scope", self.scope,
                        "--timeout", timeout, code=code, env=env)["messages"]

    def test_one_ordinary_429_keeps_the_host_classification(self):
        self.finish({"response_too_many_failed_attempts": {"http_status_code": 429}})
        fault, = self.wait()
        self.assertEqual(fault["fault"]["category"], "host_error")
        self.assertEqual(fault["fault"]["evidence"]["httpStatus"], 429)
        self.assertNotIn("private-token", json.dumps(fault))
        self.assertEqual(self.wait(code=3), [])
        self.cli("ack", str(fault["id"]))
        history = self.cli("status", "--dispatch", self.dispatch)["dispatches"][0]["messages"]
        self.assertEqual([(m["messageId"], m["state"]) for m in history],
                         [(fault["messageId"], "processed")])

    def test_quota_timeout_disconnect_recovery_and_parallel_receipts(self):
        # The deadline exists at round start. A first poll still sees a live round.
        deadline = {"WFC_MAX_TURN_SECONDS": "1"}
        self.row["created"] = time.time() - .1
        self.write_row()
        self.assertEqual(self.wait(code=3, env=deadline), [])
        first = self.finish("usage_limit_exceeded")
        quota, = self.wait(env=deadline)
        self.assertEqual((quota["type"], quota["kind"], quota["fault"]["category"]),
                         ("blocked", "host_error", "quota"))
        self.assertEqual(quota["executionId"], self.attempt)
        self.assertEqual(quota["fault"]["evidence"]["turn"], first)
        self.cli("ack", str(quota["id"]))

        # Successful completion with a matching later start records recovery.
        self.complete()
        remaining = 1.02 - (time.time() - self.row["created"])
        if remaining > 0:
            time.sleep(remaining)
        recovered, overdue = self.wait(env=deadline)
        self.assertEqual(recovered["type"], "recovered")
        self.assertEqual((overdue["kind"], overdue["fault"]["category"]),
                         ("turn_timeout", "timeout"))
        self.assertEqual(overdue["fault"]["evidence"]["timeoutMs"], 1000)
        for message in (recovered, overdue):
            self.cli("ack", str(message["id"]))

        # Same host error during alternating observations still has its old ID.
        self.finish("usage_limit_exceeded", first)
        self.assertEqual(self.wait(code=3, env=deadline), [])
        self.turn("task_started")
        self.assertEqual(self.wait(code=3, env=deadline), [])
        other = self.finish({"response_stream_disconnected": {"http_status_code": None}})
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda _: self.raw_wait(deadline), range(2)))
        self.assertEqual(sorted(code for code, _ in results), [0, 3])
        disconnected, = next(messages for code, messages in results if code == 0)
        self.assertEqual(disconnected["fault"]["category"], "protocol_disconnected")
        self.assertEqual(disconnected["fault"]["evidence"]["turn"], other)
        self.assertEqual(self.wait(code=3, env=deadline), [])
        active = self.cli("status", "--dispatch", self.dispatch)["dispatches"][0]
        self.assertEqual(active["runtimeObservation"]["fault"]["category"], "protocol_disconnected")
        self.assertEqual([m["messageId"] for m in active["messages"]],
                         [m["messageId"] for m in (quota, recovered, overdue, disconnected)])
        self.assertEqual([m["state"] for m in active["messages"]],
                         ["processed", "processed", "processed", "received"])
        self.complete()
        new, = self.wait(env=deadline)
        self.assertEqual(new["type"], "recovered")
        self.assertEqual(self.wait(code=3, env=deadline), [])

    def raw_wait(self, env):
        result = subprocess.run([BINARY, "wait", "--scope", self.scope, "--dispatch", self.dispatch,
                                 "--timeout", ".25"], cwd=self.root, env={**self.env, **env},
                                capture_output=True, text=True, timeout=4)
        return result.returncode, json.loads(result.stdout)["messages"]

    def test_deadline_can_expire_while_the_same_fault_stays_visible(self):
        settings = {"WFC_MAX_TURN_SECONDS": ".35"}
        self.row["created"] = time.time()
        self.write_row()
        time.sleep(.01)
        self.finish("usage_limit_exceeded")
        fault, = self.wait(env=settings)
        self.assertEqual(fault["kind"], "host_error")
        self.cli("ack", str(fault["id"]))
        deadline, = self.wait(env=settings, timeout="1")
        self.assertEqual(deadline["kind"], "turn_timeout")
        self.assertEqual(self.wait(code=3, env=settings), [])

    def test_question_and_done_take_priority_over_deadline_or_fault(self):
        callbacks = {"WFC_DISPATCH": self.dispatch}
        self.cli("ask", "May I continue?", "--attempt", self.attempt, env=callbacks)
        question, = self.wait()
        self.assertEqual(question["type"], "question")
        self.finish("usage_limit_exceeded")
        deadline = {"WFC_MAX_TURN_SECONDS": ".001"}
        self.assertEqual(self.wait(code=3, env=deadline), [])
        Path(self.row["report"]).write_text("Complete")
        self.cli("done", "--report", self.row["report"], "--attempt", self.attempt, env=callbacks)
        done, = self.wait(env=deadline)
        self.assertEqual(done["type"], "done")
        row = self.cli("status", "--dispatch", self.dispatch)["dispatches"][0]
        self.assertEqual(row["status"], "completed")
        self.assertNotIn("runtimeObservation", row)

    def test_stall_uses_an_open_turn_and_recovery_requires_progress(self):
        stall = {"WFC_ACTIVITY_AFTER": ".04", "WFC_MAX_TURN_SECONDS": "0"}
        self.append("response_item", {"type": "message", "role": "assistant",
                                      "content": [{"text": "usage_limit_exceeded"}]})
        time.sleep(.07)
        self.assertEqual(self.wait(code=3, env=stall), [])
        turn = self.turn("task_started")
        time.sleep(.07)
        notice, = self.wait(env=stall)
        self.assertEqual((notice["type"], notice["kind"], notice["fault"]["category"]),
                         ("stalled", "stall_timeout", "timeout"))
        self.assertEqual(self.wait(code=3, env=stall), [])
        self.complete(turn)
        recovered, = self.wait(env=stall)
        self.assertEqual(recovered["type"], "recovered")
        self.assertEqual(self.wait(code=3, env=stall), [])

    def test_unattributed_and_unparseable_records_do_not_report_quota(self):
        self.finish("usage_limit_exceeded")
        actual = self.ready["worker"]
        self.ready["worker"] = {**actual, "start_ticks": actual["start_ticks"] + 1}
        self.sign_ready()
        self.assertEqual(self.wait(code=3), [])
        self.ready["worker"] = actual
        self.sign_ready()
        self.row["codexBriefHash"] = "0" * 64
        self.write_row()
        self.assertEqual(self.wait(code=3), [])
        self.row["codexBriefHash"] = hashlib.sha256(self.brief.encode()).hexdigest()
        self.write_row()

        self.finish({"unknown_fixture": {"http_status_code": 429}})
        self.assertEqual(self.wait(code=3), [])
        self.writer.write('{"type":"event_msg","payload":')
        self.writer.flush()
        self.assertEqual(self.wait(code=3), [])
        self.writer.write('null}\n')
        self.writer.flush()
        # A later malformed line invalidates the sample.
        self.writer.write("x" * (600 * 1024) + "\n")
        self.writer.flush()
        self.assertEqual(self.wait(code=3), [])
        self.writer.close()
        self.log.rename(self.log.with_suffix(".archived"))
        # An on-disk log with no matching open writer is still unowned.
        self.log.write_text(self.log.with_suffix(".archived").read_text())
        self.assertEqual(self.wait(code=3), [])

    def test_verified_worker_exit_uses_existing_failure_state(self):
        self.sign_cleanup("exited")
        message, = self.wait()
        self.assertEqual((message["type"], message["fault"]["category"]),
                         ("exited", "process_exited"))
        row = self.cli("status", "--dispatch", self.dispatch)["dispatches"][0]
        self.assertEqual(row["status"], "failed")
        self.assertEqual(self.wait(code=3), [])

    def test_continue_changes_business_round_without_reusing_an_old_fault(self):
        turn = self.finish("usage_limit_exceeded")
        first, = self.wait()
        self.cli("ack", str(first["id"]))
        attempt = "a-000000000015"
        self.row.update({"round": 2, "attempt": attempt, "executionId": self.attempt,
                         "rounds": [{"round": 1, "attempt": self.attempt, "state": "completed",
                                     "created": self.row["created"]},
                                    {"round": 2, "attempt": attempt, "state": "running",
                                     "created": time.time() + .04}]})
        self.write_row()
        self.assertEqual(self.wait(code=3), [])
        time.sleep(.06)
        self.finish("usage_limit_exceeded", turn)
        self.assertEqual(self.wait(code=3), [])
        current = self.finish("usage_limit_exceeded")
        second, = self.wait()
        self.assertEqual(second["fault"]["evidence"]["turn"], current)
        self.assertEqual((second["round"], second["attempt"], second["executionId"]),
                         (2, attempt, self.attempt))
        self.assertNotEqual(second["messageId"], first["messageId"])
        self.assertEqual(self.wait(code=3), [])

    def test_relay_and_wait_share_the_persisted_fault_identity(self):
        controller = str(uuid.uuid4())
        controller_home = self.root / "controller"
        sessions = controller_home / "sessions"
        sessions.mkdir(parents=True)
        (sessions / f"rollout-test-{controller}.jsonl").write_text(
            json.dumps({"type": "session_meta", "payload": {"id": controller, "source": "cli"}}) + "\n")
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        queue = bin_dir / "codex"
        queue.write_text(f"#!{sys.executable}\n" +
                         "import sys\nwith open('queue-calls', 'a') as f:\n    f.write(sys.argv[5] + '\\n')\n")
        queue.chmod(0o700)
        env = {**self.env, "PATH": str(bin_dir), "CODEX_HOME": str(controller_home)}
        self.finish("usage_limit_exceeded")
        for _ in range(2):
            self.cli("relay", "--codex-thread", controller, "--scope", self.scope, "--for", ".8", env=env)
        prefix = '[router] Messages for the controller: '
        envelope, = (self.root / "queue-calls").read_text().splitlines()
        receipt, = json.loads(envelope.removeprefix(prefix))["messages"]
        self.assertEqual(receipt["fault"]["category"], "quota")
        self.assertEqual(self.wait(code=3), [])
        ack = self.cli("ack", str(receipt["id"]))
        self.assertEqual(ack["messageId"], receipt["messageId"])

    def test_distinct_turns_and_alternating_faults_share_one_episode(self):
        first = self.finish("usage_limit_exceeded")
        quota, = self.wait()
        self.cli("ack", quota["messageId"])
        retry = self.turn("task_started")
        self.assertNotEqual(retry, first)
        self.assertEqual(self.wait(code=3), [])
        self.finish("usage_limit_exceeded", retry)
        self.assertEqual(self.wait(code=3), [])

        self.finish({"response_stream_disconnected": {"http_status_code": None}})
        broken, = self.wait()
        self.assertEqual(broken["fault"]["category"], "protocol_disconnected")
        self.finish("usage_limit_exceeded")
        self.assertEqual(self.wait(code=3), [])

        self.progress(self.turn("task_started"))
        next_turn = self.finish("usage_limit_exceeded")
        recovered, new = self.wait()
        self.assertEqual(recovered["kind"], "runtime_recovered")
        self.assertEqual(new["fault"]["evidence"]["turn"], next_turn)
        self.assertNotEqual(new["messageId"], quota["messageId"])
        self.assertEqual(self.wait(code=3), [])

    def test_stall_dedup_survives_retries_until_verified_recovery(self):
        stall = {"WFC_ACTIVITY_AFTER": ".06", "WFC_MAX_TURN_SECONDS": "0"}
        turn = self.turn("task_started")
        time.sleep(.08)
        first, = self.wait(env=stall)
        self.assertEqual(first["kind"], "stall_timeout")
        ack = self.cli("ack", first["messageId"])

        results = []
        for _ in range(2):
            self.turn("turn_aborted", turn)
            turn = self.turn("task_started")
            time.sleep(.08)
            results.append(self.raw_wait(stall))
        self.assertEqual(results, [(3, []), (3, [])])
        row, = self.cli("status", "--dispatch", self.dispatch, env=stall)["dispatches"]
        original, = row["messages"]
        self.assertEqual((original["id"], original["messageId"], original["state"],
                          original["statusVersion"]),
                         (first["id"], first["messageId"], "processed", ack["statusVersion"]))

        self.progress(turn)
        recovered, = self.wait(env={**stall, "WFC_ACTIVITY_AFTER": "10"})
        self.assertEqual(recovered["kind"], "runtime_recovered")
        self.assertEqual(recovered["runtimeEvidence"]["turn"], turn)
        time.sleep(.08)
        second, = self.wait(env=stall)
        self.assertEqual(second["kind"], "stall_timeout")
        self.assertNotEqual(second["messageId"], first["messageId"])
        self.assertEqual(self.wait(code=3, env=stall), [])

    def test_partial_and_unrelated_records_do_not_restart_stall(self):
        stall = {"WFC_ACTIVITY_AFTER": ".06", "WFC_MAX_TURN_SECONDS": "0"}
        turn = self.turn("task_started")
        time.sleep(.08)
        first, = self.wait(env=stall)
        self.cli("ack", first["messageId"])
        fragment = '{"type":"event_msg","payload":'
        self.writer.write(fragment)
        self.writer.flush()
        self.assertEqual(self.wait(code=3, env=stall), [])
        self.writer.write('{"type":"token_count"}}\n')
        self.writer.flush()
        self.append("event_msg", {"type": "item_completed", "turn_id": str(uuid.uuid4()),
                                  "item": {"type": "AgentMessage", "id": "elsewhere", "content": []}})
        time.sleep(.08)
        self.assertEqual(self.wait(code=3, env=stall), [])
        self.progress(turn)
        recovered, = self.wait(env={**stall, "WFC_ACTIVITY_AFTER": "10"})
        self.assertEqual(recovered["kind"], "runtime_recovered")
        time.sleep(.08)
        second, = self.wait(env=stall)
        self.assertEqual(second["kind"], "stall_timeout")
        self.assertGreater(second["runtimeEvidence"]["progressOffset"],
                           first["runtimeEvidence"]["progressOffset"])

    def test_only_completed_command_in_the_active_turn_recovers_stall(self):
        stall = {"WFC_ACTIVITY_AFTER": ".06", "WFC_MAX_TURN_SECONDS": "0"}
        turn = self.turn("task_started")
        time.sleep(.08)
        original, = self.wait(env=stall)
        self.assertEqual(original["kind"], "stall_timeout")
        self.cli("ack", original["messageId"])

        item = {"type": "CommandExecution", "id": "exec-1", "status": "completed",
                "exit_code": 7, "command": ["fixture-command", "", "private-arg"],
                "aggregated_output": "private-output"}
        payload = {"type": "item_completed", "turn_id": turn, "item": item}
        self.append("event_msg", {**payload, "turn_id": str(uuid.uuid4())})
        self.append("event_msg", {**payload, "type": "item_started"})
        self.assertEqual(self.wait(code=3, env=stall), [])
        for invalid in ({"id": ""}, {"status": "in_progress"}, {"status": "failed"},
                        {"exit_code": None}, {"exit_code": "7"}, {"exit_code": 7.5},
                        {"command": []}, {"command": [""]}, {"command": "fixture-command"},
                        {"command": ["fixture-command", 7]}):
            with self.subTest(invalid=invalid):
                self.append("event_msg", {**payload, "item": {**item, **invalid}})
                self.assertEqual(self.wait(code=3, env=stall), [])

        # Even a parseable JSON object is incomplete until the line ends.
        self.writer.write(json.dumps({"timestamp": now(), "type": "event_msg", "payload": payload}))
        self.writer.flush()
        self.assertEqual(self.wait(code=3, env=stall), [])
        self.writer.write("\n")
        self.writer.flush()

        quiet = {**stall, "WFC_ACTIVITY_AFTER": "10"}
        recovered, = self.wait(env=quiet)
        self.assertEqual(recovered["kind"], "runtime_recovered")
        self.assertEqual(recovered["runtimeEvidence"]["turn"], turn)
        self.assertGreater(recovered["runtimeEvidence"]["offset"],
                           original["runtimeEvidence"]["progressOffset"])
        self.assertEqual(self.wait(code=3, env=quiet), [])
        history = self.cli("status", "--dispatch", self.dispatch)["dispatches"][0]["messages"]
        self.assertEqual([(m["messageId"], m["state"]) for m in history],
                         [(original["messageId"], "processed"), (recovered["messageId"], "received")])
        for secret in ("fixture-command", "private-arg", "private-output"):
            self.assertNotIn(secret, json.dumps(history))

    def test_read_only_status_finds_fault_after_more_than_one_poll_window(self):
        for _ in range(240):
            self.append("response_item", {"type": "function_call_output", "call_id": "controlled",
                                          "output": "x" * 5000})
        turn = self.finish("usage_limit_exceeded")
        self.assertGreater(self.log.stat().st_size, 1024 * 1024)

        def snapshot():
            return {p.relative_to(self.state): (p.stat().st_mode, p.read_bytes())
                    for p in self.state.rglob("*") if p.is_file()}

        before = snapshot()
        for _ in range(2):
            row, = self.cli("status", "--dispatch", self.dispatch)["dispatches"]
            self.assertEqual(row["runtimeObservation"]["fault"]["category"], "quota")
            self.assertEqual(row["runtimeObservation"]["fault"]["evidence"]["turn"], turn)
            self.assertEqual(row["messages"], [])
            self.assertEqual(snapshot(), before)

    def test_long_completed_command_recovers_and_preserves_same_turn_terminal(self):
        stall = {"WFC_ACTIVITY_AFTER": ".06", "WFC_MAX_TURN_SECONDS": "0"}
        quiet = {**stall, "WFC_ACTIVITY_AFTER": "10"}
        turn = self.turn("task_started")
        time.sleep(.08)
        first, = self.wait(env=stall)
        self.cli("ack", first["messageId"])

        self.writer.write(json.dumps({
            "timestamp": now(), "type": "event_msg",
            "payload": {"type": "item_completed", "turn_id": turn, "item": {
                "type": "CommandExecution", "id": "exec-long", "status": "completed",
                "exit_code": 0, "command": ["true"],
                "aggregated_output": "private-output" * (1024 * 80),
            }},
        }))
        self.writer.flush()
        self.assertEqual(self.wait(code=3, env=quiet), [])
        self.writer.write("\n")
        self.writer.flush()
        recovered, = self.wait(env=quiet)
        self.assertEqual(recovered["kind"], "runtime_recovered")
        self.assertEqual(recovered["runtimeEvidence"]["turn"], turn)
        self.assertNotIn("private-output", json.dumps(recovered))
        self.assertEqual(self.wait(code=3, env=quiet), [])

        self.finish("usage_limit_exceeded", turn)
        fault, = self.wait(env=quiet)
        self.assertEqual(fault["fault"]["category"], "quota")
        self.assertEqual(fault["fault"]["evidence"]["turn"], turn)
        history = self.cli("status", "--dispatch", self.dispatch)["dispatches"][0]["messages"]
        self.assertEqual([(m["messageId"], m["state"]) for m in history],
                         [(first["messageId"], "processed"),
                          (recovered["messageId"], "received"), (fault["messageId"], "received")])

    def test_read_limits_keep_unobserved_turn_unknown(self):
        stall = {"WFC_ACTIVITY_AFTER": ".06", "WFC_MAX_TURN_SECONDS": "0"}
        turn = self.turn("task_started")
        time.sleep(.08)
        first, = self.wait(env=stall)
        self.cli("ack", first["messageId"])
        self.append("event_msg", {"type": "item_completed", "turn_id": turn, "item": {
            "type": "CommandExecution", "id": "exec-oversize", "status": "completed",
            "exit_code": 0, "command": ["true"], "aggregated_output": "x" * (4 * 1024 * 1024),
        }})
        self.complete(turn)
        for _ in range(2):
            self.assertEqual(self.wait(code=3, env=stall), [])
        row, = self.cli("status", "--dispatch", self.dispatch, env=stall)["dispatches"]
        self.assertEqual(row["runtimeObservation"]["kind"], "runtime_evidence_incomplete")
        self.assertEqual(row["runtimeObservation"]["type"], "unknown")
        self.assertEqual([m["messageId"] for m in row["messages"]], [first["messageId"]])

        # Start a provably new turn beyond the read-only catch-up limit.
        for _ in range(18):
            self.append("response_item", {"type": "function_call_output", "call_id": "controlled",
                                          "output": "x" * (512 * 1024)})
        self.finish("usage_limit_exceeded")
        row, = self.cli("status", "--dispatch", self.dispatch, env=stall)["dispatches"]
        self.assertEqual(row["runtimeObservation"]["kind"], "runtime_evidence_incomplete")
        for _ in range(12):
            code, messages = self.raw_wait(stall)
            if code == 0:
                fault, = messages
                self.assertEqual(fault["fault"]["category"], "quota")
                break
            self.assertEqual((code, messages), (3, []))
        else:
            self.fail("polls did not reach the new turn")
        self.assertEqual(self.wait(code=3, env=stall), [])

    def test_large_rollout_catches_up_across_fresh_wait_processes(self):
        stall = {"WFC_ACTIVITY_AFTER": ".07", "WFC_MAX_TURN_SECONDS": "0"}
        turn = self.turn("task_started")
        self.progress(turn)
        for _ in range(700):
            self.append("response_item", {"type": "function_call_output", "call_id": "controlled",
                                          "output": "x" * 5000})
        time.sleep(.09)
        self.assertGreater(self.log.stat().st_size, 3 * 1024 * 1024)
        self.assertEqual(self.wait(code=3, env=stall), [])
        self.assertEqual(self.wait(code=3, env=stall), [])
        self.assertEqual(self.wait(code=3, env=stall), [])
        stalled, = self.wait(env=stall)
        self.assertEqual(stalled["runtimeEvidence"]["turn"], turn)
        self.assertEqual(stalled["kind"], "stall_timeout")
        self.assertEqual(self.wait(code=3, env=stall), [])

    def test_scope_only_wait_zero_advances_across_fresh_processes(self):
        for i in range(128):
            dispatch = f"d-{i + 1000:08x}"
            self.private_json(self.record.with_name(f"{dispatch}.json"),
                              {"dispatch": dispatch, "scope": self.scope, "host": "opencode"})
        self.finish("usage_limit_exceeded")
        for _ in range(130):
            result = subprocess.run(
                [BINARY, "wait", "--scope", self.scope, "--timeout", "0"], cwd=self.root,
                env=self.env, capture_output=True, text=True, timeout=4)
            if result.returncode == 0:
                fault, = json.loads(result.stdout)["messages"]
                self.assertEqual(fault["dispatch"], self.dispatch)
                self.assertEqual(fault["fault"]["category"], "quota")
                break
            self.assertEqual(result.returncode, 3, result)
        else:
            self.fail("scope scan never visited the signed worker")

    def test_signed_runtime_process_loss_is_distinct_from_worker_exit(self):
        service = subprocess.Popen(["/bin/sleep", "30"])
        try:
            self.ready["service"] = process_identity(service.pid)
            self.sign_ready()
            self.assertEqual(self.wait(code=3), [])
        finally:
            service.terminate()
            service.wait(timeout=2)
        notice, = self.wait()
        self.assertEqual((notice["type"], notice["kind"], notice["fault"]["category"]),
                         ("unknown", "backend_observation", "protocol_disconnected"))
        self.assertEqual(self.wait(code=3), [])


if __name__ == "__main__":
    unittest.main()
