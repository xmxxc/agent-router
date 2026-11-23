"""Load a unittest selector without invoking the fixture file's __main__."""
from pathlib import Path
import runpy
import sys
from types import SimpleNamespace
import unittest


def load_suite(script, binary, selector, expected):
    script = Path(script).resolve()
    sys.path.insert(0, str(script.parent))
    # The existing fixtures read the wfc path from argv[1] at import time.
    sys.argv[:] = [str(script), binary]
    fixture = SimpleNamespace(**runpy.run_path(str(script), run_name="_wfc_test"))
    loader = unittest.TestLoader()
    suite = loader.loadTestsFromName(selector, fixture)
    if loader.errors:
        raise SystemExit("\n".join(loader.errors))
    count = suite.countTestCases()
    if count != expected:
        raise SystemExit(f"{selector}: expected {expected} tests, discovered {count}")
    return suite


if __name__ == "__main__":
    script, binary, selector, expected = sys.argv[1:]
    count = int(expected)
    suite = load_suite(script, binary, selector, count)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    sys.exit(0 if result.wasSuccessful() and result.testsRun == count and not result.skipped else 1)
