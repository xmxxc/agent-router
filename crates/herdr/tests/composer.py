"""Raw PTY fixture with scrollback, a wrapped draft and two adjacent footer lines."""
import os
import select
import subprocess
import sys
from pathlib import Path
import time
import tty


reply_worker = '--reply-worker' in sys.argv[2:]
if not reply_worker:
    while not Path('show-history').exists():
        time.sleep(.02)
tty.setraw(0)
if reply_worker:
    wfc, attempt = os.environ['WFC_BIN'], os.environ['WFC_ATTEMPT']
    receipt = subprocess.check_output([wfc, 'ask', 'One controlled question?',
                                       '--attempt', attempt, '--message-id', 'reply-question'])
    Path('question.tmp').write_bytes(receipt)
    Path('question.tmp').rename('question.json')
else:
    os.write(1, b'old history\r\n' * 260)
draft = b'original draft' if reply_worker else b''
ambiguous = reply_worker
accepted = False
submit_first = '--submit-first' in sys.argv[2:]


def draw():
    body = draft.replace(b' tail ', b'\r\n  tail ')
    if ambiguous:
        body += '\r\n  tail · value'.encode()
    elif reply_worker and not body:
        body = b'Ask Codex to do anything'
    footer = '\r\n  model · dir\r\n'
    if sys.argv[1] == 'two':
        footer += '  keys · help\r\n'
    if not draft and (reply_worker or not submit_first):
        footer = '\r\n' + footer
    os.write(1, '\x1b[2J\x1b[H› '.encode() + body + footer.encode())


draw()
drop_next_enter = not submit_first
while True:
    if reply_worker:
        if Path('clear-draft').exists():
            Path('clear-draft').unlink()
            draft = b''
            ambiguous = False
            draw()
        if accepted and Path('finish').exists():
            Path('finish').unlink()
            report = Path(os.environ['WFC_REPORT'])
            report.write_text('original worker received the answer\n')
            subprocess.run([wfc, 'done', '--report', report, '--attempt', attempt,
                            '--message-id', 'reply-done'], check=True, stdout=subprocess.DEVNULL)
        if not select.select([0], [], [], .02)[0]:
            continue
    key = os.read(0, 1)
    if not key:
        break
    if key in (b'\r', b'\n'):
        if drop_next_enter:
            drop_next_enter = False
        else:
            with Path('accepted' if reply_worker else 'events').open('ab') as log:
                log.write(draft + b'\n')
            accepted = True
            draft = b''
    else:
        draft += key
    draw()
