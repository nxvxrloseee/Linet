# Linet

Связь Android-смартфона и Linux-ноутбука в духе экосистемы Apple. Как устроено — в
[docs/DESIGN.md](docs/DESIGN.md).

- файлы в обе стороны, с докачкой после обрыва;
- уведомления телефона на ноутбуке: кнопки, ответ, смахивание; фильтр по приложениям;
- общий буфер обмена: текст и картинки;
- Handoff: страница из браузера на телефон и обратно;
- разблокировка ноутбука телефоном и автоблокировка, когда телефон ушёл;
- экран телефона на ноутбуке (scrcpy) и экран ноутбука или второй монитор на телефоне (wayvnc);
- заряд телефона в трее, обновление приложения по сети.

## Состав

| Путь | Что это |
|---|---|
| `crates/linet-core` | Протокол, криптография, транспорт. Общий для ноутбука и телефона |
| `crates/linetd` | Демон на ноутбуке, D-Bus API `org.linet.Daemon` |
| `crates/linetctl` | CLI; `linet-nmh` — host для браузерного расширения |
| `crates/linet-ffi` | Обёртка ядра для Android (uniffi) |
| `crates/linet-sim` | Симулятор телефона для проверок |
| `android/` | Android-приложение (Kotlin, Compose) |
| `extension/` | Расширение для Firefox/Chromium |

## Ноутбук

Нужны: `wl-clipboard`, `fuzzel`, `zenity`, `scrcpy`, `wayvnc`, `android-tools`.

```sh
cargo install --path crates/linetd
cargo install --path crates/linetctl      # linetctl и linet-nmh
mkdir -p ~/.config/systemd/user && cp dist/linetd.service ~/.config/systemd/user/
systemctl --user daemon-reload
scripts/install-desktop.sh                # пункты меню запуска и «Открыть с помощью»
```

Демону нужно окружение Wayland, поэтому его запускает Hyprland: в автозапуск добавить
`systemctl --user restart linetd`. Логи: `journalctl --user -u linetd -f`.
Если включён файрвол: UDP 47321 (Linet), UDP 5353 (mDNS), TCP 5910 (VNC).

```sh
linetctl pair                 # QR-код для телефона, затем «Сопрячь» в уведомлении
linetctl devices              # устройства и заряд
linetctl send файл…           # на телефон; --pick — выбрать в окне
linetctl screen [--pick-app]  # экран телефона или приложение с него
linetctl open <url>           # открыть страницу на телефоне
linetctl update-phone         # обновить приложение на телефоне
linetctl stats                # подключения, трафик, память, разряд телефона
linetctl unlock               # разблокировать экран телефоном
```

## Телефон

Сборка: Android SDK + NDK, `cargo-ndk`, rustup с `aarch64-linux-android`, JDK 17.

```sh
scripts/build-android-lib.sh
cd android && JAVA_HOME=/usr/lib/jvm/java-17-openjdk ./gradlew assembleRelease
```

Первая установка — по USB (`adb install`), дальше `linetctl update-phone`. Один раз по USB:

```sh
adb shell pm grant org.linet android.permission.WRITE_SECURE_SETTINGS   # экран телефона по Wi-Fi
```

На Xiaomi: в приложении «Снять ограничение» (батарея), «Автозапуск», «Контроль
активности → Нет ограничений», закрепить Linet в недавних. Для экрана ноутбука нужен AVNC.

Подпись APK: ключ в `~/.config/linet/` (`signing.properties` + `.jks`) — держать в бэкапе.

## Браузер

```sh
scripts/install-browser-host.sh     # регистрирует linet-nmh
scripts/sign-firefox.sh             # подпись unlisted, ключи AMO в ~/.config/linet/amo.env
```

Firefox: `about:addons` → «Установить дополнение из файла» → `dist/firefox/*.xpi`.
Chromium: `chrome://extensions` → режим разработчика → «Загрузить распакованное» → `extension/`.

## Проверка

```sh
scripts/e2e.sh     # сквозная проверка без телефона, перед каждым обновлением
```
