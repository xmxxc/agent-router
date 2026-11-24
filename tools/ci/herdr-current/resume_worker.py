"""Model the Codex CLI's saved conversation and positional resume prompt."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import tty


def save(path, value):
    temp = path.with_suffix('.tmp')
    temp.write_text(json.dumps(value))
    temp.replace(path)


def callback(name, *args):
    response = subprocess.check_output([
        os.environ['WFC_BIN'], *args, '--attempt', os.environ['WFC_ATTEMPT'], '--message-id', name,
    ])
    save(Path(name + '.json'), json.loads(response))


tty.setraw(0)
session = '11111111-2222-4333-8444-555555555555'
argv = sys.argv[1:]
resumed = argv[0] == 'resume'
rollouts = Path(os.environ['CODEX_HOME']) / 'sessions' / '2026' / '11' / '23'
rollout = rollouts / f'rollout-fixture-{session}.jsonl'
if resumed:
    assert argv[-2] == session and rollout.is_file()
else:
    rollouts.mkdir(parents=True)
    native_records = [
        {'type': 'session_meta', 'payload': {'id': session, 'source': 'cli', 'cwd': os.getcwd()}},
        {'type': 'event_msg', 'payload': {'type': 'user_message', 'message': argv[-1]}},
    ]
    rollout.write_text(''.join(json.dumps(line) + '\n' for line in native_records))
with Path('launches.jsonl').open('a') as launches:
    print(json.dumps({'pid': os.getpid(), 'argv': argv, 'environment': {
        key: os.environ['WFC_' + key.upper()] for key in ('dispatch', 'round', 'attempt', 'task', 'report')
    }}), file=launches)
os.write(1, '› Ask Codex to do anything\r\n\r\n  model · dir\r\n'.encode())
if not resumed:
    callback('first-question', 'ask', 'Is the original task still assigned?')
    while not Path('stop-first').exists():
        time.sleep(.02)
    sys.exit(17)

while not Path('finish-resumed').exists():
    time.sleep(.02)
callback('resumed-done', 'done', '--report', os.environ['WFC_REPORT'])
while True:
    time.sleep(.1)
