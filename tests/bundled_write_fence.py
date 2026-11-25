"""Check the bundled state-path fence on ordinary temporary directories."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def check(binary, inherited):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        home = root / "home"
        legacy = home / (".local/state/workflow-core" if inherited is None else "legacy")
        legacy.mkdir(parents=True)
        blocked = legacy / "router.db"
        original = b"fixture marker; must never be opened as SQLite"
        blocked.write_bytes(original)
        native = root / "native"
        env = {"PATH": "/usr/bin:/bin", "HOME": str(home)}
        if inherited is not None:
            env["WFC_STATE"] = inherited

        def refused(request):
            result = subprocess.run([binary, "--bundled", native], cwd=root, env=env,
                                    input=json.dumps(request), capture_output=True,
                                    text=True, check=True, timeout=5)
            reply = json.loads(result.stdout)
            assert reply["result"] == "refused", reply
            assert str(blocked) in reply["diagnostic"], reply
            assert blocked.read_bytes() == original

        refused({"op": "create", "unit": "fixture", "attempt": "a",
                 "cwd": str(root), "argv": ["/bin/true"]})
        assert not native.exists()
        unit = native / "bundled" / "fixture"
        unit.mkdir(parents=True, mode=0o700)
        unit.parent.chmod(0o700)
        (unit / "lock").touch(mode=0o600)
        refused({"op": "close", "ticket": {"unit": "fixture"}})
        assert sorted(path.name for path in unit.iterdir()) == ["lock"]
        assert sorted(path.name for path in legacy.iterdir()) == ["router.db"]
        print("BUNDLED_WRITE_FENCE_PASS", inherited if inherited is not None else "legacy-default")


if __name__ == "__main__":
    executable = Path(sys.argv[1]).resolve(strict=True)
    check(executable, None)
    check(executable, "~/legacy")
