"""Worker for the Linux PTY ownership regression, launched inside the test pane."""
import os
from pathlib import Path
import signal
import time


def wait_file(name):
    while not Path(name).exists():
        time.sleep(.02)


# The PTY job starts a helper in its foreground group. After the PTY root
# retakes the terminal, the helper continues independently until released.
wait_file('start-job')
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
pid = os.fork()
if pid == 0:
    os.setpgid(0, 0)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    if os.fork() == 0:
        print("foreground writer", os.getpid(), flush=True)
        Path('job-pid').write_text(str(os.getpid()))
        wait_file('background-job')
        os.setsid()
        Path('detached').touch()
        wait_file('stop-job')
        os._exit(0)
    wait_file('detached')
    os._exit(0)

wait_file('job-pid')
os.tcsetpgrp(0, pid)
wait_file('background-job')
os.tcsetpgrp(0, os.getpgrp())
os.waitpid(pid, 0)
Path('backgrounded').touch()
while True:
    signal.pause()
