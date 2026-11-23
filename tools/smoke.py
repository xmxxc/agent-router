"""Native sidecar round trip with a single controlled terminal process."""
import json
from pathlib import Path
import subprocess
import sys
from threading import Timer
import time
import uuid


def run(binary, *args):
    with subprocess.Popen([str(binary), *args], cwd=binary.parent, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, encoding="utf-8") as kernel:
        # Bound a stalled hello or RPC as well as the polling loop.
        deadline = Timer(45, kernel.kill)
        deadline.start()
        try:
            hello = json.loads(kernel.stdout.readline())
            if hello["protocol"] != 1 or hello["transport"] != "private-stdio-ndjson":
                raise RuntimeError(f"Unexpected kernel hello: {hello}")
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
                if reply["id"] != sequence or not reply["ok"]:
                    raise RuntimeError(f"Unexpected {operation} reply: {reply}")
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
            resized = call("resize", cols=80, rows=20)
            if resized != {"cols": 80, "rows": 20}:
                raise RuntimeError(f"Unexpected resize result: {resized}")
            view = call("read")
            if (view["cols"], view["rows"]) != (80, 20):
                raise RuntimeError(f"Unexpected viewport after resize: {view}")
            marker = uuid.uuid4().hex
            call("input", text=marker + "\r")
            observe(marker[::-1])
            closed = call("close")
            if closed["closed"] is not True:
                raise RuntimeError(f"Unexpected close result: {closed}")
            kernel.stdin.close()
            status = kernel.wait(timeout=10)
            if status != 0:
                raise RuntimeError(f"Kernel exited with status {status}")
            print("Native PTY create, resize, submit, observe child response, close passed")
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
    run(Path(sys.argv[1]).resolve(), *sys.argv[2:])
