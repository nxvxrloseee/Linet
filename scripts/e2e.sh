#!/usr/bin/env bash
# Сквозная проверка Linet без телефона. Запускать перед обновлением.
#
# Поднимает отдельный linetd (своя шина D-Bus, свой каталог данных и порт) и
# гоняет против него linet-sim — того же клиента, что в Android-приложении.
# Настоящий демон, телефон и буфер обмена не затрагиваются; Hyprland — только
# коротким захватом экрана для проверки VNC.
set -uo pipefail
cd "$(dirname "$0")/.."

if [[ -z "${LINET_E2E_INNER:-}" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
    echo "Сборка…"
    cargo build -q --release -p linetd -p linetctl -p linet-sim || exit 1
    # Короткий путь: в нём будут unix-сокеты, а у них ограничение длины.
    W=$(mktemp -d -p "${XDG_RUNTIME_DIR:-/tmp}" linet-e2e.XXXX)
    trap 'rm -rf "$W"' EXIT
    LINET_E2E_INNER=1 LINET_E2E_DIR="$W" dbus-run-session -- bash "$0"
    exit $?
fi

W=$LINET_E2E_DIR
B=./target/release
PORT=47397
FAILS=0
export HOME="$W/home" XDG_CONFIG_HOME="$W/home/.config"
mkdir -p "$HOME"

ok() { printf '  \033[32m✓\033[0m %s\n' "$1"; }
fail() { printf '  \033[31m✗\033[0m %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { local name=$1; shift; if "$@" >/dev/null 2>&1; then ok "$name"; else fail "$name"; fi; }
sha() { sha256sum "$1" | cut -d' ' -f1; }

DAEMON=
start_daemon() {
    "$B/linetd" --data-dir "$W/laptop" --port $PORT --no-mdns --no-clipboard --no-unlock --no-autolock \
        >>"$W/daemon.log" 2>&1 &
    DAEMON=$!
    for _ in $(seq 1 50); do "$B/linetctl" stats >/dev/null 2>&1 && return 0; sleep 0.1; done
    echo "демон не поднялся"; tail -5 "$W/daemon.log"; exit 1
}
stop_daemon() { kill "$DAEMON" 2>/dev/null; wait "$DAEMON" 2>/dev/null; }
sim() { "$B/linet-sim" --dir "$W/phone" --name e2e-phone "$@"; }

echo "Сопряжение"
start_daemon
uri=$(busctl --user call org.linet.Daemon /org/linet/Daemon org.linet.Daemon1 StartPairing | grep -oP '"\K[^"]+')
# Подтвердить сопряжение некому (в изолированной сессии нет уведомлений) — должно отклониться.
reply=$(timeout 30 "$B/linet-sim" --dir "$W/phone" pair "$uri" 2>&1)
if grep -q "сопряжение отклонено" <<<"$reply" && ! "$B/linetctl" devices | grep -q e2e-phone; then
    ok "без подтверждения на ноутбуке сопряжение отклоняется"
else
    fail "сопряжение без подтверждения не отклонено: $(tail -1 <<<"$reply")"
fi
stop_daemon
# Дальше сопрягаем напрямую: прописываем доверие в файлы обеих сторон.
python3 - "$W" "$PORT" <<'PY'
import hashlib, json, sys, time
w, port = sys.argv[1], sys.argv[2]
lc = open(f"{w}/laptop/identity.crt.der", "rb").read()
pc = open(f"{w}/phone/identity.crt.der", "rb").read()
now = int(time.time())
json.dump([{"fingerprint": hashlib.sha256(pc).hexdigest(), "name": "e2e-phone", "kind": "phone",
            "addrs": [], "paired_at": now}], open(f"{w}/laptop/peers.json", "w"))
json.dump([{"fingerprint": hashlib.sha256(lc).hexdigest(), "name": "e2e-laptop", "kind": "laptop",
            "addrs": [f"127.0.0.1:{port}"], "paired_at": now, "cert": lc.hex()}], open(f"{w}/phone/peers.json", "w"))
PY
start_daemon

echo "Связь"
check "ping" sim ping

echo "Файлы"
head -c 50M /dev/urandom >"$W/f50.bin"
sim send "$W/f50.bin" >/dev/null 2>&1
check "телефон → ноутбук, 50 МБ, sha256" test "$(sha "$W/f50.bin")" = "$(sha "$HOME/Downloads/Linet/f50.bin" 2>/dev/null)"

head -c 300M /dev/urandom >"$W/f300.bin"
timeout 0.8 "$B/linet-sim" --dir "$W/phone" send "$W/f300.bin" >/dev/null 2>&1
part=$(find "$HOME/Downloads/Linet" -name '.linet-*.part' -printf '%s\n' 2>/dev/null | head -1)
if [[ -n "$part" && "$part" -gt 0 ]]; then ok "обрыв оставил начало файла (${part} байт)"; else fail "после обрыва нет .part"; fi
sim send "$W/f300.bin" >/dev/null 2>&1
check "докачка 300 МБ, sha256 совпал" test "$(sha "$W/f300.bin")" = "$(sha "$HOME/Downloads/Linet/f300.bin" 2>/dev/null)"
check "повтор начался с места обрыва" grep -qE "входящий файл.*f300.bin.*offset=[1-9]" "$W/daemon.log"

sim run >"$W/sim-run.log" 2>&1 &
SIM=$!
for _ in $(seq 1 50); do "$B/linetctl" devices 2>/dev/null | grep -q "подключено" && break; sleep 0.1; done
head -c 20M /dev/urandom >"$W/f20.bin"
"$B/linetctl" send "$W/f20.bin" >/dev/null 2>&1
check "ноутбук → телефон, 20 МБ, sha256" test "$(sha "$W/f20.bin")" = "$(sha "$W/phone/inbox/f20.bin" 2>/dev/null)"
kill "$SIM" 2>/dev/null; wait "$SIM" 2>/dev/null

echo "Экран ноутбука (VNC)"
sim vnc --secs 3 >"$W/vnc.log" 2>&1 &
VSIM=$!
sleep 2
url=$(grep -oP 'url=\Kvnc://\S+' "$W/vnc.log")
if [[ -n "$url" ]]; then
    hostport=$(sed -E 's#vnc://([^?]+).*#\1#' <<<"$url")
    banner=$(timeout 3 python3 -c "import socket,sys; h,p=sys.argv[1].rsplit(':',1); s=socket.create_connection((h,int(p)),2); print(s.recv(12).decode().strip())" "$hostport" 2>/dev/null)
    if [[ "$banner" == RFB* ]]; then ok "wayvnc отвечает ($banner), пароль в ссылке"; else fail "wayvnc не ответил"; fi
else
    fail "ссылка VNC не выдана: $(grep -o 'error=.*' "$W/vnc.log")"
fi
wait "$VSIM" 2>/dev/null

echo "Память"
sim spam-images 40 8 >/dev/null 2>&1
sim spam-images 40 8 >/dev/null 2>&1
rss=$("$B/linetctl" stats --json | python3 -c "import json,sys; print(int(json.load(sys.stdin)['rss_kb'])//1024)")
if [[ "$rss" -lt 64 ]]; then ok "после 80 картинок по 8 МБ демон держит ${rss} МБ (< 64)"; else fail "память демона ${rss} МБ — утечка?"; fi
connects=$("$B/linetctl" stats --json | python3 -c "import json,sys; print(json.load(sys.stdin)['connects'])")
check "счётчик подключений растёт ($connects)" test "$connects" -ge 5

echo "Уборка"
stop_daemon
sleep 1
# pgrep -f нашёл бы и саму эту строку проверки — ищем по имени процесса.
check "wayvnc закрыт вместе с демоном" bash -c '! pgrep -a -x wayvnc | grep -q linet-wayvnc'
check "виртуальный монитор не остался" bash -c '! hyprctl monitors 2>/dev/null | grep -q LINET-PHONE'

echo
if [[ $FAILS -eq 0 ]]; then echo "Всё в порядке."; else echo "Провалено проверок: $FAILS. Лог демона: оставлен ниже."; tail -20 "$W/daemon.log"; fi
exit $FAILS
