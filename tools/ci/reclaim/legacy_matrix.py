"""Run the fixed ordinary-user public fixtures and continue after a failed group."""
import contextlib
import functools
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import traceback


def main():
    if os.geteuid() == 0:
        raise SystemExit("the public legacy matrix requires an ordinary user")

    source, binary = map(lambda arg: Path(arg).resolve(), sys.argv[1:])
    spec = importlib.util.spec_from_file_location("legacy_public", source)
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)

    selectors = ("native_ledger_directory", "native_ledger_lookup_failure")
    cases = [(name, 1, functools.partial(getattr(fixture, name), binary)) for name in selectors]
    planned = len(cases)

    attempted = 0
    passed = 0
    failed_groups = 0
    for name, count, check in cases:
        stdout = io.StringIO()
        failed = False
        try:
            with contextlib.redirect_stdout(stdout):
                check()
        except Exception:
            failed = True
            failed_groups += 1
            print(f"legacy fixture {name} failed", file=sys.stderr)
            traceback.print_exc()
        transcript = stdout.getvalue()
        print(transcript, end="")
        # Each original fixture prints one success line after its assertions.
        successes = len(transcript.splitlines())
        if successes > count or (not failed and successes != count):
            raise SystemExit(f"unexpected success line count from {name}: {successes}")
        tried = min(count, successes + int(failed))
        attempted += tried
        passed += successes
        print(json.dumps(dict(group=name, planned=count, attempted=tried, passed=successes,
                              failed=failed)))
    print(json.dumps(dict(uid=os.geteuid(), planned=planned, attempted=attempted, passed=passed,
                          failed_groups=failed_groups)))
    return int(bool(failed_groups) or attempted != planned or passed != planned)


if __name__ == "__main__":
    sys.exit(main())
