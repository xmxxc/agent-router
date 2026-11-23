import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

producer, candidate = map(lambda p: str(Path(p).resolve()), sys.argv[1:])
for execution in ({}, {"executionId": "A"}):
    with tempfile.TemporaryDirectory(prefix="r9-legacy-") as directory:
        root = Path(directory)
        endpoint = root / "new-parent" / "socket"
        state = root / "state"
        env = dict(PATH="/usr/bin:/bin", HOME=directory, LANG="C.UTF-8", TERM="xterm",
                   WFC_STATE=str(state), WFC_TMUX_SOCKET=str(endpoint))
        scope = dict(bootId=Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                     pidNamespace=Path("/proc/self/ns/pid").stat().st_ino,
                     networkNamespace=Path("/proc/self/ns/net").stat().st_ino,
                     userNamespace=Path("/proc/self/ns/user").stat().st_ino,
                     effectiveUid=os.geteuid())
        tmux = ["/usr/bin/tmux", "-S", str(endpoint)]
        def request(binary, op, **fields):
            process = subprocess.run([binary, "--tmux-backend"], env=env, text=True, capture_output=True,
                                     check=True, timeout=15, input=json.dumps(dict(op=op, dispatch="d-upgrade", **fields)))
            return json.loads(process.stdout)
        launch = dict(cwd=directory, argv=["/bin/cat"])
        try:
            first = request(producer, "create", **execution, **launch)
            receipt = Path(first["receipt"])
            old_bytes = receipt.read_bytes()
            assert json.loads(old_bytes) == {"category": "not_started"}, first
            endpoint.parent.mkdir()
            assert request(candidate, "inspect", **execution)["result"] == "uncertain"
            mismatch = {**scope, "networkNamespace": scope["networkNamespace"] + 1}
            assert request(candidate, "inspect", legacyRecovery=mismatch, **execution)["result"] == "uncertain"
            absent = request(candidate, "inspect", legacyRecovery=scope, **execution)
            assert absent == dict(result="accepted", alive=False, owned=False), absent
            second = request(candidate, "create", **execution, **launch)
            assert second["result"] == "accepted", second
            assert receipt.read_bytes() == old_bytes
            print("genuine R7 untyped receipt", execution, json.loads(old_bytes),
                  "verified same-scope absence", absent, "retried worker", second, flush=True)
        finally:
            subprocess.run(tmux + ["kill-server"], env=env, capture_output=True, timeout=8)
