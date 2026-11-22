"""Native sidecar round trip with a single controlled terminal process."""
import json
from pathlib import Path
import subprocess
import sys
from threading import Timer
import time
import uuid


def run(binary):
    with subprocess.Popen([str(binary)], cwd=binary.parent, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, encoding="utf-8") as kernel:
        # Bound a stalled hello or RPC as well as the polling loop.
        deadline = Timer(45, kernel.kill)
        deadline.start()
        try:
            hello = json.loads(kernel.stdout.readline())
            assert hello["protocol"] == 1 and hello["transport"] == "private-stdio-ndjson", hello
            generation = hello["generation"]
            unit = dict(unit="native", generation=generation)
            sequence = 0

            def call(operation, **fields):
                nonlocal sequence
                sequence += 1
                request = dict(unit, op=operation, id=sequence, **fields)
                kernel.stdin.write(json.dumps(request) + "\n")
                kernel.stdin.flush()
                reply = json.loads(kernel.stdout.readline())
                assert reply["id"] == sequence and reply["ok"], reply
                return reply["result"]

            script = "print('READY', flush=True); print(input()[::-1], flush=True); input()"
            child = call("spawn", argv=[sys.executable, "-u", "-c", script], cols=100, rows=24)
            unit["nonce"] = child["nonce"]

            def observe(marker):
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    view = call("read")
                    if marker in view["text"]:
                        return
                    time.sleep(0.1)
                raise RuntimeError(view)

            observe("READY")
            assert call("resize", cols=80, rows=20) == {"cols": 80, "rows": 20}
            view = call("read")
            assert (view["cols"], view["rows"]) == (80, 20), view
            marker = uuid.uuid4().hex
            call("input", text=marker + "\r")
            observe(marker[::-1])
            assert call("close")["closed"]
            kernel.stdin.close()
            assert kernel.wait(timeout=10) == 0
            print("Native PTY create, submit, observe child response, close passed")
        finally:
            deadline.cancel()
            if kernel.poll() is None:
                kernel.stdin.close()
                try:
                    kernel.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    kernel.kill()
                    kernel.wait()


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve())
