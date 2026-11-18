# Linet на Ryoku

Адаптация 2026-11-11: Hyprland 0.57.2, Ryoku 1.0.8.r6060.g2befae3,
Quickshell 0.3.2. Caelestia не запускается и не требуется. Android и `linet/1`
не менялись. Поддержка niri для VNC здесь не заявляется.

## Установка и обновление

```sh
python3 scripts/install-user.py
python3 scripts/setup-ryoku.py
bash scripts/install-ryoku-plugin.sh
```

Сборка первого шага: `cargo build --release --locked -p linetd -p linetctl`.
Для уже собранной версии: `python3 scripts/install-user.py --no-build`.
Канонические программы — `~/.local/bin/{linetd,linetctl,linet-nmh}`. Существующие
`~/.cargo/bin/linet*` становятся ссылками на них; unit, ярлыки, Nautilus и native
messaging используют абсолютные пути. Установщик учитывает XDG_CONFIG_HOME и
XDG_DATA_HOME. Повторный запуск не создаёт дубликатов.

При ручной установке Cargo указывайте тот же root:

```sh
cargo install --locked --force --root "$HOME/.local" --path crates/linetd
cargo install --locked --force --root "$HOME/.local" --path crates/linetctl
systemctl --user restart linetd
```

Unit использует Type=dbus, BusName=org.linet.Daemon, явный PATH и проверку
Wayland-сокета. WantedBy=ryoku-session.target и PartOf привязывают его к сессии;
Ryoku bootstrap сначала импортирует Wayland-окружение. Вне Ryoku установщик
использует graphical-session.target. Отдельная запись desktop.autostart не
создаётся. After= не используется как замена готовности трея/уведомлений:
регистрация SNI повторяется при смене владельца watcher; zbus подписки следуют
за новым владельцем Notifications. Старые callback/id уведомлений не переходят
между поколениями сервера, capability body-markup запрашивается заново.

Протокол native messaging и подпись расширения не менялись; расширение
не переподписывается. Проверить пути можно в org.linet.json в native-messaging
каталогах Firefox/Chromium. После смены расширения оно устанавливается отдельно,
но переустановка Linet не требует переиздания расширения.

## Плагин и окно телефона

Исходник плагина — **один** каталог
`~/Documents/ryoku-plugins/linet/`, созданный `ryoku plugin new`.
Установленная копия receipt-owned: её не редактировать. После изменения исходника:

```sh
ryoku plugin validate ~/Documents/ryoku-plugins/linet
bash scripts/install-ryoku-plugin.sh
```

В QS Bar Settings → Community доступен Linet. Щелчок по значку открывает имя,
статус, заряд, выбор устройства, отправку файлов, scrcpy, выбор приложения и
последние пять передач. Watcher один на плагин; он нужен закрытому бару для
заряда/связи, UI закрытой панели не опрашивается. Все изменяющие команды только
по нажатию; динамический shell-код не собирается. Нет KDE Connect, root/PAM,
сетевых запросов из плагина или доступа к секретному состоянию Linet.
При нескольких подключённых телефонах можно выбирать получателя файлов;
существующий screen API рассчитан на один подключённый телефон и UI честно
отключает screen-действия при неоднозначности.

В `hypr/user.lua` добавляется только маркированный блок `LINET RYOKU` из
`dist/ryoku-scrcpy.lua`: события window.open/window.title, class=scrcpy и
title=Телефон, float/center без фиксирования пропорций. Остальные окна не меняются.
Настройки Displays → bar/widgets для LINET-PHONE применяются через подтверждённые
команды settings.patch владельца shell.json, как в Hub, без прямой записи JSON.

## JSON API v1

`SnapshotJson` D-Bus возвращает JSON-строку. `linetctl snapshot` — один документ;
`linetctl watch` — NDJSON (по одному документу в строке), события DevicesChanged,
TransferProgress/Finished, смена владельца сервиса и резервный снимок раз в 30 с.
Обновление батареи вызывает совместимый DevicesChanged. RPC снимка ограничен
двумя секундами. Watcher не завершается при остановке демона; недоступность:

```json
{"schema_version":1,"available":false,"devices":[],"transfers":[]}
```

При available=true devices содержит `{id,name,kind,connected,battery}`.
battery — null или `{level:0..100,charging:boolean}`. id устройства — строка.
transfers — до 64 последних записей за текущий запуск демона:
`{id,peer,name,incoming,done,total,state,detail}`; id передачи — десятичная строка
для сохранения всех 64 бит в JavaScript, размеры — байты, state —
progress/done/failed. detail — путь/описание ошибки, пустая строка при прогрессе.
Новый прогресс/итог заменяет запись по `(id,incoming,peer)`.
При перезапуске демона история очищается, active UI восстанавливает новый снимок.
Существующие методы, сигналы и waybar-совместимый `linetctl status` сохранены.

`linetctl lock-status` / `LockStatus` возвращают schema_version=1, backend=ryoku,
state=locked|unlocked|unknown, can_lock и authenticated_unlock=false.
Автоблокировка работает только при достоверно unlocked, один запрос на эпизод
простоя. Неизвестный backend не трактуется как unlocked. Phone unlock выделен
в [отдельное предложение](QYLOCK_PHONE_AUTH.md), без включения системной auth.

## VNC

Hyprland backend остаётся в vnc.rs: mirror/extend, wayvnc, комбинированная US/RU
раскладка, масштаб для настольных приложений. Старт разрешён только при достоверно
unlocked. Во время захвата guard проверяет состояние каждые 250 мс; при locked,
unknown или PrepareForSleep захват закрывается. Guards привязаны к generation,
чтобы старый не остановил новый показ. Неуспешный старт и остановка удаляют
созданный output; конфиг с одноразовым паролем и socket удаляются после остановки.
Пассивный клиент без подключения закрывается через 60 секунд. Firewall не менялся.
Проверка реальной блокировки, сна, auth и изображения в AVNC выполняется с человеком;
тест RFB-баннера сам по себе не доказывает корректность кадров на NVIDIA.

## Проверки и откат

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/test-install.py
LINET_E2E_SKIP_VNC=1 bash scripts/e2e.sh
LINET_E2E_VNC_TIMEOUT=1 bash scripts/e2e.sh
```

Последняя команда кратко захватывает физический экран и создаёт/удаляет
LINET-PHONE; запускайте без чужого активного wayvnc. Тесты используют отдельную
D-Bus-шину, временные данные/Downloads, не меняют HOME и отключают unlock/autolock.
Они не доказывают работу реальной аутентификации qylock.

Приватный бэкап этого перехода:
`~/.local/state/linet/backups/ryoku-20261111-135513/` (0700).
`state.tar.gz` содержит старые программы, интеграцию и приватное состояние;
его нельзя добавлять в Git или отправлять внешнему агенту.

Адресный откат (не восстанавливает и не удаляет данные/сопряжения Linet):

```sh
python3 scripts/rollback-ryoku.py "$HOME/.local/state/linet/backups/ryoku-20261111-135513"
```

Он убирает установленный плагин, останавливает/отключает новый unit, восстанавливает
только неизменённые после установки программы и integration-файлы по записанным
хешам, удаляет свой Lua-блок, восстанавливает только два display-флага через
владельца настроек. Исходник плагина, данные Linet и чужие пользовательские правки
сохраняются. Старый демон остаётся inactive, как до адаптации; не запускайте его
старую Caelestia-разблокировку в Ryoku. Подробный журнал фактических проверок —
[RYOKU_VERIFICATION.md](RYOKU_VERIFICATION.md).
