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


def draw():
    body = draft.replace(b' tail ', b'\r\n  tail ')
    footer = '\r\n  fixture · model\r\n'
    if sys.argv[1] == 'two':
        footer += '  keys · help\r\n'
    if not draft:
        footer = '\r\n' + footer
    os.write(1, '\x1b[2J\x1b[H› '.encode() + body + footer.encode())


draw()
dropped = False
while True:
    key = os.read(0, 1)
    if key in (b'\r', b'\n'):
        if not dropped:
            dropped = True
        else:
            with Path('events').open('ab') as log:
                log.write(draft + b'\n')
            draft = b''
    else:
        draft += key
    draw()
