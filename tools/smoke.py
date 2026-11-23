"""Native sidecar round trip with a single controlled terminal process."""
from contextlib import contextmanager, nullcontext
import json
import os
from pathlib import Path
import subprocess
import sys
from threading import Timer
import time
import uuid


def windows_fixture(binary):
    system32 = Path(os.environ["SystemRoot"]) / "System32"
    powershell = system32 / "WindowsPowerShell/v1.0/powershell.exe"
    env = {k: os.environ[k] for k in ("SystemRoot", "TEMP", "TMP", "USERPROFILE") if k in os.environ}
    env["PATH"] = str(system32)
    for directory in (binary.parent, system32):
        for tool in ("herdr.exe", "tmux.exe", "python.exe", "python3.exe", "py.exe"):
            if (directory / tool).exists():
                raise RuntimeError(f"External tool visible on kernel search path: {directory / tool}")
    script = (
        "[Console]::WriteLine(('READY-{0}!' -f $PID)); "
        "$chars = [Console]::ReadLine().ToCharArray(); [Array]::Reverse($chars); "
        "[Console]::WriteLine((-join $chars)); [Threading.Thread]::Sleep(60000)"
    )
    return env, [str(powershell), "-NoLogo", "-NoProfile", "-Command", script]


@contextmanager
def windows_child_exit(pid):
    import _winapi
    import ctypes
    from ctypes import wintypes

    # Hold the original process object across close, even if its PID is later reused.
    handle = _winapi.OpenProcess(0x1000 | 0x100000, False, pid)  # QUERY_LIMITED_INFORMATION | SYNCHRONIZE
    try:
        get_times = ctypes.WinDLL("kernel32", use_last_error=True).GetProcessTimes
        get_times.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
        get_times.restype = wintypes.BOOL
        times = [wintypes.FILETIME() for _ in range(4)]
        if not get_times(handle, *(ctypes.byref(t) for t in times)):
            raise ctypes.WinError(ctypes.get_last_error())
        creation = times[0].dwHighDateTime << 32 | times[0].dwLowDateTime
        if not creation or _winapi.WaitForSingleObject(handle, 0) != _winapi.WAIT_TIMEOUT:
            raise RuntimeError(f"Expected live child pid={pid} creation={creation}")
        yield
        if _winapi.WaitForSingleObject(handle, 0) != _winapi.WAIT_OBJECT_0:
            raise RuntimeError(f"Child still alive after close: pid={pid} creation={creation}")
        print(f"Windows child exited on close: pid={pid} creation FILETIME={creation}")
    finally:
        _winapi.CloseHandle(handle)


def run(binary, *args):
    if os.name == "nt":
        env, argv = windows_fixture(binary)
    else:
        env = None
        script = "print('READY', flush=True); print(input()[::-1], flush=True); input()"
        argv = [sys.executable, "-u", "-c", script]
    with subprocess.Popen([str(binary), *args], cwd=binary.parent, env=env, stdin=subprocess.PIPE,
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

            child = call("spawn", argv=argv, cols=100, rows=24)
            unit["nonce"] = child["nonce"]

            def observe(marker):
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    view = call("read")
                    if marker in view["text"]:
                        return
                    time.sleep(0.1)
                raise RuntimeError(view)

            observe(f"READY-{child['pid']}!" if os.name == "nt" else "READY")
            resized = call("resize", cols=80, rows=20)
            if resized != {"cols": 80, "rows": 20}:
                raise RuntimeError(f"Unexpected resize result: {resized}")
            view = call("read")
            if (view["cols"], view["rows"]) != (80, 20):
                raise RuntimeError(f"Unexpected viewport after resize: {view}")
            marker = uuid.uuid4().hex
            call("input", text=marker + "\r")
            observe(marker[::-1])
            with windows_child_exit(child["pid"]) if os.name == "nt" else nullcontext():
                closed = call("close")
                if closed["closed"] is not True:
                    raise RuntimeError(f"Unexpected close result: {closed}")
            print(f"Observed child response {marker[::-1]}")
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
