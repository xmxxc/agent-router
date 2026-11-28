"""Native sidecar round trip with a single controlled terminal process."""
import json
from pathlib import Path
import subprocess
import sys
from threading import Timer
import time
import uuid


def windows_process(identity):
    import ctypes as ct
    from ctypes import wintypes as wt

    api = ct.WinDLL("kernel32", use_last_error=True)
    api.OpenProcess.argtypes = wt.DWORD, wt.BOOL, wt.DWORD
    api.OpenProcess.restype = wt.HANDLE
    api.GetProcessTimes.argtypes = (wt.HANDLE,) + (ct.POINTER(wt.FILETIME),) * 4
    api.WaitForSingleObject.argtypes = wt.HANDLE, wt.DWORD
    api.WaitForSingleObject.restype = wt.DWORD
    api.CloseHandle.argtypes = (wt.HANDLE,)
    handle = api.OpenProcess(0x101000, False, identity["pid"])
    if not handle:
        raise ct.WinError(ct.get_last_error())
    try:
        times = [wt.FILETIME() for _ in range(4)]
        if not api.GetProcessTimes(handle, *(ct.byref(t) for t in times)):
            raise ct.WinError(ct.get_last_error())
        creation = str(times[0].dwHighDateTime << 32 | times[0].dwLowDateTime)
        if creation != identity["creation"]:
            raise RuntimeError(f"ConPTY process identity changed for PID {identity['pid']}")
    except BaseException:
        api.CloseHandle(handle)
        raise
    return api, handle


def run(binary, *args):
    with subprocess.Popen([str(binary), *args], cwd=binary.parent, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, encoding="utf-8") as kernel:
        # Bound a stalled hello or RPC as well as the polling loop.
        deadline = Timer(45, kernel.kill)
        deadline.start()
        pinned = None
        try:
            hello = json.loads(kernel.stdout.readline())
            if hello["protocol"] != 1 or hello["transport"] != "private-stdio-ndjson":
                raise RuntimeError(f"Unexpected kernel hello: {hello}")
            generation = hello["generation"]
            unit = dict(unit="native", generation=generation)
            sequence = 0

            def request(operation, **fields):
                nonlocal sequence
                sequence += 1
                request = dict(unit, op=operation, id=sequence, **fields)
                kernel.stdin.write(json.dumps(request) + "\n")
                kernel.stdin.flush()
                reply = json.loads(kernel.stdout.readline())
                if reply["id"] != sequence or reply["generation"] != generation:
                    raise RuntimeError(f"Unexpected {operation} reply: {reply}")
                return reply

            def call(operation, **fields):
                reply = request(operation, **fields)
                if not reply["ok"]:
                    raise RuntimeError(f"Unexpected {operation} reply: {reply}")
                return reply["result"]

            if sys.platform == "win32":
                private_arg = "fixture-private-launch-argument"
                for program, category, error in [
                    (binary.parent / (uuid.uuid4().hex + ".exe"), "executable_missing", 2),
                    (binary.parent, "permission_denied", 5),
                ]:
                    rejected = request("spawn", argv=[str(program), private_arg], cols=100, rows=24)
                    assert rejected["ok"] is False and rejected["errorCategory"] == category, rejected
                    assert f"os error {error}" in rejected["error"], rejected
                    assert private_arg not in rejected["error"] and str(program) not in rejected["error"], rejected
                print("ConPTY reports missing executable and permission denial with redacted diagnostics")

            script = "print('READY', flush=True); print(input()[::-1], flush=True); input()"
            child = call("spawn", argv=[sys.executable, "-u", "-c", script], cols=100, rows=24,
                         **({"processIdentity": True} if sys.platform == "win32" else {}))
            if sys.platform == "win32":
                pinned = windows_process(child["identity"])
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
            if pinned is not None:
                api, handle = pinned
                if api.WaitForSingleObject(handle, 0) != 0:
                    raise RuntimeError(f"Close acknowledged before process exit: {child['identity']}")
                print(f"Windows process signalled at close: {child['identity']}")
            if closed["closed"] is not True:
                raise RuntimeError(f"Unexpected close result: {closed}")
            kernel.stdin.close()
            status = kernel.wait(timeout=10)
            if status != 0:
                raise RuntimeError(f"Kernel exited with status {status}")
            print("Native PTY create, resize, submit, observe child response, close passed")
        finally:
            deadline.cancel()
            if pinned is not None:
                api, handle = pinned
                api.CloseHandle(handle)
            if kernel.poll() is None:
                kernel.stdin.close()
                try:
                    kernel.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    kernel.kill()
                    kernel.wait()


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve(), *sys.argv[2:])
