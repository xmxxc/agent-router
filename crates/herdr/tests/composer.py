"""Raw PTY fixture with scrollback, a wrapped draft and two adjacent footer lines."""
import os
import select
import subprocess
import sys
from pathlib import Path
import time
import tty


reply_worker = '--reply-worker' in sys.argv[2:]
reply_mode = sys.argv[sys.argv.index('--reply-worker') + 1] if reply_worker else None
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
draft = {
    'ambiguous-footer': b'original draft',
    'compact-placeholder': b'AskCodextodoanything',
    'styled-compact-draft': '\n  model · dir'.encode(),
    'unstyled-compact-draft': '\n  model · dir'.encode(),
}.get(reply_mode, b'')
protect_draft = reply_worker
compact = reply_mode in (
    'styled-compact-empty', 'styled-compact-draft', 'unstyled-compact-empty', 'unstyled-compact-draft',
)
accepted = False
submit_first = '--submit-first' in sys.argv[2:]


def draw():
    if compact:
        background = b'\x1b[48;2;20;30;40m' if reply_mode.startswith('styled-') else b''
        reset = b'\x1b[0m' if background else b''
        footer = '  keys · help'
        if reply_mode.endswith('-empty'):
            footer = '  model · dir\r\n' + footer
        os.write(1, b'\x1b[2J\x1b[H' + background + '› '.encode() + draft.replace(b'\n', b'\r\n') +
                 reset + b'\r\n' + footer.encode() + b'\r\n')
        return
    body = draft.replace(b' tail ', b'\r\n  tail ')
    if protect_draft and reply_mode != 'compact-placeholder':
        body += '\r\n  tail · value'.encode()
    elif reply_worker and not body:
        body = b'Ask Codex to do\r\n  anything' if reply_mode == 'compact-placeholder' else b'Ask Codex to do anything'
    footer = '\r\n  model · dir\r\n'
    if sys.argv[1] == 'two':
        footer += '  keys · help\r\n'
    if not protect_draft and not draft and (reply_worker or not submit_first):
        footer = '\r\n' + footer
    os.write(1, '\x1b[2J\x1b[H› '.encode() + body + footer.encode())


draw()
drop_next_enter = not submit_first
while True:
    if reply_worker:
        if Path('clear-draft').exists():
            Path('clear-draft').unlink()
            draft = b''
            protect_draft = False
            compact = False
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
