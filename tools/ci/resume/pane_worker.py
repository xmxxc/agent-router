"""Complete a fixture round from the worker's own pane environment."""
import json
import os
from pathlib import Path
import re
import subprocess
import time


def proc_stat(pid):
    return Path(f"/proc/{pid}/stat").read_bytes().rsplit(b")", 1)[1].split()


def original_stopped(directory):
    source = json.loads((directory / "reverse-check").read_bytes())
    marker = json.loads(Path(source["stop"]).read_bytes())
    row = json.loads(Path(source["record"]).read_bytes())
    close, = [event for event in row["audit"] if event["operation"] == "close"]
    try:
        fields = proc_stat(source["pid"])
        alive = fields[0] not in (b"Z", b"X", b"x") and int(fields[19]) == source["startTicks"]
    except (FileNotFoundError, ProcessLookupError):
        alive = False
    return (
        not alive and marker["writerTreeStopped"] is True
        and close["result"] == "completed"
        and close["attempt"] == marker["attempt"] == source["attempt"]
    )


def save(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data))
    temporary.replace(path)


def finish(worker):
    report = Path(worker["report"])
    report.write_text(f'Round {worker["round"]} completed by its pane worker.\n')
    try:
        result = subprocess.run(
            [worker["binary"], "done", "--report", report, "--attempt", worker["attempt"]],
            capture_output=True,
            timeout=6,
        )
    except subprocess.TimeoutExpired:
        return {"exitCode": None, "receipt": None}
    receipt = None
    if result.returncode == 0:
        try:
            data = json.loads(result.stdout)
        except (ValueError, UnicodeError):
            data = None
        if (
            isinstance(data, dict)
            and type(data.get("message")) is int
            and re.fullmatch(r"m-[0-9a-f]{32}", str(data.get("messageId")))
            and data.get("state") == "unsent"
            and type(data.get("statusVersion")) is int
        ):
            receipt = {key: data[key] for key in ("message", "messageId", "state", "statusVersion")}
    return {"exitCode": result.returncode, "receipt": receipt}


def main():
    directory = Path("handoff-workers")
    worker = {
        "dispatch": os.environ["WFC_DISPATCH"],
        "round": int(os.environ["WFC_ROUND"]),
        "attempt": os.environ["WFC_ATTEMPT"],
        "scope": os.environ["WFC_SCOPE"],
        "task": os.environ["WFC_TASK"],
        "report": os.environ["WFC_REPORT"],
        "binary": os.environ["WFC_BIN"],
        "cwd": str(Path.cwd()),
        "tmuxPane": os.getenv("TMUX_PANE"),
        "herdrPane": os.getenv("HERDR_PANE_ID"),
        "pid": os.getpid(),
        "parentPid": os.getppid(),
        "startTicks": int(proc_stat("self")[19]),
    }
    if worker["tmuxPane"]:
        worker["sourceStoppedBeforeTarget"] = original_stopped(directory)
    stem = directory / worker["attempt"]
    save(stem.with_suffix(".json"), worker)

    deadline = time.monotonic() + 45
    completed = False
    while not stem.with_suffix(".quit").exists() and time.monotonic() < deadline:
        if not completed and stem.with_suffix(".finish").is_file():
            outcome = finish(worker)
            save(stem.with_suffix(".done"), outcome)
            if outcome["receipt"] is None:
                return 1
            completed = True
        time.sleep(.025)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
