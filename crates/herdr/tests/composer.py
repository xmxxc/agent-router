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
    'existing-scroll': 'original draft ↑'.encode(),
    'ambiguous-footer': b'original draft',
    'compact-placeholder': b'AskCodextodoanything',
    'plain-placeholder-draft': b'Ask Codex to do anything',
    'folded-placeholder-draft': b'Ask Codex to do\n  anything',
    'ultra-draft': b'original Ultra draft',
    'shell': b'echo untouched',
    'styled-compact-draft': '\n  model · dir'.encode(),
    'unstyled-compact-draft': '\n  model · dir'.encode(),
}.get(reply_mode, b'')
protect_draft = reply_mode in ('ambiguous-footer', 'ambiguous-continuation')
compact = reply_mode in (
    'styled-compact-empty', 'styled-compact-draft', 'unstyled-compact-empty', 'unstyled-compact-draft',
)
prompt = {'shell': '!', 'unknown': '❯', 'ultra-empty': '»', 'ultra-draft': '»'}.get(reply_mode, '›')
disabled = reply_mode == 'disabled'
accepted = False
submit_first = '--submit-first' in sys.argv[2:]


def draw():
    if compact:
        background = b'\x1b[48;2;20;30;40m' if reply_mode.startswith('styled-') else b''
        glyph = b'\x1b[1m' + '› '.encode() if background else '› '.encode()
        reset = b'\x1b[0m' if background else b''
        footer = '  keys · help'
        if reply_mode.endswith('-empty'):
            footer = '  model · dir\r\n' + footer
        os.write(1, b'\x1b[2J\x1b[H' + background + glyph + draft.replace(b'\n', b'\r\n') +
                 reset + b'\r\n' + footer.encode() + b'\r\n')
        return
    body = draft.replace(b' tail ', b'\n  tail ').replace(b'\n', b'\r\n')
    if protect_draft:
        body += '\r\n  tail · value'.encode()
    elif reply_worker and not body:
        folded = reply_mode in ('compact-placeholder', 'folded-placeholder-draft')
        placeholder = b'Ask Codex to do\r\n  anything' if folded else b'Ask Codex to do anything'
        body = b'\x1b[2m' + placeholder + b'\x1b[0m'
    footer = '\r\n  model · dir\r\n'
    if sys.argv[1] == 'two':
        footer += '  keys · help\r\n'
    if (reply_worker and not protect_draft and not (reply_mode == 'compact-placeholder' and draft)
            or not reply_worker and not draft and not submit_first):
        footer = '\r\n' + footer
    prompt_style = b'\x1b[2m' if disabled else b'\x1b[1m'
    os.write(1, b'\x1b[2J\x1b[H' + prompt_style + prompt.encode() + b'\x1b[0m ' + body + footer.encode())


draw()
drop_next_enter = not submit_first
while True:
    if reply_worker:
        if Path('clear-draft').exists():
            Path('clear-draft').unlink()
            draft = b''
            protect_draft = False
            compact = False
            disabled = False
            if prompt not in ('›', '»'):
                prompt = '›'
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
        if reply_mode == 'always-drop':
            pass
        elif drop_next_enter:
            drop_next_enter = False
        else:
            with Path('accepted' if reply_worker else 'events').open('ab') as log:
                log.write(draft + b'\n')
            accepted = True
            draft = b'Ask Codex to do anything' if reply_mode == 'plain-after-send' else b''
    else:
        draft += key
    draw()
