"""Run the fixed R11 fixture once and require both socket checkpoints."""

import hashlib
import json
import os
from pathlib import Path
import runpy
import stat
import subprocess
import sys
import unittest

PRODUCT = "3a091e22d5cbb00ed3a13e980811fd67ba1d6718"
CASE = "test_legacy_cancelled_reservation_requires_empty_original_endpoint"
binary = Path(sys.argv[1]).resolve()
revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
assert revision == PRODUCT, revision
subprocess.run(["git", "diff", "--exit-code", PRODUCT, "--", "tools/check_tmux.py"], check=True)
version = subprocess.check_output([binary, "--version"], text=True).strip()
assert version.endswith(f"build {PRODUCT})"), version

evidence = Path(os.environ["EVIDENCE"]) / "fixture.jsonl"


def emit(event, **fields):
    row = json.dumps(dict(event=event, **fields), sort_keys=True)
    print(row, flush=True)
    with evidence.open("a", encoding="utf-8") as stream:
        print(row, file=stream)


with binary.open("rb") as elf:
    elf_digest = hashlib.file_digest(elf, "sha256").hexdigest()
emit("input", source=revision, case=CASE, version=version, elf_sha256=elf_digest,
     fixture_sha256=hashlib.sha256(Path("tools/check_tmux.py").read_bytes()).hexdigest())
sys.argv[:] = [sys.argv[0], str(binary)]
Backend = runpy.run_path("tools/check_tmux.py", run_name="focused")["Backend"]


def recovery_scope():
    return {
        "bootId": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
        "pidNamespace": Path("/proc/self/ns/pid").stat().st_ino,
        "networkNamespace": Path("/proc/self/ns/net").stat().st_ino,
        "userNamespace": Path("/proc/self/ns/user").stat().st_ino,
        "effectiveUid": os.geteuid(),
    }


class Diagnostic(Backend):
    def setUp(self):
        super().setUp()
        self.stopped = False
        self.checkpoints = []

    def stop_server(self):
        endpoint = Path(self.socket)
        original = endpoint.lstat()
        assert stat.S_ISSOCK(original.st_mode)
        saved = self.root / "original-socket-link"
        os.link(endpoint, saved, follow_symlinks=False)
        try:
            super().stop_server()
            restored_link = not os.path.lexists(endpoint)
            if restored_link:
                os.link(saved, endpoint, follow_symlinks=False)
            assert os.path.samestat(original, endpoint.lstat())
        finally:
            saved.unlink()
        address = os.fsencode(self.socket)
        rows = Path("/proc/net/unix").read_bytes().split(b"\n")
        original_address_present = any(row.split(maxsplit=7)[7:] == [address] for row in rows)
        emit("stopped-original-endpoint", original_dev=original.st_dev,
             original_inode=original.st_ino, restored_link=restored_link,
             original_address_present=original_address_present)
        assert not original_address_present
        self.original_socket = original
        self.stopped = True

    def call(self, op, *args, **kwargs):
        capture = op == "inspect" and self.stopped and kwargs.get("legacyRecovery") == recovery_scope()
        if capture:
            endpoint = Path(self.socket)
            present = os.path.lexists(endpoint)
            if present:
                assert os.path.samestat(self.original_socket, endpoint.lstat())
        result = super().call(op, *args, **kwargs)
        if capture:
            self.checkpoints.append((present, result.get("result"), result.get("alive")))
            emit("recovery-inspect", endpoint_present=present,
                 executionId=kwargs.get("executionId"), response=result)
        return result


test = Diagnostic(CASE)
result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite([test]))
if not result.wasSuccessful():
    sys.exit(1)
expected = [(True, "uncertain", None), (False, "accepted", False), (False, "accepted", False)]
assert test.checkpoints == expected, test.checkpoints
emit("pass", checkpoints=test.checkpoints)
