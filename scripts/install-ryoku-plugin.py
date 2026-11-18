#!/usr/bin/env python3
"""Install/update the authoring plugin through Ryoku, preserving its settings/place."""
import argparse
import json
import os
from pathlib import Path
import subprocess


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    args = parser.parse_args()
    source = args.source.resolve()
    print(run('ryoku', 'plugin', 'validate', str(source)), end='')
    installed = next((p for p in json.loads(run('ryoku', 'plugin', 'list', '--json')) if p['id'] == 'linet'), None)
    manifest = json.loads((source / 'manifest.json').read_text())
    paths = ['manifest.json', 'README.md', 'LICENSE', *manifest['entryPoints'].values(), *manifest.get('files', [])]
    if installed and all((Path(installed['dir']) / name).is_file() and
                         (source / name).read_bytes() == (Path(installed['dir']) / name).read_bytes() for name in paths):
        print('Linet plugin already up to date')
        return
    placement = None
    settings = {}
    if installed:
        config = Path(os.environ.get('XDG_CONFIG_HOME', str(Path.home() / '.config')))
        # Read only; the registry's sole writer remains ryoku-plugins-place.
        settings = json.loads((config / 'ryoku/plugins.json').read_text()).get('linet', {})
        placement = next((p for p in json.loads(run('ryoku-shell', 'bar', 'list', '--json')) if p['id'] == 'linet'), None)
        print(run('ryoku', 'plugin', 'remove', 'linet'), end='')
    print(run('ryoku', 'plugin', 'add', str(source), '--bar', '--yes'), end='')
    if installed:
        run('ryoku-plugins-place', 'linet', 'settings', json.dumps(settings.get('settings', {})))
        run('ryoku-plugins-place', 'linet', 'enabled', str(settings.get('enabled', True)).lower())
        if placement:
            run('ryoku-shell', 'bar', 'move', 'linet', '--section', placement['section'], '--index', str(placement['index']))


if __name__ == '__main__':
    main()
