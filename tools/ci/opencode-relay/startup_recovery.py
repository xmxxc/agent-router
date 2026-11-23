"""Verify a recovered tmux startup notice through the existing OpenCode fixture."""
import json
import os
from pathlib import Path
import shutil
import sys
import unittest

sys.path.insert(0, str(Path("tests").resolve()))
binary = sys.argv[1]
from opencode_relay import OpenCodeRelay

# Each original fixture consumes the release binary path during import.
sys.argv.insert(1, binary)
from startup_failure import StartupFailure


class StartupOpenCode(OpenCodeRelay):
    start_tmux = StartupFailure.start_tmux
    hold_tmux_create = StartupFailure.hold_tmux_create
    staged_startup_exit = StartupFailure.staged_startup_exit
    stop_after_startup_decision = StartupFailure.stop_after_startup_decision

    def until(self, condition, description="await the relay fixture"):
        return StartupFailure.until(self, condition, description)

    @unittest.skipUnless(shutil.which("tmux"), "requires real Linux tmux")
    def test_recovers_interrupted_notice_through_scoped_http(self):
        self.cwd = self.root
        self.state = Path(self.env["WFC_STATE"])
        socket = self.cwd / "socket"
        self.env.update(
            PATH=f"{self.cwd}:{os.environ['PATH']}", TERM="xterm", WFC_TMUX_SOCKET=str(socket),
        )
        self.tmux = [shutil.which("tmux"), "-S", str(socket), "-f", "/dev/null"]
        (self.cwd / "report").write_text("startup evidence")
        self.start_tmux()

        child, selected = self.stop_after_startup_decision("ours")
        ledger = self.state / "messages.json"
        self.cli("relay", *self.target(), "--scope", "neighbor", "--for", ".2")
        self.assertEqual(self.posts, [])
        self.assertFalse(ledger.exists())

        # This synthetic history row uses #36's explicit worker event shape
        # for the same attempt whose durable startup decision is waiting.
        reclaimed_id = "m-fixture-reclaimed"
        ledger.write_text(json.dumps([{
            "id": 1, "messageId": reclaimed_id, "dispatch": child["dispatch"],
            "attempt": child["attempt"], "round": selected["round"], "scope": selected["scope"],
            "role": selected["role"], "report": selected["report"], "tmux": "worker-history",
            "type": "reclaimed", "state": "unsent", "statusVersion": 1,
        }]))
        self.send("1.5")
        post, = self.posts
        notice, = self.batch(post)
        self.assertEqual(
            (notice["dispatch"], notice["round"], notice["attempt"], notice["scope"], notice["kind"]),
            (child["dispatch"], 1, child["attempt"], "ours", "startup_failure"),
        )
        launch = Path(selected["create"]["receipt"]).stem
        self.assertEqual(
            (notice["fault"]["launchId"], notice["fault"]["exitCode"], notice["fault"]["pid"]),
            (launch, 17, child["pid"]),
        )
        event, history = self.data("status", "--dispatch", child["dispatch"])["dispatches"][0]["messages"]
        self.assertEqual((event["messageId"], event["type"], event["to"]),
                         (reclaimed_id, "reclaimed", "worker"))
        self.cli("ack", reclaimed_id, code=1)
        identity = (notice["messageId"], notice["id"])
        self.assertEqual((history["messageId"], history["id"]), identity)
        self.assertEqual((history["state"], history["statusVersion"]), ("received", 3))
        ack = self.data("ack", notice["messageId"])
        self.assertEqual((ack["messageId"], ack["message"]), identity)
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))
        self.assertTrue(self.data("ack", notice["messageId"])["duplicate"])
        self.assertFalse(self.data("wait", "--timeout", "0", code=3)["messages"])
        self.send(".3")
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(len(json.loads(ledger.read_text())), 2)
        self.assertEqual(self.calls(), [])
        print(json.dumps({
            "dispatch": child["dispatch"], "attempt": child["attempt"], "launchId": launch,
            "messageId": ack["messageId"], "message": ack["message"], "posts": 1,
            "wrongScopePosts": 0, "workerHistory": reclaimed_id, "ackVerified": True,
        }), flush=True)
