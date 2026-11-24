"""A local PTY worker that calls the real CLI for two questions and completion."""
import os
from pathlib import Path
import select
import subprocess
import sys
import tty


def publish(name, *args):
    receipt = subprocess.check_output([
        os.environ['WFC_BIN'], *args, '--attempt', os.environ['WFC_ATTEMPT'], '--message-id', name,
    ])
    temporary = Path(name + '.tmp')
    temporary.write_bytes(receipt)
    temporary.replace(name + '.json')


def draw(draft):
    body = draft or b'\x1b[2mAsk Codex to do anything\x1b[0m'
    os.write(1, '\x1b[2J\x1b[H\x1b[1m›\x1b[0m '.encode() + body + b'\r\n\r\n  model \xc2\xb7 dir\r\n')


tty.setraw(0)
with Path('worker-launches').open('a') as launches:
    print(os.getpid(), file=launches)

questions = int(sys.argv[1])
sequence = 1
if questions:
    publish('question-1', 'ask', 'First project question?')

draft = b''
draw(draft)
while True:
    if Path('exit-worker').exists():
        break
    if Path('finish-report').exists():
        Path('finish-report').unlink()
        report = Path(os.environ['WFC_REPORT'])
        report.write_text('ordinary project fixture complete\n')
        publish('work-complete', 'done', '--report', report)
    if not select.select([0], [], [], .02)[0]:
        continue
    key = os.read(0, 1)
    if not key:
        break
    if key in (b'\r', b'\n'):
        with Path('worker-input').open('ab') as log:
            log.write(draft + b'\n')
        draft = b''
        draw(draft)
        if sequence < questions:
            sequence += 1
            publish('question-2', 'ask', 'Second project question?')
    else:
        draft += key
        draw(draft)
