"""Private Linux process identities for runtime-observation fixtures."""
import ctypes
from dataclasses import dataclass
import os
from pathlib import Path
import signal
import time


@dataclass(frozen=True)
class Process:
    pid: int
    parent: int
    group: int
    session: int
    started: int
    state: str


def process(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    except FileNotFoundError:
        return None
    return Process(pid, int(fields[1]), int(fields[2]), int(fields[3]), int(fields[19]), fields[0])


def children(parent):
    try:
        return list(map(int, Path(f"/proc/{parent}/task/{parent}/children").read_text().split()))
    except FileNotFoundError:
        return []


def named_child(parent, command):
    for pid in children(parent):
        try:
            argv = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
        except FileNotFoundError:
            continue
        if argv[1:2] == [command]:
            identity = process(pid)
            if identity and identity.parent == parent:
                return identity
    return None


def capture(caller):
    supervisor = named_child(caller.pid, b"--observe-host-owner")
    if supervisor is None:
        return None
    sample = named_child(supervisor.pid, b"--observe-host-runtime")
    if sample is None:
        return None
    members = [identity for pid in children(supervisor.pid)
               if (identity := process(pid)) and identity.group == sample.pid]
    if len(members) < 2:  # observer and its group guard
        return None
    return supervisor, sample, members


def subreaper(test):
    libc = ctypes.CDLL(None)
    old = ctypes.c_int()
    test.assertEqual(libc.prctl(37, ctypes.byref(old), 0, 0, 0), 0)
    test.assertEqual(libc.prctl(36, 1, 0, 0, 0), 0)
    test.addCleanup(libc.prctl, 36, old.value, 0, 0, 0)


def abort_and_reap(test, caller, capture, extra=()):
    supervisor, sampler, members = capture
    test.assertEqual((supervisor.parent, supervisor.group, supervisor.session),
                     (caller.pid, supervisor.pid, supervisor.pid))
    test.assertEqual((sampler.parent, sampler.group, sampler.session),
                     (supervisor.pid, sampler.pid, sampler.pid))
    for member in [*members, *extra]:
        test.assertEqual((member.group, member.session), (sampler.pid, sampler.pid))
    with_pid = os.pidfd_open(caller.pid)
    started = time.monotonic()
    try:
        signal.pidfd_send_signal(with_pid, signal.SIGKILL)
        caller.communicate(timeout=3)
        test.assertEqual(caller.returncode, -signal.SIGKILL)
    finally:
        os.close(with_pid)
    deadline = started + 3
    while True:
        current = process(supervisor.pid)
        if current and current.started == supervisor.started and current.state == "Z":
            break
        test.assertLess(time.monotonic(), deadline, "observation supervisor failed to finish")
        time.sleep(.01)
    test.assertEqual(current.parent, os.getpid(), "fixture must adopt only the supervisor")
    # The supervisor goes zombie after stop_group returned. The group members,
    # including the nested client, must already have been reaped by then.
    for member in [*members, *extra]:
        remaining = process(member.pid)
        test.assertTrue(remaining is None or remaining.started != member.started,
                        f"observation group member remains: {remaining}")
    waited, status = os.waitpid(supervisor.pid, 0)
    test.assertEqual(waited, supervisor.pid)
    test.assertEqual(os.waitstatus_to_exitcode(status), 4)
    return {"supervisorPid": supervisor.pid, "supervisorStart": supervisor.started,
            "samplerPid": sampler.pid, "samplerStart": sampler.started,
            "ownedGroupReapedBySupervisor": True, "supervisorReapedByFixture": True,
            "elapsedMs": int((time.monotonic() - started) * 1000)}
