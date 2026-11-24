"""Run an isolated CI group as a Linux subreaper and diagnose leftovers."""
import ctypes
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


FIXTURE_SCRIPTS = {b"handoff-host.py", b"resume-host.py", b"check_tmux.py",
                   b"worker script.py", b"herdr_public.py", b"public_host_resume.py",
                   b"startup_failure.py", b"pane_worker.py", b"pane_completion.py"}


def children(pid):
    result = set()
    for path in (Path("/proc") / str(pid) / "task").glob("*/children"):
        try:
            result.update(int(child) for child in path.read_bytes().split())
        except OSError:
            continue
    return result


def process(pid, group_root):
    proc = Path("/proc") / str(pid)
    try:
        stat = (proc / "stat").read_bytes()
    except OSError:
        return None
    name, fields = stat.rsplit(b")", 1)
    name = name.split(b"(", 1)[1]
    fields = fields.split()
    if len(fields) <= 19:
        return None
    data = dict(pid=pid, ppid=int(fields[1]), pgrp=int(fields[2]), session=int(fields[3]),
                startTicks=int(fields[19]), state=fields[0].decode("ascii"), kind="unknown")
    try:
        argv = (proc / "cmdline").read_bytes().split(b"\0")
        status = (proc / "status").read_bytes()
    except OSError:
        return data
    if b"--tmux-host" in argv[1:]:
        data["kind"] = "wfc-tmux-supervisor"
    elif b"--tmux-backend" in argv[1:]:
        data["kind"] = "wfc-tmux-backend"
    elif name in (b"tmux: server", b"tmux"):
        data["kind"] = name.decode("ascii")
    elif name in (b"cat", b"herdr", b"sleep", b"sh", b"bash", b"timeout"):
        data["kind"] = name.decode("ascii")
    elif name.startswith(b"python"):
        script = os.fsencode(Path(os.fsdecode(argv[1])).name) if len(argv) > 1 else b""
        data["kind"] = script.decode("ascii") if script in FIXTURE_SCRIPTS else "python"
        # The detached handoff descendant is the only fixture using this stop argument.
        if any(re.fullmatch(rb"a-[0-9a-f]{32}\.stop", arg) for arg in argv[1:]):
            data["kind"] = "handoff-detached-fixture"
    ignored = re.search(rb"(?m)^SigIgn:\s*([0-9a-f]+)$", status)
    if ignored:
        data["hangupIgnored"] = bool(int(ignored[1], 16) & (1 << (signal.SIGHUP - 1)))
    try:
        cwd = Path(os.readlink(proc / "cwd"))
    except OSError:
        return data
    if cwd.is_relative_to(group_root / "tmp"):
        relative = cwd.relative_to(group_root / "tmp")
        if relative.parts and re.fullmatch(r"[a-zA-Z0-9_-]+", relative.parts[0]):
            data["fixtureRoot"] = relative.parts[0]
    return data


def descendants(pid, group_root):
    pending = children(pid)
    visited = {pid}
    result = []
    while pending:
        child = pending.pop()
        if child in visited:
            continue
        visited.add(child)
        if (row := process(child, group_root)) is not None:
            result.append(row)
            pending.update(children(child))
    return sorted(result, key=lambda row: row["pid"])


def main(group_root, evidence, command):
    group_root = Path(group_root).resolve(strict=True)
    pid = os.getpid()
    if ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) != 0:
        raise OSError("could not set PR_SET_CHILD_SUBREAPER")
    with Path(evidence).open("w", encoding="utf-8", buffering=1) as log:
        def record(event, **values):
            print(json.dumps({"event": event, "monotonicNs": time.monotonic_ns(), **values}), file=log)

        boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        record("reaper", pid=pid, bootId=boot, groupRoot=str(group_root))
        env = dict(os.environ, WFC_CI_REAPER_PID=str(pid))
        code = subprocess.run(command, env=env).returncode
        record("after-runner", runnerExit=code, descendants=descendants(pid, group_root))
        deadline = time.monotonic() + 5
        forced = set()
        echild = False
        while time.monotonic() < deadline + 20:
            try:
                reaped, status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                echild = True
                break
            if reaped:
                record("reaped", pid=reaped, waitStatus=status)
            elif time.monotonic() >= deadline:
                for child in sorted(children(pid) - forced):
                    # A direct child cannot reuse its PID until we reap it.
                    identity = process(child, group_root)
                    subtree = descendants(child, group_root)
                    try:
                        os.kill(child, signal.SIGKILL)
                    except ProcessLookupError:
                        record("cleanup-race", pid=child, process=identity)
                        continue
                    record("extra-cleanup-signal", pid=child, signal=int(signal.SIGKILL),
                           process=identity, descendants=subtree)
                    forced.add(child)
            if not reaped:
                time.sleep(.02)
        record("summary", echild=echild, extraCleanupSignals=len(forced), runnerExit=code)
        print(f"Own descendants reaped: {'ECHILD' if echild else 'unconfirmed'}, "
              f"extra cleanup signals: {len(forced)}", flush=True)
    return code or int(not echild or bool(forced))


if __name__ == "__main__":
    root, log, *command = sys.argv[1:]
    sys.exit(main(root, log, command))
