"""Exercise a real isolated tmux launch and the Claude SessionStart command."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


def controlled_claude():
    argv = sys.argv[2:]

    def value(flag):
        return argv[argv.index(flag) + 1]

    session = value("--session-id")
    plugin = Path(value("--plugin-dir"))
    hooks = json.loads((plugin / "hooks/hooks.json").read_text(encoding="utf-8"))
    [start] = hooks["hooks"]["SessionStart"]
    assert start["matcher"] == "startup"
    [hook] = start["hooks"]
    cwd = Path.cwd()
    profile = Path(os.environ.get("CLAUDE_CONFIG_DIR", Path(os.environ["HOME"]) / ".claude"))
    transcript = profile / "projects" / "controlled-path" / (session + ".jsonl")
    event = {
        "hook_event_name": "SessionStart",
        "source": "startup",
        "session_id": session,
        "cwd": str(cwd),
        "transcript_path": str(transcript),
        "test_field": "raw event should not be stored",
    }
    command = ["/bin/sh", "-c", hook["command"]]
    rejected = subprocess.run(
        command, input=json.dumps({**event, "source": "resume"}),
        text=True, capture_output=True, timeout=5,
    )
    accepted = subprocess.run(
        command, input=json.dumps(event),
        text=True, capture_output=True, timeout=5,
    )
    result = {
        "argv": argv,
        "home": os.environ["HOME"],
        "profile": str(profile),
        "rejected": rejected.returncode,
        "accepted": accepted.returncode,
        "hook_stdout": rejected.stdout + accepted.stdout,
        "hook_stderr": rejected.stderr + accepted.stderr,
    }
    (cwd / "controlled.json.tmp").write_text(json.dumps(result), encoding="utf-8")
    (cwd / "controlled.json.tmp").rename(cwd / "controlled.json")
    time.sleep(30)  # Keep the pane alive until the test obtains its launch receipt.


if sys.argv[1:2] == ["--controlled-claude"]:
    controlled_claude()
    sys.exit(0)


WFC = Path(sys.argv.pop(1)).resolve()


class ClaudeDispatch(unittest.TestCase):
    def test_public_first_launch_keeps_user_hooks_and_binds_private_adoption(self):
        with tempfile.TemporaryDirectory(prefix="wfc-claude-tmux-") as tmp:
            root = Path(tmp).resolve()
            router_home = root / "router home"
            worker_home = root / "worker home"
            parent = root / "trusted"
            cwd = parent / "work tree"
            profile = worker_home / "custom Claude profile"
            profile.mkdir(parents=True)
            router_home.mkdir()
            cwd.mkdir(parents=True)
            state = root / "state"
            socket = root / "sock"
            # Make the hook traverse a shell-quoted, nontrivial executable path.
            wfc = root / "worker's wfc"
            shutil.copy2(WFC, wfc)
            router_config = router_home / ".claude.json"
            worker_config = worker_home / ".claude.json"
            profile_config = profile / ".claude.json"
            configs = {
                path: json.dumps({"domain": name, "projects": {
                    str(parent): {"hasTrustDialogAccepted": True},
                }}).encode()
                for name, path in [("router", router_config), ("worker", worker_config),
                                   ("profile", profile_config)]
            }
            for path, data in configs.items():
                path.write_bytes(data)
                path.chmod(0o640)
            settings = profile / "settings.json"
            existing_hooks = b'{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}\n'
            settings.write_bytes(existing_hooks)
            launch = root / "launch.json"
            launch.write_text(json.dumps({"hosts": {"claude": {
                "command": [sys.executable, str(Path(__file__).resolve()), "--controlled-claude"],
                "env": {"HOME": str(worker_home), "CLAUDE_CONFIG_DIR": str(profile)},
            }}}), encoding="utf-8")
            task = root / "task.md"
            task.write_text("Synthetic dispatch for the SessionStart fixture.\n", encoding="utf-8")
            env = {
                "HOME": str(router_home), "PATH": os.environ["PATH"], "TERM": "xterm-256color",
                "WFC_STATE": str(state), "WFC_TMUX_SOCKET": str(socket),
                "WFC_CLAUDE_CONFIG": str(router_config),
            }
            tmux = ["tmux", "-f", "/dev/null", "-S", str(socket)]
            try:
                subprocess.run(
                    tmux + ["new-session", "-d", "-s", "fixture", "/bin/cat"],
                    env=env, check=True, capture_output=True, timeout=5,
                )
                dispatch = subprocess.run(
                    [wfc, "dispatch", "--host", "claude", "--backend", "tmux",
                     "--cwd", cwd, "--task", task, "--report", root / "out/report.md",
                     "--model", "sonnet", "--effort", "high", "--label", "Claude worker",
                     "--git", "--network", "--launch-config", launch],
                    env=env, capture_output=True, text=True, timeout=15,
                )
                self.assertEqual(dispatch.returncode, 0, dispatch.stderr)
                row = json.loads(dispatch.stdout)
                self.assertNotIn("trustedBy", row)
                self.assertNotIn("workerCallbackHash", row)
                self.assertEqual(len(row["ignored"]), 2)
                self.assertTrue(any(item.startswith("--git:") for item in row["ignored"]))
                self.assertTrue(any(item.startswith("--network:") for item in row["ignored"]))
                result = cwd / "controlled.json"
                deadline = time.monotonic() + 5
                while not result.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                trace = json.loads(result.read_text(encoding="utf-8"))
                self.assertEqual(trace["home"], str(worker_home))
                self.assertEqual(trace["profile"], str(profile))
                self.assertEqual(trace["accepted"], 0, trace["hook_stderr"])
                self.assertNotEqual(trace["rejected"], 0)
                self.assertEqual(trace["hook_stdout"], "")
                self.assertTrue(trace["argv"][0].startswith("You are a worker dispatched "))
                for flag, value in [
                    ("--session-id", row["hostSession"]), ("--model", "sonnet"),
                    ("--effort", "high"), ("--name", "Claude worker"),
                    ("--permission-mode", "auto"),
                ]:
                    self.assertEqual(trace["argv"][trace["argv"].index(flag) + 1], value)
                self.assertNotIn("--resume", trace["argv"])
                evidence = state / "claude-evidence" / f'{row["dispatch"]}.{row["attempt"]}'
                path = evidence / "adopted.json"
                receipt = json.loads(path.read_text(encoding="utf-8"))
                self.assertEqual(receipt["identity"], {
                    "attempt": row["attempt"], "cwd": str(cwd), "dispatch": row["dispatch"],
                    "session": row["hostSession"], "source": "startup",
                })
                self.assertEqual(set(receipt), {"identity", "transcript"})
                self.assertEqual(evidence.stat().st_mode & 0o777, 0o700)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                self.assertEqual(settings.read_bytes(), existing_hooks)
                for path, data in configs.items():
                    self.assertEqual(path.read_bytes(), data)
                    self.assertEqual(path.stat().st_mode & 0o777, 0o640)
                    self.assertFalse(path.with_name(f".{path.name}.wfc.lock").exists())
            finally:
                subprocess.run(tmux + ["kill-server"], env=env, capture_output=True, timeout=5)

    def test_launcher_wrappers_leave_trust_to_claude(self):
        for mode in ("command-rewrites", "runtime-forwards", "path-shim-rewrites"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(prefix="wfc-claude-wrapper-") as tmp:
                self.check_wrapper_trust(Path(tmp).resolve(), mode)

    def check_wrapper_trust(self, root, mode):
        router = root / "router-home"
        worker = root / "worker-home"
        profile = root / "wrapper-profile"
        cwd = root / "trusted-parent" / "worktree"
        wrapper = root / "claude"
        task = root / "task.md"
        task.write_text("Synthetic Claude wrapper dispatch.\n", encoding="utf-8")
        for directory in (router / ".claude", worker / ".claude", profile, cwd):
            directory.mkdir(parents=True)
        configs = {}
        hooks = b'{"hooks":{"SessionStart":[]}}\n'
        for name, home, settings_dir in [
            ("router", router, router / ".claude"),
            ("worker", worker, worker / ".claude"),
            ("profile", profile, profile),
        ]:
            config = home / ".claude.json"
            config_bytes = json.dumps({"domain": name, "projects": {
                str(cwd.parent): {"hasTrustDialogAccepted": True},
            }}).encode()
            settings = settings_dir / "settings.json"
            for path, data, permissions in [(config, config_bytes, 0o640), (settings, hooks, 0o600)]:
                path.write_bytes(data)
                path.chmod(permissions)
                configs[path] = (data, permissions)

        forward = mode == "runtime-forwards"
        rewrite = 'export HOME="$R19_WORKER_HOME"\nexport CLAUDE_CONFIG_DIR="$R19_PROFILE"\n'
        command = [sys.executable, str(Path(__file__).resolve()), "--controlled-claude"]
        script = "#!/bin/sh\n"
        selection = {}
        if forward:
            script += 'exec "$@"\n'
            selection = {"runtime": [str(wrapper)], "command": command, "env": {"HOME": str(worker)}}
        elif mode == "command-rewrites":
            script += rewrite + 'exec "$@"\n'
            selection = {"command": [str(wrapper), *command]}
        else:
            script += rewrite + 'exec "$R19_PYTHON" "$R19_FIXTURE" --controlled-claude "$@"\n'
        wrapper.write_text(script, encoding="utf-8")
        wrapper.chmod(0o700)

        launch = root / "launch.json"
        launch.write_text(json.dumps({"hosts": {"claude": selection}}), encoding="utf-8")
        socket = root / "sock"
        state = root / "state"
        env = {
            "HOME": str(router), "PATH": os.pathsep.join([str(root), os.environ["PATH"]]),
            "TERM": "xterm-256color", "WFC_STATE": str(state), "WFC_TMUX_SOCKET": str(socket),
            # This file contains a trusted parent and would otherwise be eligible.
            "WFC_CLAUDE_CONFIG": str((worker if forward else router) / ".claude.json"),
            "R19_WORKER_HOME": str(worker), "R19_PROFILE": str(profile),
            "R19_PYTHON": command[0], "R19_FIXTURE": command[1],
        }
        tmux = ["tmux", "-f", "/dev/null", "-S", str(socket)]
        try:
            subprocess.run(tmux + ["new-session", "-d", "-s", "fixture", "/bin/cat"],
                           env=env, check=True, capture_output=True, timeout=5)
            dispatch = subprocess.run(
                [WFC, "dispatch", "--host", "claude", "--backend", "tmux", "--cwd", cwd,
                 "--task", task, "--report", root / "report.md", "--launch-config", launch],
                env=env, text=True, capture_output=True, timeout=15,
            )
            self.assertEqual(dispatch.returncode, 0, dispatch.stderr)
            row = json.loads(dispatch.stdout)
            self.assertNotIn("trustedBy", row)
            result = cwd / "controlled.json"
            deadline = time.monotonic() + 5
            while not result.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            trace = json.loads(result.read_text(encoding="utf-8"))
            self.assertEqual(trace["home"], str(worker))
            self.assertEqual(trace["profile"], str(worker / ".claude" if forward else profile))
            self.assertNotEqual(trace["rejected"], 0)
            self.assertEqual(trace["accepted"] == 0, forward, trace["hook_stderr"])
            evidence = state / "claude-evidence" / f'{row["dispatch"]}.{row["attempt"]}'
            self.assertEqual((evidence / "adopted.json").exists(), forward)
            for path, (data, permissions) in configs.items():
                self.assertEqual(path.read_bytes(), data)
                self.assertEqual(path.stat().st_mode & 0o777, permissions)
            self.assertEqual(list(root.rglob("*.wfc.lock")), [])
        finally:
            subprocess.run(tmux + ["kill-server"], env=env, capture_output=True, timeout=5)


if __name__ == "__main__":
    unittest.main()
