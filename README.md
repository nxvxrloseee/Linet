# Linet

Связь Android-смартфона и Linux-ноутбука в духе экосистемы Apple. Дизайн: [docs/DESIGN.md](docs/DESIGN.md).

Готово:
- сопряжение по QR-коду, защищённое соединение (QUIC + mTLS с закреплёнными
  сертификатами), автопереподключение;
- файлы в обе стороны: `linetctl send` на ноутбуке, «Поделиться → Отправить на
  ноутбук» на телефоне. Принятое лежит в `~/Downloads/Linet` и «Загрузки/Linet»;
- уведомления телефона на рабочем столе: кнопки, ответ (через fuzzel/rofi/zenity),
  смахивание синхронизируется в обе стороны;
- буфер обмена (текст): с ноутбука — автоматически, с телефона — плиткой
  «Буфер → ноутбук», через «Поделиться» или кнопкой в приложении;
- Handoff: страница из браузера на телефон (расширение `extension/`, Ctrl+Alt+P
  или `linetctl open <url>`), ссылка с телефона на ноутбук через «Поделиться»;
- разблокировка экрана телефоном при открытии крышки, выходе из сна или по
  `linetctl unlock`: без касаний, если телефон в руке и разблокирован, иначе —
  подтверждение на телефоне.

## Состав

| Путь | Что это |
|---|---|
| `crates/linet-core` | Протокол, криптография, транспорт. Общий для ноутбука и телефона |
| `crates/linetd` | Демон на ноутбуке, D-Bus API `org.linet.Daemon` |
| `crates/linetctl` | CLI и модуль для waybar |
| `crates/linet-ffi` | Обёртка ядра для Android (uniffi) |
| `crates/linet-sim` | Симулятор телефона для проверки без телефона |
| `android/` | Android-приложение (Kotlin, Compose) |

## Ноутбук

```sh
cargo install --path crates/linetd
cargo install --path crates/linetctl
mkdir -p ~/.config/systemd/user && cp dist/linetd.service ~/.config/systemd/user/
systemctl --user enable --now linetd
```

Если включён файрвол, открой UDP-порт `47321` и mDNS (UDP `5353`) для локальной сети.

```sh
linetctl pair        # QR-код для телефона
linetctl devices
linetctl ping <id>   # id можно сокращать до префикса
linetctl unpair <id>
linetctl send файл1 файл2   # на подключённый телефон
```

Модуль для waybar:

```json
"custom/linet": {
    "exec": "linetctl status",
    "return-type": "json",
    "interval": 30,
    "format": "{}"
}
```

## Телефон

Нужны Android SDK + NDK, `cargo-ndk`, rustup с целью `aarch64-linux-android` и JDK 17.

```sh
scripts/build-android-lib.sh                  # ядро на Rust → jniLibs + Kotlin-биндинги
cd android && JAVA_HOME=/usr/lib/jvm/java-17-openjdk ./gradlew installDebug
```

На Xiaomi после установки: в приложении нажать «Снять ограничение» (оптимизация
батареи), включить «Автозапуск», в настройках приложения выбрать «Контроль
активности → Нет ограничений» и закрепить Linet в недавних приложениях.

## Браузер (Handoff)

```sh
cargo install --path crates/linetctl     # ставит linetctl и linet-nmh
scripts/install-browser-host.sh          # регистрирует native messaging host
scripts/sign-firefox.sh                  # подпись unlisted, ключи в ~/.config/linet/amo.env
```

Firefox: `about:addons` → «Установить дополнение из файла» → `dist/firefox/*.xpi`.
Chromium/Chrome: `chrome://extensions` → режим разработчика → «Загрузить
распакованное» → `extension/`.

## Проверка без телефона

```sh
linetctl pair --uri                       # скопировать URI
cargo run -p linet-sim -- pair '<URI>'
cargo run -p linet-sim -- run
```
