#!/usr/bin/env bash
# Пункты «Отправить на телефон» в меню запуска, «Открыть с помощью» и сценариях Nautilus.
set -euo pipefail
cd "$(dirname "$0")/.."
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$apps"
LINETCTL="${LINETCTL:-$(command -v linetctl || echo "$HOME/.cargo/bin/linetctl")}"
for f in dist/linet-*.desktop; do
    sed "s|@LINETCTL@|$LINETCTL|" "$f" > "$apps/$(basename "$f")"
done
update-desktop-database "$apps" 2>/dev/null || true
echo "Меню запуска и «Открыть с помощью»: $apps/linet-*.desktop"

scripts="${XDG_DATA_HOME:-$HOME/.local/share}/nautilus/scripts"
mkdir -p "$scripts"
cat > "$scripts/Отправить на телефон" <<'SH'
#!/bin/sh
exec "$HOME/.cargo/bin/linetctl" send --notify "$@"
SH
chmod +x "$scripts/Отправить на телефон"
echo "Nautilus, ПКМ → Сценарии: $scripts/Отправить на телефон"
