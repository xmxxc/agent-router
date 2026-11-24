"""Hosted acceptance for an offline synthetic #39/#40 source and a fresh native target."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from hashlib import sha256
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time

from legacy_public import snapshot

DISPATCH = "d-00000030"
FIRST, CURRENT = "a-000000000039", "a-000000000040"
SCOPE, OTHER = "fixture-legacy", "fixture-other"
OLD, NEW, WRONG_SCOPE = "fixture-old-owner", "fixture-new-owner", "fixture-other-owner"


def digest(value):
    return sha256(value).hexdigest()


def identifier(number):
    return f"m-{number:032x}"


def build_source(root, schema, backend):
    source, work = root / "source", root / "work"
    source.mkdir()
    work.mkdir()
    task, first_report, report = (work / file for file in ("task.md", "first.md", "last.md"))
    for file in (task, first_report, report):
        file.write_text(file.name + "\n")
    written = {41: f'{{ "report" : {json.dumps(str(first_report))} }}',
               49: json.dumps({"report": str(report), "round": 2, "attempt": CURRENT,
                               "scope": SCOPE, "role": "worker", "head": None, "dirty": None}, indent=2),
               60: json.dumps({"round": 2, "attempt": CURRENT, "question": "保留未知", "options": []},
                              ensure_ascii=False),
               61: json.dumps({"round": 2, "attempt": CURRENT, "question": "remaining", "options": []})}
    database = source / "router.db"
    with closing(sqlite3.connect(database)) as db, db:
        assert db.execute("PRAGMA journal_mode=DELETE").fetchone() == ("delete",)
        db.executescript(schema.read_text())
        db.execute("INSERT INTO controller_owner VALUES (?,?,?,?,?,?)",
                   (SCOPE, "fixture controller", OLD, 7, time.time() + 400, 9))
        db.execute("INSERT INTO controller_owner VALUES (?,?,?,?,?,?)",
                   (OTHER, "fixture other", "another-old-fixture", 3, time.time() + 400, 9))
        db.execute("""INSERT INTO dispatch (id,host,backend,cwd,task,created,scope,role,current_round,current_attempt,report)
                   VALUES (?, 'codex', ?, ?, ?, 10, ?, 'worker', 2, ?, ?)""",
                   (DISPATCH, backend, str(work), str(task), SCOPE, CURRENT, str(report)))
        for number, attempt, path, raw in ((1, FIRST, first_report, written[41]), (2, CURRENT, report, written[49])):
            db.execute("""INSERT INTO dispatch_round (dispatch,round,attempt,task,report,state,result,created,completed)
                       VALUES (?,?,?,?,?,'completed',?,10,12)""", (DISPATCH, number, attempt, str(task), str(path), raw))
        for number, kind, state, version, delivered in (
            (41, "done", "received", 3, 1), (49, "done", "processed", 4, 1),
            (60, "question", "unknown", 3, 0), (61, "question", "unsent", 1, 0),
        ):
            raw = written[number]
            db.execute("""INSERT INTO message (id,dispatch,recipient,type,body,created,delivered,message_uid,
                       request_key,body_hash,state,status_version) VALUES (?,?,'controller',?,?,12,?,?,?,?,?,?)""",
                       (number, DISPATCH, kind, raw, delivered, identifier(number), f"fixture:{number}",
                        digest(raw.encode()), state, version))
        db.execute("UPDATE sqlite_sequence SET seq=90 WHERE name='message'")
    return source, work, written


def proof_for(path, database, target, *, override=None, stopped=True):
    value = {"sourceSha256": override or digest(database.read_bytes()), "writersStopped": stopped,
             "target": str(target),
             "stoppedBy": "fixture owner", "evidence": "stopped synthetic fixture with no host or relay",
             "ownerReplacements": {scope: {"tokenSha256": digest(token.encode()), "leaseUntil": time.time() + 600}
                                   for scope, token in ((SCOPE, NEW), (OTHER, WRONG_SCOPE))}}
    path.write_text(json.dumps(value))
    return path


def argv_env(binary, root, target, *args, token=None, extra=None):
    env = {"HOME": str(root / "home"), "PATH": os.defpath}
    if target is not None:
        env["WFC_STATE"] = str(target)
    if token is not None:
        env["WFC_OWNER_TOKEN"] = token
    env.update(extra or {})
    return [binary, *args], env


def run(binary, root, target, *args, token=None, extra=None):
    argv, env = argv_env(binary, root, target, *args, token=token, extra=extra)
    return subprocess.run(argv, cwd=root, env=env, text=True, capture_output=True, timeout=20)


def output(result, code=0):
    assert result.returncode == code and not result.stderr, result
    return json.loads(result.stdout)


def denied(result, diagnostic):
    assert result.returncode == 1 and not result.stdout and diagnostic in result.stderr, result


def import_args(source, proof):
    return ("migrate-legacy", "--offline-copy", source, "--stop-proof", proof)


def rejects_invalid_sources(binary, root, source, proof):
    original = snapshot(source)
    args = import_args(source, proof)
    absent = root / "never-published"
    denied(run(binary, root, None, *args), "explicit absolute WFC_STATE")
    denied(run(binary, root, "relative", *args), "absolute WFC_STATE")
    denied(run(binary, root, source, *args), "must not overlap")
    denied(run(binary, root, source / "child", *args), "must not overlap")
    denied(run(binary, root, root, *args), "must not overlap")
    alias = root / "source-alias"
    alias.symlink_to(source, target_is_directory=True)
    denied(run(binary, root, alias / "child", *args), "must not overlap")
    existing = root / "already-present"
    existing.mkdir()
    denied(run(binary, root, existing, *args), "target differs")
    existing_proof = proof_for(root / "existing-proof.json", source / "router.db", existing)
    denied(run(binary, root, existing, *import_args(source, existing_proof)), "already exists")
    assert not list(existing.iterdir())
    bad = proof_for(root / "bad-proof.json", source / "router.db", absent, override="0" * 64)
    denied(run(binary, root, absent, *import_args(source, bad)), "does not match stop-proof")
    missing = proof_for(root / "not-stopped.json", source / "router.db", absent, stopped=False)
    denied(run(binary, root, absent, *import_args(source, missing)), "writersStopped=true")
    retired = proof_for(root / "retired-owner.json", source / "router.db", absent)
    details = json.loads(retired.read_text())
    details["ownerReplacements"][SCOPE]["tokenSha256"] = digest(OLD.encode())
    retired.write_text(json.dumps(details))
    denied(run(binary, root, absent, *import_args(source, retired)), "reused replacement fingerprint")
    expired = proof_for(root / "expired-owner.json", source / "router.db", absent)
    details = json.loads(expired.read_text())
    details["ownerReplacements"][SCOPE]["leaseUntil"] = 1
    expired.write_text(json.dumps(details))
    denied(run(binary, root, absent, *import_args(source, expired)), "lease must still be active")
    for name, sql, diagnostic in (
        ("running", "UPDATE dispatch_round SET state='running' WHERE round=2", "nonterminal round"),
        ("pending", "INSERT INTO lifecycle_event(operation,result) VALUES ('resume','pending')", "unresolved lifecycle"),
        ("waiter", "INSERT INTO waiter(pid,kind,since) VALUES (5,'relay',1)", "recorded waiter or relay"),
        ("hash", "UPDATE message SET body='{}' WHERE id=41", "body hash mismatch"),
    ):
        changed = root / name
        changed.mkdir()
        database = Path(shutil.copyfile(source / "router.db", changed / "router.db"))
        with closing(sqlite3.connect(database)) as db, db:
            db.execute(sql)
        before = snapshot(changed)
        new_proof = proof_for(root / f"{name}.json", database, absent)
        denied(run(binary, root, absent, *import_args(changed, new_proof)), diagnostic)
        assert snapshot(changed) == before and not absent.exists()
    assert snapshot(source) == original and not absent.exists()


def concurrent_import(binary, root, source, proof, target):
    args = import_args(source, proof)
    with ThreadPoolExecutor(max_workers=2) as pool:
        runners = [pool.submit(run, binary, root, target, *args) for _ in range(2)]
        receipts = [output(worker.result()) for worker in runners]
    assert sorted(receipt["duplicate"] for receipt in receipts) == [False, True]
    assert all(receipt["messageHighWater"] == 90 and receipt["messages"] == 4 for receipt in receipts)
    return next(receipt for receipt in receipts if not receipt["duplicate"])


def read_and_ack(binary, root, target, work, bodies):
    initial = output(run(binary, root, target, "status", "--dispatch", DISPATCH))["dispatches"][0]
    assert (initial["legacyArchived"], initial["backendAlive"], initial["attempt"]) == (True, False, CURRENT)
    assert initial["controllerOwner"]["generation"] == 8
    assert [row["attempt"] for row in initial["rounds"]] == [FIRST, CURRENT]
    original = {m["id"]: m for m in initial["messages"]}
    assert set(original) == set(bodies)
    for number, raw in bodies.items():
        view = original[number]
        assert (view["messageId"], view["body"], view["bodyHash"]) == (identifier(number), raw, digest(raw.encode()))
    assert original[41]["report"] == str(work / "first.md")
    assert original[41]["statusVersion"] == original[60]["statusVersion"] == 3
    assert original[60]["state"] == "unknown" and original[61]["legacyDeliveryHeld"] is True
    for credential in (None, OLD, WRONG_SCOPE):
        denied(run(binary, root, target, "ack", "41", token=credential), "stale_controller_owner")
    denied(run(binary, root, target, "ack", "41", "--body", bodies[41] + "\n", token=NEW), "conflicts with its body")
    one = output(run(binary, root, target, "ack", "41", "--body", bodies[41], token=NEW))
    two = output(run(binary, root, target, "ack", identifier(41), "--body", bodies[41], token=NEW))
    assert (one["message"], one["messageId"], one["state"], one["statusVersion"], one["duplicate"]) == (
        41, identifier(41), "processed", 4, False)
    assert two == {**one, "duplicate": True}
    empty = output(run(binary, root, target, "wait", "--scope", SCOPE, "--timeout", "0", token=NEW), code=3)
    assert empty["messages"] == []
    detail = output(run(binary, root, target, "status", "--dispatch", DISPATCH))["dispatches"][0]
    assert [m for m in detail["messages"] if m["id"] in (60, 61)] == [original[60], original[61]]
    denied(run(binary, root, target, "close", DISPATCH, token=NEW), "no close target")
    denied(run(binary, root, target, "resume", DISPATCH, token=NEW), "no resumable worker")
    denied(run(binary, root, target, "done", "--report", work / "last.md", "--attempt", CURRENT, token=NEW,
               extra={"WFC_DISPATCH": DISPATCH, "WFC_ROUND": "2", "WFC_ATTEMPT": CURRENT}), "no writable worker")

    native_report = work / "new-report.md"
    native_report.write_text("native result\n")
    row = output(run(binary, root, target, "dispatch", "--scope", SCOPE, "--role", "worker", "--cwd", work,
                     "--task", work / "task.md", "--report", native_report, "--", "/bin/true", token=NEW))
    try:
        done = output(run(binary, root, target, "done", "--report", native_report, "--attempt", row["attempt"], token=NEW,
                          extra={"WFC_DISPATCH": row["dispatch"], "WFC_ROUND": "1", "WFC_ATTEMPT": row["attempt"]}))
        assert done["message"] == 91 and done["messageId"] not in [m["messageId"] for m in original.values()]
    finally:
        output(run(binary, root, target, "close", row["dispatch"], token=NEW))
    return one, done


def interruption(binary, root, source):
    library, shim = (root / name for name in ("hold.so", "hold.c"))
    shim.write_text('''#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static void checkpoint(void) {
    int marker = open(getenv("FIXTURE_CHECKPOINT"), O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (marker < 0) _exit(90);
    close(marker);
    for (;;) pause();
}
int fsync(int fd) {
    int (*sync_file)(int) = dlsym(RTLD_NEXT, "fsync");
    int rc = sync_file(fd);
    if (rc == 0 && strcmp(getenv("FIXTURE_CUT"), "backup") == 0) {
        char link[64], path[4096];
        snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
        ssize_t n = readlink(link, path, sizeof path - 1);
        if (n > 0) {
            path[n] = 0;
            if (strstr(path, "/legacy-source/router.db")) checkpoint();
        }
    }
    return rc;
}
int renameat2(int oldfd, const char *oldpath, int newfd, const char *newpath, unsigned int flags) {
    if (strcmp(getenv("FIXTURE_CUT"), "before") == 0) checkpoint();
    int (*publish)(int, const char *, int, const char *, unsigned int) = dlsym(RTLD_NEXT, "renameat2");
    int rc = publish(oldfd, oldpath, newfd, newpath, flags);
    if (rc == 0 && strcmp(getenv("FIXTURE_CUT"), "after") == 0) checkpoint();
    return rc;
}
''')
    subprocess.run(["cc", "-shared", "-fPIC", "-o", library, shim, "-ldl"], check=True, timeout=12)
    for cut in ("backup", "before", "after"):
        checkpoint = root / f"reached-{cut}"
        target = root / f"interrupted-{cut}"
        proof = proof_for(root / f"proof-{cut}.json", source / "router.db", target)
        args = import_args(source, proof)
        existing = set(root.glob(".wfc-legacy-import-*/state"))
        argv, env = argv_env(binary, root, target, *args, extra={"LD_PRELOAD": str(library),
                            "FIXTURE_CUT": cut, "FIXTURE_CHECKPOINT": str(checkpoint)})
        with subprocess.Popen(argv, cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
            try:
                deadline = time.monotonic() + 8
                while not checkpoint.exists():
                    assert child.poll() is None and time.monotonic() < deadline, f"{cut} checkpoint was not reached"
                    time.sleep(.01)
                if cut == "after":
                    assert (target / "messages.json").is_file()
                    published = snapshot(target)
                else:
                    assert not target.exists()
                    staged, = set(root.glob(".wfc-legacy-import-*/state")) - existing
                    denied(run(binary, root, staged, "owner", "acquire", "--scope", SCOPE, token=NEW),
                           "staging has no published writer")
                child.kill()
                child.communicate(timeout=5)
                assert child.returncode == -signal.SIGKILL
            finally:
                if child.poll() is None:
                    child.kill()
        retry = output(run(binary, root, target, *args))
        assert retry["duplicate"] is (cut == "after")
        if cut == "after":
            assert snapshot(target) == published
    parked = root / "abandoned-native-target"
    target.rename(parked)
    restored = root / "restored-legacy"
    restored.mkdir()
    original = source / "router.db"
    backup = parked / "legacy-source/router.db"
    recovered = Path(shutil.copyfile(backup, restored / "router.db"))
    assert digest(recovered.read_bytes()) == digest(original.read_bytes()) and not target.exists()
    status = output(run(binary, root, restored, "status", "--dispatch", DISPATCH))
    assert status["dispatches"][0]["messages"][0]["state"] == "received"
    return {"interruptionStages": ["backup", "before", "after"],
            "rollbackBackup": digest(backup.read_bytes()), "restored": digest(recovered.read_bytes())}


def fixture(binary, schema, root, version, backend):
    root.mkdir()
    (root / "home").mkdir()
    source, work, bodies = build_source(root, schema, backend)
    original = snapshot(source)
    database = source / "router.db"
    target = root / "native"
    proof = proof_for(root / "stop-proof.json", database, target)
    rejects_invalid_sources(binary, root, source, proof)
    migrated = concurrent_import(binary, root, source, proof, target)
    backup = Path(migrated["backup"])
    assert backup.stat().st_mode & 0o777 == 0o600
    assert target.stat().st_mode & 0o777 == 0o700
    assert digest(backup.read_bytes()) == original["router.db"][1]
    denied(run(binary, root, backup.parent, "owner", "acquire", "--scope", SCOPE, token=NEW), "legacy state write blocked")
    old_bytes = snapshot(target)
    ack, done = read_and_ack(binary, root, target, work, bodies)
    after_writes = snapshot(target)
    assert output(run(binary, root, target, *import_args(source, proof)))["duplicate"] is True
    assert snapshot(target) == after_writes
    evidence = interruption(binary, root, source)
    assert snapshot(source) == original
    print(json.dumps({"syntheticPythonSchema": version, "legacyBackend": backend,
                      "sourceBeforeAfter": original["router.db"][1], "backup": digest(backup.read_bytes()),
                      "nativeLedgerBefore": old_bytes["messages.json"][1],
                      "nativeLedgerAfter": after_writes["messages.json"][1],
                      "ackedOriginal": ack["messageId"], "newNumericId": done["message"], **evidence}), flush=True)


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve(strict=True)
    schema = Path(__file__).resolve().parents[1] / "crates/legacy-state/src/schema.sql"
    with tempfile.TemporaryDirectory(prefix="wfc-offline-import-") as directory:
        for variant, backend in (("39", "tmux"), ("40", "herdr")):
            fixture(binary, schema, Path(directory) / variant, variant, backend)
    assert not Path(directory).exists()
