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
    profile = Path(os.environ["CLAUDE_CONFIG_DIR"])
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
            home = root / "home"
            parent = root / "trusted"
            cwd = parent / "work tree"
            profile = home / "custom Claude profile"
            profile.mkdir(parents=True)
            cwd.mkdir(parents=True)
            state = root / "state"
            socket = root / "sock"
            # Make the hook traverse a shell-quoted, nontrivial executable path.
            wfc = root / "worker's wfc"
            shutil.copy2(WFC, wfc)
            config = home / ".claude.json"
            config.write_text(
                json.dumps({"projects": {str(parent): {"hasTrustDialogAccepted": True}}}),
                encoding="utf-8",
            )
            config.chmod(0o640)
            settings = profile / "settings.json"
            existing_hooks = b'{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}\n'
            settings.write_bytes(existing_hooks)
            launch = root / "launch.json"
            launch.write_text(json.dumps({"hosts": {"claude": {
                "command": [sys.executable, str(Path(__file__).resolve()), "--controlled-claude"],
                "env": {"CLAUDE_CONFIG_DIR": str(profile)},
            }}}), encoding="utf-8")
            task = root / "task.md"
            task.write_text("Synthetic dispatch for the SessionStart fixture.\n", encoding="utf-8")
            env = {
                "HOME": str(home), "PATH": os.environ["PATH"], "TERM": "xterm-256color",
                "WFC_STATE": str(state), "WFC_TMUX_SOCKET": str(socket),
                "WFC_CLAUDE_CONFIG": str(config),
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
                self.assertEqual(row["trustedBy"], str(parent))
                self.assertNotIn("workerCallbackHash", row)
                self.assertEqual(len(row["ignored"]), 2)
                self.assertTrue(any(item.startswith("--git:") for item in row["ignored"]))
                self.assertTrue(any(item.startswith("--network:") for item in row["ignored"]))
                result = cwd / "controlled.json"
                deadline = time.monotonic() + 5
                while not result.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                trace = json.loads(result.read_text(encoding="utf-8"))
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
                self.assertEqual(config.stat().st_mode & 0o777, 0o640)
                updated = json.loads(config.read_text(encoding="utf-8"))
                self.assertTrue(updated["projects"][str(cwd)]["hasTrustDialogAccepted"])
            finally:
                subprocess.run(tmux + ["kill-server"], env=env, capture_output=True, timeout=5)


if __name__ == "__main__":
    unittest.main()
