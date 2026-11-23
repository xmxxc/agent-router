"""Run a command as a Linux subreaper. Account for the command's descendants."""
import ctypes
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0
try:
    code = subprocess.run(sys.argv[1:]).returncode
finally:
    deadline = time.monotonic() + 5
    forced = set()
    while True:
        try:
            pid, status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            break
        if pid:
            continue
        if time.monotonic() >= deadline:
            for child in Path('/proc/thread-self/children').read_text().split():
                os.kill(int(child), signal.SIGKILL)
                forced.add(child)
        time.sleep(.02)
    print('Own descendants reaped: ECHILD, extra cleanup signals:', len(forced), flush=True)
sys.exit(code or int(bool(forced)))
