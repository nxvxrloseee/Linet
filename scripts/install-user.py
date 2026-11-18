#!/usr/bin/env python3
"""Idempotent desktop installation. No data/identity migrations or desktop store edits."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def install(prefix, config, data, home, binaries, target):
    bindir = prefix / 'bin'
    bindir.mkdir(parents=True, exist_ok=True)
    for name in ('linetd', 'linetctl', 'linet-nmh'):
        dest = bindir / name
        temp = bindir / (name + '.install-new')
        shutil.copy2(binaries / name, temp)
        temp.chmod(0o755)
        temp.replace(dest)
        # Existing absolute launchers and cargo's old location cannot run a stale copy.
        legacy = home / '.cargo/bin' / name
        if legacy.parent.exists() and legacy != dest:
            link = legacy.with_name(name + '.install-link')
            link.unlink(missing_ok=True)
            link.symlink_to(dest)
            link.replace(legacy)
    unit = (ROOT / 'dist/linetd.service').read_text()
    unit = unit.replace('@LINETD@', json.dumps(str(bindir / 'linetd')).replace('%', '%%'))
    unit = unit.replace('Environment=PATH=@BINDIR@:/usr/local/sbin:/usr/local/bin:/usr/bin',
                        'Environment=' + json.dumps('PATH=' + str(bindir) + ':/usr/local/sbin:/usr/local/bin:/usr/bin').replace('%', '%%'))
    unit = unit.replace('@SESSION_TARGET@', target)
    units = config / 'systemd/user'
    units.mkdir(parents=True, exist_ok=True)
    (units / 'linetd.service').write_text(unit)
    install_desktop(bindir / 'linetctl', data)
    install_browser(bindir / 'linet-nmh', config, home)


def install_desktop(ctl, data):
    ctl = ctl.resolve()
    apps = data / 'applications'
    apps.mkdir(parents=True, exist_ok=True)
    # Desktop Exec is not a shell: quote its argument syntax (including literal percent).
    executable = '"' + str(ctl).replace('\\', '\\\\').replace('"', '\\"').replace('`', '\\`').replace('$', '\\$').replace('%', '%%') + '"'
    for template in (ROOT / 'dist').glob('linet-*.desktop'):
        (apps / template.name).write_text(template.read_text().replace('@LINETCTL@', executable))
    scripts = data / 'nautilus/scripts'
    scripts.mkdir(parents=True, exist_ok=True)
    script = scripts / 'Отправить на телефон'
    script.write_text('#!/usr/bin/env bash\nexec ' + shlex.quote(str(ctl)) + ' send --notify "$@"\n')
    script.chmod(0o755)


def install_browser(host, config, home):
    host = str(host.resolve())
    base = {'name': 'org.linet', 'description': 'Linet Handoff', 'path': host, 'type': 'stdio'}
    firefox = dict(base, allowed_extensions=['linet@linet.local'])
    chrome = dict(base, allowed_origins=['chrome-extension://faccofpkddbdodnngbjiiodlbpcpbnbl/'])
    for folder, manifest in [
        (home / '.mozilla/native-messaging-hosts', firefox),
        (config / 'mozilla/native-messaging-hosts', firefox),
        (config / 'chromium/NativeMessagingHosts', chrome),
        (config / 'google-chrome/NativeMessagingHosts', chrome),
    ]:
        folder.mkdir(parents=True, exist_ok=True)
        (folder / 'org.linet.json').write_text(json.dumps(manifest, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--no-service', action='store_true', help='write integration without changing running services')
    parser.add_argument('--prefix', type=Path, default=Path.home() / '.local')
    args = parser.parse_args()
    if not args.prefix.is_absolute():
        parser.error('--prefix must be absolute')
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'linetd', '-p', 'linetctl'], cwd=ROOT, check=True)
    home = Path.home()
    config = Path(os.environ.get('XDG_CONFIG_HOME', str(home / '.config')))
    data = Path(os.environ.get('XDG_DATA_HOME', str(home / '.local/share')))
    ryoku = subprocess.run(['systemctl', '--user', 'cat', 'ryoku-session.target'], capture_output=True).returncode == 0
    target = 'ryoku-session.target' if ryoku else 'graphical-session.target'
    install(args.prefix, config, data, home, ROOT / 'target/release', target)
    if shutil.which('update-desktop-database'):
        subprocess.run(['update-desktop-database', str(data / 'applications')], check=True)
    if not args.no_service:
        subprocess.run(['systemctl', '--user', 'daemon-reload'], check=True)
        subprocess.run(['systemctl', '--user', 'enable', 'linetd.service'], check=True)
        subprocess.run(['systemctl', '--user', 'restart', 'linetd.service'], check=True)
    print('Installed Linet in', args.prefix / 'bin', '; session target:', target)


if __name__ == '__main__':
    main()
