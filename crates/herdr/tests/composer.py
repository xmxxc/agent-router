"""Raw PTY fixture with scrollback, a wrapped draft and two adjacent footer lines."""
import os
import sys
from pathlib import Path
import time
import tty


while not Path('show-history').exists():
    time.sleep(.02)
tty.setraw(0)
os.write(1, b'old history\r\n' * 260)
draft = b''
submit_first = '--submit-first' in sys.argv[2:]


def draw():
    body = draft.replace(b' tail ', b'\r\n  tail ')
    footer = '\r\n  model · dir\r\n'
    if sys.argv[1] == 'two':
        footer += '  keys · help\r\n'
    if not draft and not submit_first:
        footer = '\r\n' + footer
    os.write(1, '\x1b[2J\x1b[H› '.encode() + body + footer.encode())


draw()
drop_next_enter = not submit_first
while True:
    key = os.read(0, 1)
    if key in (b'\r', b'\n'):
        if drop_next_enter:
            drop_next_enter = False
        else:
            with Path('events').open('ab') as log:
                log.write(draft + b'\n')
            draft = b''
    else:
        draft += key
    draw()
