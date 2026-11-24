"""A synthetic Codex composer that completes two tasks from one real PTY."""
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import sys
import time
import tty


def done(report, attempt, reference=None):
    report.write_text(f'{report.name} from worker {os.getpid()}\n')
    args = [os.environ['WFC_BIN'], 'done', '--report', report, '--attempt', attempt]
    if reference:
        args.extend(['--callback-ref', reference])
    received = subprocess.check_output(args)
    temp = report.with_suffix('.tmp')
    temp.write_bytes(received)
    temp.replace(report.with_suffix('.receipt'))


def draw(draft, entries):
    body = draft or b'\x1b[2mAsk Codex to do anything\x1b[0m'
    history = ''.join(f'› {entry}\r\n\r\n' for entry in entries).encode()
    os.write(1, b'\x1b[2J\x1b[H' + older_output + history + '\x1b[1m›\x1b[0m '.encode() + body + '\r\n\r\n  model · dir\r\n'.encode())


def yield_foreground():
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    with subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'], process_group=0) as other:
        try:
            os.tcsetpgrp(0, other.pid)
            Path('foreground-pid').write_text(str(other.pid))
            while other.poll() is None and not Path('restore-foreground').exists():
                time.sleep(.02)
        finally:
            os.tcsetpgrp(0, os.getpgrp())
            other.terminate()


tty.setraw(0)
with Path('launches').open('a') as launches:
    print(os.getpid(), file=launches)
draft = b''
submitted_entries = []
older_rows = 0
if '--long-history' in sys.argv:
    older_rows = 230
elif '--over-limit-history' in sys.argv:
    older_rows = 1100
older_output = ''.join(f'Older project output {row:03d}\r\n' for row in range(older_rows)).encode()
next_task = None
draw(draft, submitted_entries)
while True:
    if Path('finish-first').exists():
        Path('finish-first').unlink()
        done(Path(os.environ['WFC_REPORT']), os.environ['WFC_ATTEMPT'])
    if Path('check-old-callback').exists():
        Path('check-old-callback').unlink()
        old = subprocess.run([os.environ['WFC_BIN'], 'done', '--report', os.environ['WFC_REPORT'],
                              '--attempt', os.environ['WFC_ATTEMPT']], capture_output=True)
        temp = Path('old-callback.tmp')
        temp.write_text(str(old.returncode))
        temp.replace('old-callback.result')
    if Path('finish-next').exists() and next_task:
        Path('finish-next').unlink()
        report, attempt, reference = next_task
        done(Path(report), attempt, reference)
    if Path('show-draft').exists():
        Path('show-draft').unlink()
        draft = b'local user draft'
        draw(draft, submitted_entries)
    if Path('yield-foreground').exists():
        Path('yield-foreground').unlink()
        yield_foreground()
    if Path('clear-draft').exists():
        Path('clear-draft').unlink()
        draft = b''
        draw(draft, submitted_entries)
    if not select.select([0], [], [], .02)[0]:
        continue
    keys = os.read(0, 4096)
    if not keys:
        break
    for key in keys:
        if key in (10, 13):
            if draft:
                text = draft.decode()
                match = re.search(r"Write the report to '([^']+)'.+--attempt (a-[0-9a-f]+) --callback-ref ([0-9a-f]+)", text)
                # Record per-turn identity without writing the private reference to disk.
                with Path('inputs').open('a') as log:
                    print(re.sub(r'--callback-ref \S+', '--callback-ref <callback>', text), file=log)
                if not Path('discard-next-input').exists():
                    if match and text.startswith('[router] New task for you '):
                        next_task = match.groups()
                        submitted_entries.append(text)
                    elif text.startswith('[router] Message from the controller: '):
                        submitted_entries.append(text)
            else:
                with Path('inputs').open('a') as log:
                    print('<extra Enter>', file=log)
            draft = b''
        else:
            draft += bytes([key])
    draw(draft, submitted_entries)
