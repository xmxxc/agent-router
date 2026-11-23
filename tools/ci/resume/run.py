"""Run named cases from the fixed product and record their private fixture evidence."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import unittest
from uuid import UUID


def run(fixture, binary, evidence, selectors):
    fixture = Path(fixture).resolve()
    sys.argv[:] = [str(fixture), binary]
    sys.path.insert(0, str(fixture.parent))
    spec = importlib.util.spec_from_file_location("wfc_ci_fixture", fixture)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)

    loader = unittest.TestLoader()
    suite = loader.loadTestsFromNames(selectors, module)
    # Some fixtures use the instance's id field for a dispatch ID after setup.
    matches = [
        unittest.TestCase.id(test).removeprefix(spec.name + ".")
        for group in suite for test in group
    ]
    print(f"selectors={len(selectors)} matches={len(matches)} requested={json.dumps(selectors)}")
    if loader.errors or matches != selectors:
        print(f"matched_names={json.dumps(matches)} loader_errors={loader.errors}", file=sys.stderr)
        return 1

    scratch = Path(os.environ["TMPDIR"]).resolve()
    with Path(evidence).open("w", encoding="utf-8", buffering=1) as log:
        def record(event, case, **values):
            print(json.dumps({"event": event, "test": unittest.TestCase.id(case), **values}), file=log)

        if any(name.startswith("Backend.test_public_resume_") for name in selectors):
            original_host = module.Backend.resumed_host
            original_exit = module.Backend.end_resume_host
            original_rollout = module.Backend.rollout
            original_status = module.Backend.resume_status

            def rollout(case, session, brief, source="cli", **options):
                path = original_rollout(case, session, brief, source, **options)
                record("fixture-rollout", case, session=session, path=str(path),
                       source="top-level" if source == "cli" else "subagent")
                return path

            def resumed_host(case, attempt):
                host = original_host(case, attempt)
                argv = host["argv"]
                session = str(UUID(argv[-2])) if argv[0] == "resume" else None
                record(
                    "host-argv", case, dispatch=host["WFC_DISPATCH"], attempt=attempt,
                    round=int(host["WFC_ROUND"]), session=session, state=str(case.state),
                    codexHome=str(case.codex_home), socket=case.socket,
                )
                return host

            def end_host(case, dispatch, attempt):
                original_exit(case, dispatch, attempt)
                record("verified-offline", case, dispatch=dispatch, attempt=attempt)

            def resume_status(case, dispatch):
                row = original_status(case, dispatch)
                if row.get("audit"):
                    event = row["audit"][-1]
                    fields = ("id", "round", "fromAttempt", "toAttempt", "session",
                              "result", "launchResult", "inputResult", "category")
                    witness = {field: event[field] for field in fields if field in event}
                    if witness != getattr(case, "last_resume_witness", None):
                        record("resume-audit", case, dispatch=dispatch, **witness)
                        case.last_resume_witness = witness
                return row

            module.Backend.resumed_host = resumed_host
            module.Backend.end_resume_host = end_host
            module.Backend.rollout = rollout
            module.Backend.resume_status = resume_status

        class Results(unittest.TextTestResult):
            incomplete_cleanup = False

            def stopTest(self, case):
                root = getattr(case, "root", None)
                env = getattr(case, "env", {})
                socket = getattr(case, "socket", env.get("WFC_TMUX_SOCKET"))
                state = getattr(case, "state", env.get("WFC_STATE"))
                codex_home = getattr(case, "codex_home", env.get("CODEX_HOME"))
                if codex_home is None and "HOME" in env:
                    codex_home = Path(env["HOME"]) / ".codex"
                paths = [Path(p) for p in (root, socket, state, codex_home) if p is not None]
                isolated = root is not None and all(p.is_relative_to(scratch) for p in paths)
                removed = isolated and all(not os.path.lexists(p) for p in paths)
                self.incomplete_cleanup |= not removed
                record(
                    "fixture-cleanup", case, root=str(root), state=str(state),
                    codexHome=str(codex_home), socket=socket, isolated=isolated, removed=removed,
                )
                super().stopTest(case)

        result = unittest.TextTestRunner(verbosity=2, resultclass=Results).run(suite)
        count = result.testsRun
        print(
            f"executed={count} skipped={len(result.skipped)} errors={len(result.errors)} "
            f"failures={len(result.failures)} fixture_cleanup={not result.incomplete_cleanup}"
        )
        return int(
            count != len(selectors) or bool(result.skipped) or not result.wasSuccessful()
            or result.incomplete_cleanup
        )


if __name__ == "__main__":
    fixture, binary, evidence, *selectors = sys.argv[1:]
    sys.exit(run(fixture, binary, evidence, selectors))
