"""Raw PTY fixture with scrollback, a wrapped draft and two adjacent footer lines."""
import json
import os
import re
import select
import subprocess
import sys
from pathlib import Path
import time
import textwrap
import tty


reply_worker = '--reply-worker' in sys.argv[2:]
reply_mode = sys.argv[sys.argv.index('--reply-worker') + 1] if reply_worker else None
bundled_nudge = reply_mode == 'bundled-nudge'
bundled_continue = reply_mode == 'bundled-continue'
if not reply_worker:
    while not Path('show-history').exists():
        time.sleep(.02)
tty.setraw(0)
if reply_worker:
    wfc, attempt = os.environ['WFC_BIN'], os.environ['WFC_ATTEMPT']
    if not (bundled_nudge or bundled_continue):
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
    'opencode-draft': b'keep OpenCode draft',
    'opencode-shell': b'echo untouched',
    'claude-draft': b'keep Claude draft',
}.get(reply_mode, b'')
protect_draft = reply_mode in ('ambiguous-footer', 'ambiguous-continuation')
compact = reply_mode in (
    'styled-compact-empty', 'styled-compact-draft', 'unstyled-compact-empty', 'unstyled-compact-draft',
)
prompt = {'shell': '!', 'unknown': '❯', 'ultra-empty': '»', 'ultra-draft': '»'}.get(reply_mode, '›')
disabled = reply_mode in ('disabled', 'claude-disabled')
image = reply_mode in ('codex-image', 'codex-image-unstyled')
accepted = False
submit_first = '--submit-first' in sys.argv[2:]
bundled_paste = reply_mode == 'bundled-paste' or bundled_nudge or bundled_continue
paste_begin, paste_end = b'\x1b[200~', b'\x1b[201~'
paste_frame = bytearray()
pasted_at = None
submitted = b''
next_round = None


def format_brief(text):
    width = os.get_terminal_size(0).columns - 2
    return '\r\n  '.join(textwrap.wrap(text.decode(), width)).encode()


def complete_round(name, report, target, reference=None):
    report = Path(report)
    report.write_text(f'{name} report from the original pane\n')
    args = [wfc, 'done', '--report', report, '--attempt', target, '--message-id', name]
    if reference:
        args += ['--callback-ref', reference]
    result = subprocess.run(args, capture_output=True)
    if result.returncode:
        raise SystemExit('controlled bundled completion failed')
    receipt = json.loads(result.stdout)
    receipt = {key: receipt[key] for key in ('message', 'messageId', 'state', 'statusVersion')}
    receipt.update(pid=os.getpid(), attempt=target)
    pending = Path(name + '.tmp')
    pending.write_text(json.dumps(receipt))
    pending.replace(Path(name + '.json'))


def draw():
    clear = b'\x1b[2J\x1b[H'
    reset = b'\x1b[0m'
    if reply_mode and reply_mode.startswith('opencode-'):
        body = draft.replace(b'\n', '\r\n  ┃  '.encode())
        shell = reply_mode == 'opencode-shell'
        metadata = 'Shell' if shell else 'Build · Iceman Free OpenCode'
        footer = 'esc exit shell mode' if shell else 'shift+tab agents  ctrl+p commands'
        bar = ('  ╹' + '▀' * 40).encode()
        os.write(1, clear + '  ┃\r\n  ┃  '.encode() + body + '\r\n  ┃\r\n  ┃  '.encode() +
                 metadata.encode() + b'\r\n\x1b[38;5;24m' + bar + reset +
                 b'\r\n   ' + footer.encode() + b'\r\n')
        return
    if reply_mode and reply_mode.startswith('claude-'):
        rail = ('─' * 56).encode()
        style = b'\x1b[2m' if disabled else b'\x1b[38;5;15m'
        os.write(1, clear + rail + b'\r\n' + style + '❯'.encode() + reset + b' ' + draft +
                 b'\r\n' + rail + '\r\n  ⏵⏵ auto mode\r\n'.encode())
        return
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
    body = (format_brief(draft) if bundled_continue else
            draft.replace(b' tail ', b'\n  tail ').replace(b'\n', b'\r\n'))
    if protect_draft:
        body += '\r\n  tail · value'.encode()
    elif reply_worker and not body:
        folded = reply_mode in ('compact-placeholder', 'folded-placeholder-draft')
        placeholder = b'Ask Codex to do\r\n  anything' if folded else b'Ask Codex to do anything'
        body = b'\x1b[2m' + placeholder + b'\x1b[0m'
    upstream = reply_mode in ('codex-footer', 'codex-image', 'codex-image-unstyled')
    if upstream:
        footer = '\r\n  ' + ('? for shortcuts' + ' ' * 43 if not draft else ' ' * 58) + '100% context left\r\n'
    else:
        footer = '\r\n  model · dir\r\n'
    if sys.argv[1] == 'two':
        footer += '  keys · help\r\n'
    if (reply_worker and not protect_draft and not (reply_mode == 'compact-placeholder' and draft)
            or not reply_worker and not draft and not submit_first):
        footer = '\r\n' + footer
    prompt_style = b'\x1b[2m' if disabled else b'\x1b[1m'
    background = b'\x1b[48;2;20;30;40m' if reply_mode in ('codex-footer', 'codex-image') else b''
    history = b''
    if bundled_nudge or bundled_continue:
        history = '› Initial controlled bundled assignment\r\n\r\n'.encode()
        if submitted:
            entry = format_brief(submitted) if bundled_continue else submitted
            history += '› '.encode() + entry + b'\r\n\r\n'
    if upstream:
        history = b'older transcript\r\n'
        if reply_mode != 'codex-footer':
            image_style = background if image else b''
            history += image_style + b'  [Image #1]' + reset + b'\r\n\r\n'
            if not image and not background:
                history += b'assistant response\r\n\r\n'
    os.write(1, clear + history + background + prompt_style + prompt.encode() + reset +
             b' ' + body + footer.encode())


if bundled_paste:
    os.write(1, b'\x1b[?2004h')
draw()
drop_next_enter = not submit_first or reply_mode == 'exact-reply'
while True:
    if reply_worker:
        if bundled_continue:
            if Path('finish').exists():
                break
            if Path('finish-first').exists():
                Path('finish-first').unlink()
                complete_round('first', os.environ['WFC_REPORT'], attempt)
            if next_round and Path('finish-next').exists():
                Path('finish-next').unlink()
                complete_round('second', *next_round)
        if Path('clear-draft').exists():
            Path('clear-draft').unlink()
            draft = b''
            protect_draft = False
            compact = False
            disabled = False
            image = False
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
    if bundled_paste:
        if pasted_at is None:
            if key in (b'\r', b'\n'):
                raise SystemExit('Enter arrived before the bracketed paste ended')
            paste_frame += key
            if len(paste_frame) <= len(paste_begin) and not paste_begin.startswith(paste_frame):
                raise SystemExit('reply did not start with a bracketed paste')
            if paste_frame.endswith(paste_end):
                draft = bytes(paste_frame[len(paste_begin):-len(paste_end)])
                pasted_at = time.monotonic_ns()
                draw()
            continue
        if key != b'\r' or accepted:
            raise SystemExit('unexpected bytes after the bracketed paste')
        elapsed_ns = time.monotonic_ns() - pasted_at
        # PTY reads can be scheduled later than the acknowledged write.
        # Allow 50 ms of receiver-side slack while rejecting the old 300 ms delay.
        if elapsed_ns < 450_000_000:
            raise SystemExit('Enter arrived during the paste settling window')
        Path('paste-enter-ns').write_text(f'{elapsed_ns}\n')
    if key in (b'\r', b'\n'):
        if reply_mode == 'always-drop':
            pass
        elif reply_mode == 'changed-reply':
            draft = draft.replace(b': a b', b': ab')
        elif reply_mode == 'folded-reply':
            draft = draft.replace(b': a b', b': a\n  b')
        elif reply_mode == 'exact-reply' and not accepted and drop_next_enter:
            drop_next_enter = False
        elif drop_next_enter:
            drop_next_enter = False
        else:
            if bundled_continue:
                match = re.search(r'''Run "\$WFC_BIN" done --report '([^']+)' --attempt (a-[0-9a-f]+) --callback-ref ([0-9a-f]{64})\. ''',
                                  draft.decode())
                if not draft.startswith(b'[router] New task for you (dispatch ') or not match:
                    raise SystemExit('controlled pane did not receive a complete next-round brief')
                next_round = match.groups()
            else:
                with Path('accepted' if reply_worker else 'events').open('ab') as log:
                    log.write(draft + b'\n')
            if bundled_nudge or bundled_continue:
                submitted = draft
            accepted = True
            draft = b'Ask Codex to do anything' if reply_mode == 'plain-after-send' else b''
    else:
        draft += key
    draw()
