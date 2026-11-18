#!/usr/bin/env python3
"""Add the user-owned scrcpy override and hide shell surfaces on Linet's output.

Settings changes use the shell daemon's acknowledged settings.patch command,
the same owner API as Hub > Displays. No Ryoku JSON store is edited directly.
"""
import json
import os
from pathlib import Path
import socket
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def call(method, args):
    runtime = Path(os.environ['XDG_RUNTIME_DIR'])
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(str(runtime / 'ryoku-shell.sock'))
        payload = dict(args, id='linet-setup')
        stream.sendall(('call ' + method + ' ' + json.dumps(payload) + '\n').encode())
        reply = json.loads(stream.makefile('r').readline())
        if reply.get('ok') is not True:
            raise RuntimeError('Ryoku rejected settings.patch: ' + str(reply.get('error')))


def patch(path, value):
    call('settings.patch', {'path': path, 'value': value})


def settings():
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(str(Path(os.environ['XDG_RUNTIME_DIR']) / 'ryoku-shell.sock'))
        stream.sendall(b'subscribe settings\n')
        return json.loads(stream.makefile('r').readline())


def main():
    config = Path(os.environ.get('XDG_CONFIG_HOME', str(Path.home() / '.config')))
    user = config / 'hypr/user.lua'
    subprocess.run(['ryoku', 'owner', str(user)], check=True)
    text = user.read_text()
    begin, end = '-- LINET RYOKU BEGIN', '-- LINET RYOKU END'
    block = begin + '\n' + (ROOT / 'dist/ryoku-scrcpy.lua').read_text() + end + '\n'
    if begin in text:
        a, b = text.index(begin), text.index(end) + len(end)
        text = text[:a] + block.rstrip() + text[b:]
    else:
        text = text.rstrip() + '\n\n' + block
    user.write_text(text)
    subprocess.run(['luac', '-p', str(user)], check=True)
    subprocess.run(['ryoku', 'owner', str(config / 'ryoku/shell.json')], check=True)
    for kind in ['bar', 'widgets']:
        patch('displays.' + kind + '.LINET-PHONE', False)
    subprocess.run(['ryoku', 'wm', 'act', 'config.reload'], check=True)
    print('scrcpy override and LINET-PHONE display visibility applied')


if __name__ == '__main__':
    main()
