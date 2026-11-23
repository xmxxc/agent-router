"""Exercise recovery through fresh CLI processes against a private inbox."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

BINARY = str(Path(sys.argv.pop(1)).resolve())


class Receipts(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.state = self.root / "state"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("WFC_")}
        self.env["WFC_STATE"] = str(self.state)
        (self.root / "task").touch()
        self.dispatch = self.run_cli("dispatch", "--role", '复核 "\\n"\t员', "--task", "task",
                                     "--cwd", ".", "--", sys.executable, "-c", "pass")
        self.id = self.dispatch["dispatch"]
        self.worker = dict(self.env, WFC_DISPATCH=self.id, WFC_ROUND="1",
                           WFC_ATTEMPT=self.dispatch["attempt"])
        self.done = self.run_cli("done", "--report", "task", "--attempt", self.dispatch["attempt"],
                                 "--message-id", "stable", env=self.worker)
        self.mid = self.done["messageId"]

    def run_cli(self, *args, env=None, code=0):
        proc = subprocess.run([BINARY, *args], cwd=self.root, env=env or self.env,
                              capture_output=True, text=True, encoding="utf-8", timeout=10)
        self.assertEqual(proc.returncode, code, (args, proc.stdout, proc.stderr))
        if code in (0, 3):
            return json.loads(proc.stdout)
        self.assertEqual(proc.stdout, "")
        self.assertTrue(proc.stderr)

    def history(self):
        return self.run_cli("status", "--dispatch", self.id)["dispatches"][0]["messages"]

    @staticmethod
    def body(message):
        fields = ("attempt", "dirty", "head", "report", "role", "round", "scope")
        return json.dumps({k: message[k] for k in fields if k in message},
                          ensure_ascii=False, sort_keys=True)

    def race(self, command, state, version):
        with ThreadPoolExecutor(max_workers=8) as pool:
            receipts = list(pool.map(command, range(8)))
        self.assertEqual(sum(not r["duplicate"] for r in receipts), 1)
        for receipt in receipts:
            self.assertEqual((receipt["messageId"], receipt["state"], receipt["statusVersion"]),
                             (self.mid, state, version))
        return next(r for r in receipts if not r["duplicate"])

    def test_concurrent_ack(self):
        self.run_cli("ack", self.mid, code=1)
        self.run_cli("ack", "m-absent", code=1)
        self.assertEqual(self.history()[0]["state"], "unsent")
        msg, = self.run_cli("wait", "--timeout", "0")["messages"]
        self.assertEqual(self.history()[0]["messageId"], self.mid)
        self.assertEqual(self.run_cli("wait", "--timeout", "0", code=3),
                         {"messages": [], "timedOut": True})
        canonical = self.body(msg)
        self.run_cli("ack", "--body", "", self.mid, code=1)
        self.run_cli("ack", self.mid, "--body", "{}", code=1)
        aliases = (self.mid, str(self.done["message"]))
        self.race(lambda i: self.run_cli("ack", aliases[i % 2], "--body", canonical), "processed", 3)
        self.run_cli("ack", self.mid, "--body", canonical + " ", code=1)
        self.assertEqual(self.history()[0]["statusVersion"], 3)
        replay = self.run_cli("done", "--report", "task", "--attempt", self.dispatch["attempt"],
                              "--message-id", "stable", env=self.worker)
        self.assertEqual((replay["messageId"], replay["state"], replay["statusVersion"]),
                         (self.mid, "processed", 3))

    def test_resolve_unknown_and_replay_after_ack(self):
        msg, = self.history()
        digest = hashlib.sha256(self.body(msg).encode()).hexdigest()
        args = ["resolve", self.mid, "--actor", "reviewer", "--reason", "verified",
                "--evidence", "external receipt 47", "--body-hash", digest]
        self.run_cli(*args, code=1)
        # Model a relay whose external delivery outcome was lost, as in Python #40.
        path = self.state / "messages.json"
        rows = json.loads(path.read_bytes())
        rows[0].update(state="unknown", statusVersion=2)
        path.write_text(json.dumps(rows), encoding="utf-8")
        self.run_cli("wait", "--timeout", "0", code=3)
        self.run_cli("ack", self.mid, code=1)
        self.run_cli(*args[:-1], "0" * 64, code=1)
        self.run_cli(*args[:6], *args[8:], code=2)
        self.assertEqual(self.history()[0]["state"], "unknown")
        resolved = self.race(lambda _: self.run_cli(*args), "received", 3)
        self.assertEqual(resolved["resolution"]["source"], "operator")
        self.assertEqual(self.history()[0]["resolution"], resolved["resolution"])
        self.assertTrue(self.history()[0]["delivered"])
        ack = self.run_cli("ack", self.mid)
        self.assertEqual((ack["state"], ack["statusVersion"]), ("processed", 4))
        for index in (3, 5, 7, 9):
            changed = args.copy()
            changed[index] += "x"
            self.run_cli(*changed, code=1)
        self.run_cli(*args, "--source", "different", code=1)
        self.assertEqual(self.run_cli("resolve", "--source", "operator", *args[1:]),
                         dict(ack, duplicate=True))
        self.assertEqual(self.history()[0]["statusVersion"], 4)

    def test_mixed_legacy_inbox(self):
        path = self.state / "messages.json"
        row = json.loads(path.read_bytes())[0]
        # Exact storage fields written by the #6 baseline.
        legacy = {k: row[k] for k in ("id", "messageId", "dispatch", "role", "round",
                                      "attempt", "report", "request_key")}
        rows = [dict(legacy, received=True),
                dict(legacy, id=2, messageId="m-old2", received=False)]
        path.write_text(json.dumps(rows), encoding="utf-8")
        before = path.read_bytes()
        self.assertEqual([m["statusVersion"] for m in self.history()], [2, 1])
        self.assertEqual(path.read_bytes(), before)
        self.run_cli("ack", "1")
        self.assertEqual([m["id"] for m in self.run_cli("wait", "--timeout", "0")["messages"]], [2])
        self.assertEqual([(m["state"], m["statusVersion"]) for m in self.history()],
                         [("processed", 3), ("received", 2)])

    def test_scoped_snapshot_survives_confirmation(self):
        # A revised #6 publisher can omit the request key and store null Git data.
        path = self.state / "messages.json"
        first = json.loads(path.read_bytes())[0]
        row = {key: first[key] for key in ("id", "messageId", "dispatch", "role", "round",
                                          "attempt", "report")}
        row.update(received=True, request_key=None, scope="own", head=None, dirty=None)
        path.write_text(json.dumps([row]), encoding="utf-8")
        stored, = self.history()
        self.assertEqual([stored[k] for k in ("scope", "head", "dirty")], ["own", None, None])
        self.run_cli("ack", self.mid, "--body", self.body(stored))
        after, = self.history()
        self.assertEqual(self.body(after), self.body(stored))
        self.assertEqual(after["state"], "processed")


if __name__ == "__main__":
    unittest.main()
