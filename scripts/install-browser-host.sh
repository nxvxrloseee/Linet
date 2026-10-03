#!/usr/bin/env bash
# Регистрирует linet-nmh как native messaging host для Firefox и Chrome/Chromium.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST_BIN="${HOST_BIN:-$(command -v linet-nmh || echo "$HOME/.cargo/bin/linet-nmh")}"
[[ -x "$HOST_BIN" ]] || { echo "linet-nmh не найден: cargo install --path crates/linetctl" >&2; exit 1; }

FIREFOX_ID="linet@linet.local"
# Id распакованного расширения в Chromium определяется полем "key" в manifest.json.
CHROME_ID="faccofpkddbdodnngbjiiodlbpcpbnbl"

firefox_manifest() {
    cat <<JSON
{
  "name": "org.linet",
  "description": "Linet Handoff",
  "path": "$HOST_BIN",
  "type": "stdio",
  "allowed_extensions": ["$FIREFOX_ID"]
}
JSON
}

chrome_manifest() {
    cat <<JSON
{
  "name": "org.linet",
  "description": "Linet Handoff",
  "path": "$HOST_BIN",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://$CHROME_ID/"]
}
JSON
}

for dir in "$HOME/.mozilla/native-messaging-hosts" "${XDG_CONFIG_HOME:-$HOME/.config}/mozilla/native-messaging-hosts"; do
    mkdir -p "$dir" && firefox_manifest > "$dir/org.linet.json" && echo "Firefox:  $dir/org.linet.json"
done
for dir in "${XDG_CONFIG_HOME:-$HOME/.config}/chromium/NativeMessagingHosts" \
           "${XDG_CONFIG_HOME:-$HOME/.config}/google-chrome/NativeMessagingHosts"; do
    mkdir -p "$dir" && chrome_manifest > "$dir/org.linet.json" && echo "Chromium: $dir/org.linet.json"
done
