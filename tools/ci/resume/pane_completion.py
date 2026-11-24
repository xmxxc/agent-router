"""Hosted-only evidence for a worker completing each round from its own pane."""
import json
from pathlib import Path
import subprocess
import sys

from herdr_public import PublicLifecycle, live


class PaneCompletion(PublicLifecycle):
    def stop_workers(self):
        workers = []
        for trace in self.handoff_traces.glob("*.json"):
            trace.with_suffix(".quit").touch()
            workers.append(json.loads(trace.read_bytes()))
        live.wait_for(lambda: all(not self.handoff_worker_alive(worker) for worker in workers))

    def complete_in_pane(self, worker):
        self.assertTrue(self.handoff_worker_alive(worker))
        signal = self.handoff_traces / f'{worker["attempt"]}.finish'
        outcome = signal.with_suffix(".done")
        signal.touch()
        done = live.wait_for(lambda: json.loads(outcome.read_bytes()) if outcome.is_file() else None,
                             seconds=8)
        self.assertEqual(done["exitCode"], 0)
        self.assertIsInstance(done["receipt"], dict)
        self.assertTrue(self.handoff_worker_alive(worker))
        return done["receipt"]

    def assert_worker_context(self, worker, expected):
        self.assertEqual(
            [worker[key] for key in ("dispatch", "round", "attempt", "scope", "task", "report")],
            [expected[key] for key in ("dispatch", "round", "attempt", "scope", "task", "report")],
        )
        self.assertEqual(Path(worker["cwd"]).resolve(), self.root.resolve())
        self.assertEqual(Path(f'/proc/{worker["pid"]}/cwd').resolve(strict=True), self.root.resolve())
        self.assertEqual(Path(worker["binary"]).resolve(), Path(live.DRIVER).resolve())

    def test_completed_herdr_to_tmux_handoff_starts_next_round_once(self):
        socket = self.root / "handoff-tmux.sock"
        self.env["WFC_TMUX_SOCKET"] = str(socket)
        tmux = ["tmux", "-f", "/dev/null", "-S", str(socket)]
        subprocess.run(tmux + ["new-session", "-d", "-s", "sibling", "/bin/cat"],
                       env=self.env, check=True, capture_output=True, timeout=5)
        self.addCleanup(subprocess.run, tmux + ["kill-server"], env=self.env,
                        capture_output=True, timeout=5)
        self.handoff_traces = self.root / "handoff-workers"
        self.handoff_traces.mkdir()
        self.addCleanup(self.stop_workers)
        fixture = Path(__file__).with_name("pane_worker.py").resolve(strict=True)
        self.dispatch(command=[sys.executable, "-u", str(fixture)])
        source = self.row
        old = self.handoff_trace(source["attempt"])
        self.assert_worker_context(old, source)
        self.assertEqual(old["herdrPane"], source["backendHandle"]["pane"])
        self.assertIsNone(old["tmuxPane"])
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        original_receipt = Path(source["create"]["receipt"])
        original_started = json.loads(original_receipt.read_bytes())
        self.assertEqual((original_started["category"], original_started["pid"],
                          original_started["writerProcess"]["creation"]),
                         ("started", old["pid"], f'linux:{boot_id}:{old["startTicks"]}'))

        task, report = Path(source["task"]), Path(source["report"])
        task_before = task.read_bytes()
        self.assertFalse(report.exists())
        done = self.complete_in_pane(old)
        report_before = report.read_bytes()
        self.assertEqual(report_before, b"Round 1 completed by its pane worker.\n")
        claimed, = self.cli("wait", "--dispatch", source["dispatch"], "--scope", source["scope"],
                            "--timeout", "4")["messages"]
        self.assertEqual((claimed["type"], claimed["round"], claimed["attempt"], claimed["id"],
                          claimed["messageId"]),
                         ("done", 1, source["attempt"], done["message"], done["messageId"]))
        ack = self.cli("ack", done["messageId"])
        self.assertEqual((ack["message"], ack["messageId"], ack["state"]),
                         (done["message"], done["messageId"], "processed"))
        completed = self.status()
        self.assertEqual((completed["status"], completed["round"]), ("completed", 1))

        same_round = self.reverse_handoff(source["dispatch"])
        self.cli(*same_round, error="give the next --task and --report")
        self.assert_no_tmux_handoff()
        self.assertTrue(self.handoff_worker_alive(old))
        self.assertEqual(self.status().get("audit"), completed.get("audit"))

        next_task = self.root / "round-two-task.md"
        next_task_bytes = b"Work on the second task.\n"
        next_task.write_bytes(next_task_bytes)
        next_report = report.with_name("round-two-report.md")
        original_stop = original_receipt.with_suffix(".stopped")
        record = self.handoff_record(source)
        (self.handoff_traces / "reverse-check").write_text(json.dumps({
            "pid": old["pid"], "startTicks": old["startTicks"], "attempt": source["attempt"],
            "record": str(record), "stop": str(original_stop),
        }))
        args = same_round + ("--task", next_task, "--report", next_report)
        result = self.cli(*args)
        self.assertEqual((result["result"], result["round"], result["dispatch"], result["backend"],
                          result["fromAttempt"], result["endpoint"]),
                         ("accepted", 2, source["dispatch"], "tmux", source["attempt"], str(socket)))
        self.assertNotEqual(result["attempt"], source["attempt"])
        new = self.handoff_trace(result["attempt"])
        self.assert_worker_context(new, {
            **source, "round": 2, "attempt": result["attempt"],
            "task": str(next_task), "report": str(next_report),
        })
        self.assertTrue(new["sourceStoppedBeforeTarget"])
        self.assertFalse(self.handoff_worker_alive(old))
        self.assertTrue(self.handoff_worker_alive(new))
        self.assertIsNone(new["herdrPane"])
        self.assertIsNotNone(new["tmuxPane"])
        pane, session, pane_pid = subprocess.check_output(
            tmux + ["display-message", "-p", "-t", new["tmuxPane"],
                    "#{pane_id} #{session_name} #{pane_pid}"],
            env=self.env, text=True, timeout=5,
        ).split()
        self.assertEqual((pane, session, int(pane_pid)),
                         (new["tmuxPane"], "wfc-" + source["dispatch"], new["parentPid"]))

        target_receipt = Path(self.status()["create"]["receipt"])

        def started_receipt():
            receipt = json.loads(target_receipt.read_bytes())
            return receipt if receipt["category"] == "started" else None

        started = live.wait_for(started_receipt)
        current = self.status()
        self.assertEqual((current["round"], current["attempt"], current["scope"]),
                         (2, result["attempt"], source["scope"]))
        self.assertEqual((started["category"], started["pid"], started["processStart"]),
                         ("started", new["pid"], f'linux:{boot_id}:{new["startTicks"]}'))
        first, second = current["rounds"]
        self.assertEqual(first, completed["rounds"][0])
        self.assertEqual((second["round"], second["attempt"], second["task"], second["report"], second["state"]),
                         (2, result["attempt"], str(next_task), str(next_report), "running"))
        event, closed = current["audit"]
        self.assertEqual((event["id"], event["fromRound"], event["fromAttempt"], event["round"],
                          event["toAttempt"], event["phase"], event["result"]),
                         (result["audit"], 1, source["attempt"], 2, result["attempt"], "complete", "accepted"))
        self.assertEqual((event["fromTask"], event["fromReport"], event["toTask"], event["toReport"]),
                         (str(task), str(report), str(next_task), str(next_report)))
        self.assertEqual(event["close"], {**closed, "resourceCleanup": "confirmed"})
        self.assertEqual((closed["round"], closed["attempt"], closed["executionId"],
                          closed["closeOperationId"], closed["result"]),
                         (1, source["attempt"], source["attempt"], f'close:{source["attempt"]}', "completed"))
        stop_bytes = original_stop.read_bytes()
        stop = json.loads(stop_bytes)
        self.assertEqual(event["writerStopEvidence"], stop)
        self.assertEqual((stop["launchId"], stop["round"], stop["attempt"], stop["writerTreeStopped"]),
                         (original_receipt.stem, "1", source["attempt"], True))

        self.assertFalse(next_report.exists())
        next_done = self.complete_in_pane(new)
        next_report_bytes = b"Round 2 completed by its pane worker.\n"
        self.assertEqual(next_report.read_bytes(), next_report_bytes)
        finished = self.status()
        self.assertEqual((finished["status"], finished["round"], finished["attempt"]),
                         ("completed", 2, result["attempt"]))
        self.assertEqual(finished["rounds"][0], first)
        self.assertEqual(finished["rounds"][1]["result"]["messageId"], next_done["messageId"])
        self.assertEqual(finished["audit"], current["audit"])
        kept, replacement = finished["messages"]
        self.assertEqual((kept["type"], kept["round"], kept["attempt"], kept["id"], kept["messageId"],
                          kept["state"], kept["statusVersion"]),
                         ("done", 1, source["attempt"], done["message"], done["messageId"],
                          "processed", ack["statusVersion"]))
        self.assertEqual((replacement["type"], replacement["round"], replacement["attempt"],
                          replacement["id"], replacement["messageId"], replacement["report"]),
                         ("done", 2, result["attempt"], next_done["message"], next_done["messageId"],
                          str(next_report)))

        self.cli(*same_round, error="handoff requires a different host or backend")
        self.assertEqual(self.cli(*args), {**result, "duplicate": True})
        self.assertEqual(self.cli("ack", done["message"]), {**ack, "duplicate": True})
        after = self.status()
        self.assertEqual((after["audit"], after["rounds"], after["messages"]),
                         (finished["audit"], finished["rounds"], finished["messages"]))
        self.assertEqual(original_stop.read_bytes(), stop_bytes)
        self.assertEqual(task.read_bytes(), task_before)
        self.assertEqual(report.read_bytes(), report_before)
        self.assertEqual(next_task.read_bytes(), next_task_bytes)
        self.assertEqual(next_report.read_bytes(), next_report_bytes)
        self.assertEqual({path.stem for path in self.handoff_traces.glob("*.json")},
                         {source["attempt"], result["attempt"]})
        submission, = target_receipt.parent.glob("*.submission")
        self.assertEqual(submission.stem, target_receipt.stem)

        (self.handoff_traces / f'{result["attempt"]}.quit').touch()
        target_stop = target_receipt.with_suffix(".stopped")
        live.wait_for(target_stop.is_file)
        terminal = json.loads(target_receipt.read_bytes())
        replacement_stop = json.loads(target_stop.read_bytes())
        self.assertEqual((terminal["category"], terminal["exitCode"], terminal["launchId"]),
                         ("process_exited", 0, target_receipt.stem))
        self.assertEqual((replacement_stop["launchId"], replacement_stop["dispatch"],
                          replacement_stop["attempt"], replacement_stop["round"],
                          replacement_stop["writerTreeStopped"]),
                         (target_receipt.stem, source["dispatch"], result["attempt"], "2", True))
        self.assertFalse(self.handoff_worker_alive(new))
        print(json.dumps({
            "completedHerdrNextRound": True, "completedInTmuxPane": True,
            "sourceStoppedBeforeTarget": True, "replayRequiresNextPaths": True,
            "audit": result["audit"], "oldAttempt": source["attempt"], "newAttempt": result["attempt"],
            "oldCompletion": done, "newCompletion": next_done, "newDoneExitCode": 0, "paneExitCode": 0,
            "oldAckVersion": ack["statusVersion"], "oldClose": closed["closeOperationId"],
            "pane": pane, "panePid": int(pane_pid), "workerPid": new["pid"],
            "workerStartTicks": new["startTicks"], "tmuxSubmissions": 1,
        }), flush=True)
