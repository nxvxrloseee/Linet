#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile

spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('install-user.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

with tempfile.TemporaryDirectory(prefix='linet install ') as temp:
    root = Path(temp)
    home, config, data, prefix, binaries = [root / p for p in ['home', 'config', 'data', 'local prefix', 'built']]
    binaries.mkdir()
    (home / '.cargo/bin').mkdir(parents=True)
    for name in ['linetd', 'linetctl', 'linet-nmh']:
        (binaries / name).write_text('#!/bin/sh\nexit 0\n')
        (home / '.cargo/bin' / name).write_text('old')
    for _ in range(2):
        installer.install(prefix, config, data, home, binaries, 'ryoku-session.target')
    for name in ['linetd', 'linetctl', 'linet-nmh']:
        assert (home / '.cargo/bin' / name).resolve() == prefix / 'bin' / name
    manifest = json.loads((config / 'chromium/NativeMessagingHosts/org.linet.json').read_text())
    assert manifest['path'] == str(prefix / 'bin/linet-nmh')
    unit = (config / 'systemd/user/linetd.service').read_text()
    assert '@' not in unit
    assert 'WantedBy=ryoku-session.target' in unit
    assert str(prefix / 'bin/linetd') in unit
    desktop = (data / 'applications/linet-send.desktop').read_text()
    assert 'Exec="' + str(prefix / 'bin/linetctl') + '" send --notify %F' in desktop
    nautilus = (data / 'nautilus/scripts/Отправить на телефон').read_text()
    assert '"$@"' in nautilus and '.cargo/bin' not in nautilus
print('Installation: idempotent, XDG, spaces, legacy links and native manifests OK')
