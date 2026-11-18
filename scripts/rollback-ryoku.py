#!/usr/bin/env python3
"""Undo this integration using its private backup; never restore/delete Linet data."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tarfile

spec = importlib.util.spec_from_file_location('setup_ryoku', Path(__file__).with_name('setup-ryoku.py'))
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)


def fingerprint(path):
    if path.is_symlink():
        return {'link': os.readlink(path)}
    if path.is_file():
        with path.open('rb') as f:
            return {'sha256': hashlib.file_digest(f, 'sha256').hexdigest()}
    return None


def integration_path(home, name):
    relative = Path(name)
    if relative.is_absolute() or '..' in relative.parts or not relative.parts:
        raise ValueError('Unsafe integration path')
    path = home / relative
    # Resolve parents only: replacing a legacy symlink itself is intentional.
    if not path.parent.resolve().is_relative_to(home.resolve()):
        raise ValueError('Integration parent escapes home')
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('backup', type=Path)
    args = parser.parse_args()
    after = json.loads((args.backup / 'integration-after.json').read_text())
    home = Path.home()
    # Validate every target before stopping services or changing files.
    for name in after['files']:
        integration_path(home, name)
    # A deliberate rollback invocation authorizes removal of this installed plugin,
    # but its authoring source and every other plugin remain untouched.
    subprocess.run(['ryoku', 'plugin', 'remove', 'linet'], check=False)
    subprocess.run(['systemctl', '--user', 'disable', '--now', 'linetd.service'], check=True)
    with tarfile.open(args.backup / 'state.tar.gz') as archive:
        members = {m.name: m for m in archive.getmembers()}
        for name, expected in after['files'].items():
            path = integration_path(home, name)
            if fingerprint(path) != expected:
                print('Kept subsequently modified integration file:', path)
                continue
            member = members.get(name)
            if member is None:
                path.unlink(missing_ok=True)  # only an unchanged file created by this install
                continue
            path.parent.mkdir(parents=True, exist_ok=True)
            staged = path.with_name(path.name + '.linet-rollback')
            staged.unlink(missing_ok=True)
            if member.issym():
                staged.symlink_to(member.linkname)
            elif member.isfile():
                staged.write_bytes(archive.extractfile(member).read())
                staged.chmod(member.mode)
            else:
                raise RuntimeError('Unexpected integration archive entry: ' + name)
            staged.replace(path)

        # Remove only our managed Lua block, preserving subsequent user additions.
        lua = home / '.config/hypr/user.lua'
        current = lua.read_text()
        block = after['lua_block']
        if block in current:
            lua.write_text(current.replace(block, '', 1))

        # These stores are only changed through their owning shell daemon API.
        old = json.loads(archive.extractfile('.config/ryoku/shell.json').read())
        current_settings = setup.settings()
        for kind in ['bar', 'widgets']:
            if current_settings.get('displays', {}).get(kind, {}).get('LINET-PHONE') is not False:
                print('Kept subsequently modified LINET-PHONE setting:', kind)
                continue
            value = old.get('displays', {}).get(kind, {}).get('LINET-PHONE')
            path = 'displays.' + kind + '.LINET-PHONE'
            if value is None:
                setup.call('settings.reset', {'path': path})
            else:
                setup.patch(path, value)

    subprocess.run(['systemctl', '--user', 'daemon-reload'], check=True)
    subprocess.run(['ryoku', 'wm', 'act', 'config.reload'], check=True)
    # At the initial audited snapshot linetd was inactive; do not auto-start an
    # old Caelestia-dependent build under Ryoku. No identities/peers are restored.
    print('Integration restored; old service left inactive. Linet data and plugin source preserved.')


if __name__ == '__main__':
    main()
