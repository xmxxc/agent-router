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
LEGACY_FIELDS = ("id", "messageId", "dispatch", "role", "round", "attempt", "report", "request_key")


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
                                     "--cwd", ".", "--report", "task", "--", sys.executable, "-c", "pass")
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
        legacy = {k: row[k] for k in LEGACY_FIELDS}
        rows = [dict(legacy, received=True),
                dict(legacy, id=2, messageId="m-old2", received=False)]
        path.write_text(json.dumps(rows), encoding="utf-8")
        before = path.read_bytes()
        received, pending = self.history()
        self.assertEqual([received["statusVersion"], pending["statusVersion"]], [2, 1])
        self.assertNotIn("head", received)
        self.assertEqual([pending[k] for k in ("scope", "head", "dirty")], [self.id, None, None])
        self.assertEqual(path.read_bytes(), before)
        self.run_cli("ack", "1")
        claimed, = self.run_cli("wait", "--scope", self.id, "--timeout", "0")["messages"]
        self.assertEqual(claimed["id"], 2)
        self.assertEqual(self.body(claimed), self.body(pending))
        self.assertEqual([(m["state"], m["statusVersion"]) for m in self.history()],
                         [("processed", 3), ("received", 2)])

    def test_pre_scope_versioned_receipt_keeps_original_body(self):
        path = self.state / "messages.json"
        row, = json.loads(path.read_bytes())
        for field in ("scope", "head", "dirty"):
            del row[field]
        canonical = self.body(row)
        row["body_hash"] = hashlib.sha256(canonical.encode()).hexdigest()
        unsigned = dict(row, id=2, messageId="m-migrated-unsigned")
        del unsigned["body_hash"]
        path.write_text(json.dumps([row, unsigned]), encoding="utf-8")
        self.assertEqual([self.body(msg) for msg in self.history()], [canonical, canonical])
        claimed = self.run_cli("wait", "--scope", self.id, "--timeout", "0")["messages"]
        for msg in claimed:
            self.assertEqual(self.body(msg), canonical)
            self.run_cli("ack", msg["messageId"], "--body", self.body(dict(msg, scope=self.id)), code=1)
            self.run_cli("ack", msg["messageId"], "--body", canonical)
        self.assertEqual([self.body(msg) for msg in self.history()], [canonical, canonical])

    def test_first_wait_binds_the_delivered_legacy_body(self):
        path = self.state / "messages.json"
        row, = json.loads(path.read_bytes())
        legacy = {k: row[k] for k in LEGACY_FIELDS}
        path.write_text(json.dumps([dict(legacy, received=False)]), encoding="utf-8")
        claimed, = self.run_cli("wait", "--timeout", "0")["messages"]
        self.run_cli("ack", self.mid, "--body", self.body(legacy), code=1)
        self.assertEqual(self.history()[0]["statusVersion"], 2)
        self.assertEqual(self.run_cli("ack", self.mid, "--body", self.body(claimed))["statusVersion"], 3)

    def test_original_body_after_unscoped_null_snapshot_rewrite(self):
        path = self.state / "messages.json"
        first, = json.loads(path.read_bytes())
        row = {k: first[k] for k in LEGACY_FIELDS}
        original = self.body(row)
        row.update(received=True, head=None, dirty=None)
        path.write_text(json.dumps([row]), encoding="utf-8")
        expanded = self.body(self.history()[0])
        for field, value in (("role", "different"), ("attempt", "wrong"), ("report", "elsewhere"),
                             ("round", 2)):
            wrong = self.body(dict(json.loads(original), **{field: value}))
            self.run_cli("ack", self.mid, "--body", wrong, code=1)
        self.run_cli("ack", self.mid, "--body", original + " ", code=1)
        self.assertEqual(self.run_cli("ack", self.mid, "--body", original)["statusVersion"], 3)
        for body in (original, expanded):
            self.assertTrue(self.run_cli("ack", self.mid, "--body", body)["duplicate"])

        # These rows can be distinguished from receipts rewritten from the
        # original publisher. A saved digest always retains strict verification.
        for protected in (dict(scope=self.id), dict(request_key=None), dict(dirty=False),
                          dict(created_at="2026-01-01 12:00:00"), dict(head="a" * 40),
                          dict(body_hash=hashlib.sha256(expanded.encode()).hexdigest()),
                          dict(body_hash=hashlib.sha256(original.encode()).hexdigest())):
            with self.subTest(protected=protected):
                path.write_text(json.dumps([dict(row, **protected)]), encoding="utf-8")
                self.run_cli("ack", self.mid, "--body", original, code=1)
                self.assertEqual(self.history()[0]["state"], "received")

    @unittest.skipUnless(sys.platform == "linux" and os.geteuid() != 0,
                         "requires ordinary Linux directory permissions")
    def test_confirmation_retries_sync_parent_after_rename(self):
        path = self.state / "messages.json"
        initial = path.read_bytes()
        digest = json.loads(initial)[0]["body_hash"]
        for operation in ("ack", "resolve"):
            with self.subTest(operation=operation):
                path.write_bytes(initial)
                if operation == "ack":
                    self.run_cli("wait", "--timeout", "0")
                    args = ["ack", self.mid]
                    expected = ("processed", 3)
                else:
                    rows = json.loads(initial)
                    rows[0].update(state="unknown", statusVersion=8)
                    path.write_text(json.dumps(rows), encoding="utf-8")
                    args = ["resolve", self.mid, "--actor", "reviewer", "--reason", "verified",
                            "--source", "operator", "--evidence", "delivery-47", "--body-hash", digest]
                    expected = ("received", 9)
                self.state.chmod(0o300)
                try:
                    self.run_cli(*args, code=1)
                    after_rename = self.history()[0]
                    self.assertEqual((after_rename["state"], after_rename["statusVersion"]), expected)
                    self.run_cli(*args, code=1)
                finally:
                    self.state.chmod(0o700)
                retry = self.run_cli(*args)
                self.assertTrue(retry["duplicate"])
                self.assertEqual(self.history()[0], after_rename)
                self.assertEqual(retry.get("resolution"), after_rename.get("resolution"))

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

    def test_only_first_receipt_for_legacy_attempt_accepts_original_body(self):
        path = self.state / "messages.json"
        new, = json.loads(path.read_bytes())
        row = {k: new[k] for k in LEGACY_FIELDS}
        original = self.body(row)
        row.update(received=True, head=None, dirty=None)
        rows = [dict(row, id=1, messageId="m-other-dispatch", dispatch="d-other"),
                dict(row, id=2, messageId="m-other-attempt", attempt="a-earlier"),
                dict(row, id=3, messageId="m-first"),
                dict(row, id=4, messageId="m-second", request_key="second")]
        path.write_text(json.dumps(rows), encoding="utf-8")
        self.run_cli("ack", "m-second", "--body", original, code=1)
        _, first, second = self.history()
        self.assertEqual((first["statusVersion"], second["statusVersion"]), (2, 2))
        self.assertEqual(self.run_cli("ack", "m-second", "--body", self.body(second))["statusVersion"], 3)
        self.assertEqual(self.run_cli("ack", "m-first", "--body", original)["statusVersion"], 3)
        self.assertTrue(self.run_cli("ack", "m-first", "--body", original)["duplicate"])
        self.run_cli("ack", "m-second", "--body", original, code=1)


if __name__ == "__main__":
    unittest.main()
