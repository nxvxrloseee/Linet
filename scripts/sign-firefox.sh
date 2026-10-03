#!/usr/bin/env bash
# Подписывает расширение в addons.mozilla.org как unlisted (не публикуется в каталоге)
# и кладёт .xpi в dist/firefox/. Ключи API: https://addons.mozilla.org/developers/addon/api/key/
# Их нужно положить в ~/.config/linet/amo.env (chmod 600):
#   WEB_EXT_API_KEY=user:12345:678
#   WEB_EXT_API_SECRET=...
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/linet/amo.env"
[[ -f "$ENV_FILE" ]] || { echo "Нет $ENV_FILE с WEB_EXT_API_KEY и WEB_EXT_API_SECRET" >&2; exit 1; }
set -a; source "$ENV_FILE"; set +a

# Поле "key" нужно только Chromium (стабильный id), Firefox его не понимает.
src=$(mktemp -d)
trap 'rm -rf "$src"' EXIT
cp extension/* "$src/"
python3 - "$src/manifest.json" <<'PY'
import json, sys
p = sys.argv[1]
m = json.load(open(p))
m.pop("key", None)
json.dump(m, open(p, "w"), ensure_ascii=False, indent=2)
PY

npx --yes web-ext@latest sign --source-dir "$src" --channel unlisted --artifacts-dir dist/firefox
ls -1 dist/firefox/*.xpi
