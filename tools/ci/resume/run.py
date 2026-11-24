"""Run named cases from the fixed product and record their private fixture evidence."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import unittest
from uuid import UUID

from reap import descendants


def group_layout(root):
    root = Path(root).resolve(strict=True)
    layout = {}
    for field, variable, child in (
        ("home", "HOME", "home"),
        ("codexHome", "CODEX_HOME", "codex"),
        ("state", "WFC_STATE", "state"),
        ("socket", "WFC_TMUX_SOCKET", "tmux.sock"),
        ("tmp", "TMPDIR", "tmp"),
    ):
        path = root / child
        if os.environ.get(variable) != str(path) or path.resolve() != path:
            raise ValueError(f"{variable} does not match the private group path")
        layout[field] = path
    return layout


def resolved_path(path):
    if not path.is_absolute() or ".." in path.parts:
        return None
    try:
        return path.resolve()
    except (OSError, RuntimeError):
        return None


def path_is_gone(path):
    try:
        path.lstat()
    except FileNotFoundError:
        return True
    except OSError:
        pass
    return False


def cleanup_status(reported, layout):
    root = Path(reported["root"]) if reported["root"] is not None else None
    tmp = layout["tmp"]
    valid_root = (
        root is not None and root != tmp and root.is_relative_to(tmp) and resolved_path(root) == root
    )
    fixture_paths, group_paths, invalid_paths, remaining = {}, {}, {}, {}
    for field, value in reported.items():
        if value is None:
            continue
        path = Path(value)
        resolved = resolved_path(path)
        if path == layout.get(field) and resolved == path:
            # The outer shell verifies removal of its own group directory.
            group_paths[field] = str(path)
        elif (
            valid_root and path.is_relative_to(root)
            and resolved is not None and resolved.is_relative_to(root)
        ):
            fixture_paths[field] = str(path)
            if not path_is_gone(path):
                remaining[field] = str(path)
        else:
            invalid_paths[field] = str(path)
    isolated = valid_root and not invalid_paths
    return {
        "isolated": isolated, "removed": isolated and not remaining,
        "fixturePaths": fixture_paths, "groupPaths": group_paths,
        "invalidPaths": invalid_paths, "fixtureRemaining": remaining,
    }


def run(group_root, fixture, binary, evidence, selectors):
    layout = group_layout(group_root)
    reaper = int(os.environ["WFC_CI_REAPER_PID"])
    group_root = layout["tmp"].parent
    fixture = Path(fixture).resolve()
    sys.argv[:] = [str(fixture), binary]
    offline_herdr = fixture.name == "check_offline_resume.py"
    if offline_herdr:
        sys.path.insert(0, str(Path("tests").resolve(strict=True)))
    if fixture.name == "herdr_public.py" or offline_herdr:
        sys.argv.insert(1, os.environ["HERDR_BINARY"])
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
                    codexHomeEnv=host.get("CODEX_HOME"), codexHomeExisted=host.get("codexHomeExisted"),
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

        def fixture_root(case):
            if offline_herdr:
                return getattr(getattr(case, "service", None), "root", None)
            return getattr(case, "root", getattr(case, "cwd", None))

        class Results(unittest.TextTestResult):
            incomplete_cleanup = False

            def startTest(self, case):
                super().startTest(case)
                record("before-fixture", case, processes=descendants(reaper, group_root))
                cleanups = case.doCleanups

                def before_cleanups():
                    # unittest calls this after a failure too, while fixture
                    # paths and processes are still available for attribution.
                    record("before-fixture-cleanups", case,
                           root=str(fixture_root(case)),
                           processes=descendants(reaper, group_root))
                    return cleanups()

                case.doCleanups = before_cleanups

            def stopTest(self, case):
                root = fixture_root(case)
                env = getattr(case, "env", {})
                socket = getattr(case, "socket", env.get("WFC_TMUX_SOCKET", env.get("WFC_HERDR_ENDPOINT")))
                state = getattr(case, "state", env.get("WFC_STATE"))
                codex_home = getattr(case, "codex_home", env.get("CODEX_HOME"))
                home = env.get("HOME")
                if codex_home is None and home is not None:
                    codex_home = Path(home) / ".codex"
                reported = {
                    field: str(value) if value is not None else None
                    for field, value in (("root", root), ("state", state), ("home", home),
                                         ("codexHome", codex_home), ("socket", socket))
                }
                cleanup = cleanup_status(reported, layout)
                self.incomplete_cleanup |= not cleanup["removed"]
                record("fixture-cleanup", case, **reported, **cleanup)
                record("after-fixture-cleanups", case, processes=descendants(reaper, group_root))
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
    group_root, fixture, binary, evidence, *selectors = sys.argv[1:]
    sys.exit(run(group_root, fixture, binary, evidence, selectors))
