"""Run the unchanged R9 private regression and persist narrowly scoped evidence."""

import hashlib
import json
import os
from pathlib import Path
import runpy
import stat
import subprocess
import sys
import time
import unittest


PRODUCT = "3deb4a3e215feb16c94a9bdd1b61f8166aef822c"
ORIGINAL_ELF = "9b19e0d750cdb1a7612cb1cd96a249c9f9dfad89ef85cf919654ddb0928ca5c1"
CASE = "test_legacy_cancelled_reservation_requires_empty_original_endpoint"
binary = Path(sys.argv[1]).resolve()
revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
assert revision == PRODUCT, revision
subprocess.run(["git", "diff", "--exit-code", PRODUCT, "--", "tools/check_tmux.py"], check=True)
version = subprocess.check_output([binary, "--version"], text=True).strip()
assert version.endswith(f"build {PRODUCT})"), version
with binary.open("rb") as elf:
    digest = hashlib.file_digest(elf, "sha256").hexdigest()

evidence = Path(os.environ["EVIDENCE"]) / "fixture.jsonl"


def emit(event, **fields):
    record = json.dumps(dict(event=event, monotonic_ns=time.monotonic_ns(), **fields), sort_keys=True)
    print(record, flush=True)
    with evidence.open("a", encoding="utf-8") as output:
        print(record, file=output)


emit("fixture-input", product=revision, case=CASE, binary_sha256=digest,
     original_runner_elf=digest == ORIGINAL_ELF, version=version,
     fixture_sha256=hashlib.sha256(Path("tools/check_tmux.py").read_bytes()).hexdigest())
sys.argv[:] = [sys.argv[0], str(binary)]
Backend = runpy.run_path("tools/check_tmux.py", run_name="focused")["Backend"]


def process_stat(pid):
    if pid is None:
        return None
    try:
        data = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
        return dict(pid=pid, state=data[0], ppid=int(data[1]), start_ticks=int(data[19]))
    except OSError as error:
        return dict(pid=pid, errno=error.errno)


def recovery_scope():
    return {
        "bootId": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
        "pidNamespace": Path("/proc/self/ns/pid").stat().st_ino,
        "networkNamespace": Path("/proc/self/ns/net").stat().st_ino,
        "userNamespace": Path("/proc/self/ns/user").stat().st_ino,
        "effectiveUid": os.geteuid(),
    }


def endpoint_evidence(fixture, recovery):
    endpoint = Path(fixture.socket)
    try:
        mode = endpoint.lstat().st_mode
        path_state = dict(exists=True, socket=stat.S_ISSOCK(mode), symlink=stat.S_ISLNK(mode))
    except OSError as error:
        path_state = dict(exists=False, errno=error.errno)
    try:
        parent = str(endpoint.parent.resolve(strict=True))
    except OSError as error:
        parent = dict(errno=error.errno)
    tmux_servers, pending_clients, errors = [], [], []
    expected_launches = {b"af" * 16, b"bf" * 16}
    for proc in Path("/proc").iterdir():
        if not proc.name.isdecimal():
            continue
        try:
            if proc.stat().st_uid != os.geteuid():
                continue
            argv = (proc / "cmdline").read_bytes()
            if not argv:
                continue
            comm = (proc / "comm").read_bytes()
            if argv.startswith(b"tmux: server") or comm.startswith(b"tmux: server"):
                tmux_servers.append(process_stat(int(proc.name)))
            if any(arg in expected_launches or arg.removeprefix(b"WFC_TMUX_LAUNCH=") in expected_launches
                   for arg in argv.split(b"\0")):
                pending_clients.append(process_stat(int(proc.name)))
        except FileNotFoundError:
            continue
        except OSError as error:
            errors.append([int(proc.name), error.errno])
    try:
        rows = [
            row for row in Path("/proc/net/unix").read_text().splitlines()
            if row.split(maxsplit=7)[7:] == [fixture.socket]
        ]
    except OSError as error:
        rows = dict(errno=error.errno)
    records = {}
    paths = [fixture.state / "tmux/d-abc12345.json"]
    for launch in ("af" * 16, "bf" * 16):
        for kind in ("json", "submission", "spec", "held", "intent", "session", "server", "cleanup", "revoked"):
            path = fixture.state / "launches" / f"{launch}.{kind}"
            if kind in ("spec", "held"):
                records[path.name] = dict(exists=os.path.lexists(path))
            else:
                paths.append(path)
    for path in paths:
        try:
            records[str(path.relative_to(fixture.state))] = json.loads(path.read_bytes())
        except OSError as error:
            records[str(path.relative_to(fixture.state))] = dict(errno=error.errno)
        except json.JSONDecodeError:
            records[str(path.relative_to(fixture.state))] = dict(invalid_json=True)
    return dict(socket=str(endpoint), endpoint=path_state, resolved_parent=parent,
                scope_matches={key: recovery.get(key) == value for key, value in recovery_scope().items()},
                stopped_server=process_stat(fixture.original_pid), tmux_servers=tmux_servers,
                pending_launch_clients=pending_clients, process_read_errors=errors,
                original_socket_rows=rows, records=records)


class Diagnostics(Backend):
    original_pid = None
    server_stopped = False

    def stop_server(self):
        self.original_pid = int(subprocess.check_output(
            self.tmux + ["display-message", "-p", "#{pid}"], env=self.env, timeout=10))
        super().stop_server()
        self.server_stopped = True

    def call(self, op, *args, **kwargs):
        recovery = kwargs.get("legacyRecovery")
        capture = op == "inspect" and self.server_stopped and recovery == recovery_scope()
        if capture:
            emit("before-recovery-inspect", **endpoint_evidence(self, recovery))
        result = super().call(op, *args, **kwargs)
        if op == "inspect":
            emit("inspect-reply", fields=list(kwargs), after_stop=self.server_stopped, response=result)
            if capture:
                emit("after-recovery-inspect", **endpoint_evidence(self, recovery))
        return result


result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite([Diagnostics(CASE)]))
sys.exit(not result.wasSuccessful())
