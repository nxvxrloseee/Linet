# Проверка адаптации к Ryoku — 2026-11-11

Базовый этап реализован и установлен на действующей Hyprland-сессии:
Ryoku 1.0.8.r6060.g2befae3, Hyprland 0.57.2, Quickshell 0.3.2, NVIDIA RTX 4050.
Коммиты, push и публикация плагина не выполнялись. Android и linet-core не изменены.

## Установленное состояние

- Единые бинарники: `~/.local/bin/linetd`, `linetctl`, `linet-nmh`.
  Старые пути в `~/.cargo/bin` — совместимые ссылки. Хеши установленных программ
  совпадают с release-сборкой.
- `linetd.service` active/enabled, один демон, Type=dbus, привязка к
  `ryoku-session.target`, проверка Wayland-сокета перед запуском.
- Desktop entries, Nautilus script и native messaging manifests используют
  канонические абсолютные пути.
- Единственный исходник плагина: `~/Documents/ryoku-plugins/linet/`.
  Установлен локально, показан справа на QS Bar. Обновление:
  `scripts/install-ryoku-plugin.sh`; повторный запуск ничего не меняет.
- В `~/.config/hypr/user.lua` добавлен только помеченный блок scrcpy.
  Существующие настройки сохранены. Бар и виджеты для LINET-PHONE выключены
  через API владельца настроек; dock design=none оставлен без изменений.
- Идентичность ноутбука и существующее сопряжение проверены по приватному бэкапу
  и сохранены. POCO X6 Pro5G подключается; Android 0.1.1-125 не пересобирался.
- Временная беспроводная ADB-отладка после проверки отключена через Linet,
  тестовое ADB-соединение закрыто. Захват wayvnc и тестовые scrcpy завершены.

## Автоматические проверки

- `cargo test --workspace --locked`: 36 тестов (25 core, 11 desktop), все успешны.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: успешно.
- Изолированные D-Bus-тесты: позднее появление и смена SNI watcher;
  смена notification owner, повторное использование ID и защита callbacks
  от старого владельца. Проверены bounded transfer history, JSON IDs как строки,
  неизвестное состояние lock и отказ неподдерживаемой разблокировки,
  защита новой VNC-сессии от старого guard.
- `python3 scripts/test-install.py`: повторная установка, XDG, пробелы в путях,
  legacy links, unit/native manifests и аргументы Nautilus — успешно.
- `systemd-analyze --user verify`, bash syntax, Python compilation,
  `git diff --check` — успешно.
- `ryoku plugin validate`: 0 blocking, 0 warnings; qmllint публичных импортов
  прошёл. Установщик проверен на обновлении с сохранением настроек/позиции и no-op.
- Полный `LINET_E2E_VNC_TIMEOUT=1 bash scripts/e2e.sh` прошёл: двусторонние файлы,
  докачка 300 MiB после обрыва, целостность, изображения/нагрузка, pairing refusal,
  ping, mirror/extend, неизменные параметры физического монитора, удаление output
  после stop и реальный 60-секундный таймаут без клиента.
  RFB banner подтверждает listener, а не правильность кадров на NVIDIA.
- Один и тот же `linetctl watch` пережил остановку/запуск демона:
  available=true → false → true. Живой snapshot содержит подключение, батарею
  и строковые transfer IDs. Runtime unlock явно возвращает unsupported.

## Проверено на действующей системе и телефоне

- После `ryoku reload` renderer перезапущен, демон Linet не перезапускался;
  плагин и один watch-процесс восстановились. Живой SNI broker сохранил owner;
  именно смена owner проверялась отдельно на изолированной шине.
- Native messaging host получил framed Handoff собственного example.org URL
  и вернул ok. Расширение не переподписывалось.
- Собственный файл 256 KiB отправлен на телефон; SHA-256 Downloads совпал,
  тестовый файл удалён. Дополнительный тест через Nautilus/ShareActivity запущен,
  но его итоговый вывод не сохранён, поэтому успешным здесь не считается.
- Запуск установленного `.desktop` через gio открыл scrcpy «Телефон»:
  окно class=scrcpy плавающее и центрированное. App-mode org.linet также
  открылся отдельным плавающим окном. Тестовые процессы закрыты.
- ShareActivity телефона передала собственный кириллический текст в буфер
  ноутбука; исходный буфер сохранён/восстановлен без печати содержимого.
  Локальный PNG wl-copy/wl-paste прошёл побайтовую проверку.
- Widget/panel отрисованы offscreen собственным Qt-компонентом с живыми данными
  телефона; проверены изображение, кириллица и размещение действий. Это не
  утверждение о ручном клике по всем действиям в реальном баре.

## Что требует ручной проверки

Рабочая сессия намеренно не блокировалась, не усыплялась и не завершалась.
Нужны проверки logout/login, suspend/resume, реальной secure-lock блокировки
во время VNC, hotplug/автомасштабирования и восстановления после сна.

Не подтверждены реальные AVNC-кадры на NVIDIA, ввод кириллицы в VNC-клиенте,
передача изображения между телефонным и desktop-буфером, отсутствие эха для
всех типов буфера, ручные file/app pickers и контекстное меню файлового менеджера,
клик browser extension, живые действия pairing/ответа/открытия/закрытия уведомлений
и их изображения. Ответы другим людям в ходе проверки не отправлялись.

Authenticated phone unlock явно отключён; password/PAM/qylock не изменены.
Состояние unknown запрещает начало захвата и останавливает текущий, как locked;
дополнительно обрабатывается PrepareForSleep. Реальное снятие блокировки требует
участия пользователя. Безопасный следующий этап описан отдельно в
[QYLOCK_PHONE_AUTH.md](QYLOCK_PHONE_AUTH.md).

## Изменённые файлы

- `crates/linetd/src/{unlock,autolock,main,daemon,dbus,tray,notify,mirror,vnc}.rs`,
  новые `snapshot.rs`, `test_bus.rs`: адаптер Ryoku, lifecycle, API, захват.
- `crates/linetctl/src/{main,lib}.rs`, новый `status.rs`: snapshot/watch/lock-status;
  `crates/linet-sim/src/main.rs`: явное завершение VNC-пробы.
- `scripts/install-user.py`, desktop/browser wrappers, plugin installer,
  `setup-ryoku.py`, `rollback-ryoku.py`, `test-install.py`, `e2e.sh`;
  `dist/linetd.service`, `dist/ryoku-scrcpy.lua`, `.gitignore`.
- `README.md`, `docs/DESIGN.md`, `docs/RYOKU*.md`, `docs/QYLOCK_PHONE_AUTH.md`.
- Исходник QML/manifest/preview/README плагина в отдельном авторском каталоге.
  Пользовательская постановка `RYOKU_ADAPTATION_TASK.md` оставлена без изменений.

## Откат

Приватный бэкап 0700: `~/.local/state/linet/backups/ryoku-20261111-135513/`.
Не публиковать его: он содержит приватное состояние. Точный адресный откат:

```sh
python3 scripts/rollback-ryoku.py "$HOME/.local/state/linet/backups/ryoku-20261111-135513"
```

Скрипт проверяет fingerprints интеграционных файлов, сохраняет последующие
чужие правки, удаляет только свой Lua-блок/плагин и восстанавливает два display-флага
через владельца. Данные, идентичность и сопряжения Linet не откатываются и не
удаляются. Старый сервис остаётся остановленным. Сам откат не запускался, поскольку
он отменил бы установленную адаптацию. Подробнее — [RYOKU.md](RYOKU.md).
