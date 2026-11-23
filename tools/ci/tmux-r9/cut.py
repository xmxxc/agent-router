import json, os, runpy, subprocess, sys, unittest
from pathlib import Path

baseline = len(sys.argv) > 2
sys.argv = [sys.argv[0], sys.argv[1]]
Backend = runpy.run_path("tools/check_tmux.py", run_name="suite")["Backend"]

class Cut(Backend):
    def test_stop_before_submission(self):
        with (self.root / "tmux-requests").open("w") as requests:
            shim = self.work / "tmux"
            shim.write_text(f"""#!{sys.executable}
import os, sys
with open({requests.name!r}, 'a') as log: print(' '.join(sys.argv[1:]), file=log)
os.execv('/usr/bin/tmux', ['/usr/bin/tmux', *sys.argv[1:]])
""")
            shim.chmod(0o700)
        parked = self.root / "parked"
        env = dict(PATH=str(self.work), LD_PRELOAD=os.environ.get("WFC_R9_CUT_LIB", str(Path(__file__).with_name("park.so"))),
                   R9_CUT_CLAIM=str(self.state / "tmux/d-abc12345.json"),
                   R9_CUT_SOCKET=self.socket, R9_CUT_PARKED=str(parked))
        try:
            reply = self.call("create", executionId="A", environ=env, **self.launch)
            self.assertTrue(parked.is_socket())
        finally:
            if parked.exists():
                parked.rename(self.socket)
        requests = (self.root / "tmux-requests").read_text()
        self.assertNotIn("new-session", requests)
        record, = (self.state / "launches").glob("*.submission")
        phase = json.loads(record.read_text())["phase"]
        key = dict(executionId="A", closeOperationId="end")
        closed = self.call("close", **key)
        print("create", reply, "phase", phase, "tmux calls", requests.splitlines(),
              "ordinary close after restoration", closed, flush=True)
        self.assertEqual(phase, "InFlight" if baseline else "Prepared")
        self.assertEqual(closed["result"], "uncertain" if baseline else "completed")
        self.assertNotIn("category", closed)
        self.assertEqual(self.call("close", **key), closed)

result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite([Cut("test_stop_before_submission")]))
sys.exit(not result.wasSuccessful())
