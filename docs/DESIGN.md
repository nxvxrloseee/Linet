# Linet: дизайн

Связка Android-смартфона и Linux-ноутбука в духе экосистемы Apple: передача файлов,
уведомления, общий буфер обмена, Handoff и разблокировка ноутбука телефоном.

Пет-проект. Основная платформа — Hyprland (Wayland), но без жёсткой привязки к нему.

## Решения

| Вопрос | Решение | Почему |
|---|---|---|
| Протокол | Свой | Нужны фичи, которых нет в KDE Connect (Handoff, разблокировка, BLE) |
| Десктоп | Rust | tokio, quinn (QUIC), zbus (D-Bus), wayland-client, bluer (BlueZ) |
| Android | Kotlin + Jetpack Compose | Нативные API: NotificationListener, CompanionDeviceManager, Keystore |
| Общий код | `linet-core` на Rust, в Android через uniffi | Протокол, криптография и сеть реализованы один раз |
| Транспорт | QUIC (TLS 1.3) | Шифрование из коробки, мультиплексирование потоков: файл не блокирует уведомления |
| Сериализация | CBOR (serde + ciborium) | Самоописываемый формат: неизвестные поля игнорируются, протокол легко расширять |
| Обнаружение | mDNS/DNS-SD `_linet._udp`, позже BLE | mDNS работает в одной сети сразу; BLE нужен для близости и разблокировки |

## Архитектура

```
 Android (Kotlin)                                Linux
┌────────────────────────────┐                ┌──────────────────────────────────┐
│ UI (Compose)               │                │ linetd  (systemd --user)         │
│ ForegroundService          │                │  ├─ linet-core (то же ядро)      │
│ NotificationListener       │     QUIC       │  ├─ плагины: files, notify,      │
│ Share target, QS-плитки    │◄──────────────►│  │   clipboard, handoff, unlock  │
│ CompanionDeviceManager     │   mTLS, CBOR   │  ├─ D-Bus API  org.linet.Daemon  │
│ ┌────────────────────────┐ │                │  └─ unix-сокет для PAM           │
│ │ linet-core (uniffi/JNI)│ │      BLE       │                                  │
│ └────────────────────────┘ │◄ ─ ─ ─ ─ ─ ─ ─►│ linetctl (CLI, waybar, rofi)     │
└────────────────────────────┘   (близость)   │ pam_linet.so  (hyprlock)         │
                                              │ браузерное расширение            │
                                              │   + native messaging host        │
                                              └──────────────────────────────────┘
```

### Структура репозитория

```
linet/
├── Cargo.toml              # workspace
├── crates/
│   ├── linet-core/         # протокол, сообщения, крипто, транспорт, обнаружение
│   ├── linet-ffi/          # uniffi-обёртка core для Android
│   ├── linetd/             # демон: плагины, D-Bus, интеграция с десктопом
│   ├── linetctl/           # CLI + JSON-вывод для waybar
│   ├── pam-linet/          # PAM-модуль (cdylib)
│   └── linet-nmh/          # native messaging host для браузера
├── android/                # Gradle-проект, подключает .so из linet-ffi (cargo-ndk)
├── extension/              # WebExtension (Firefox/Chromium)
└── docs/
```

## Ядро: протокол

### Идентичность и сопряжение

- У каждого устройства есть долговременный ключ Ed25519 и самоподписанный сертификат.
  Идентичность — `sha256(сертификат)`, короткий `DeviceId` — первые 8 байт в hex.
  - Linux: `~/.local/share/linet/identity.key.der` (0600).
  - Android: файл в приватном каталоге приложения. TLS-стек работает в Rust и
    должен иметь доступ к ключу, поэтому Keystore для него не подходит. Keystore
    будет использоваться для отдельного ключа разблокировки (этап 6).
- Роли фиксированы: ноутбук — сервер, телефон — клиент. Так телефон сам решает,
  когда подключаться, и ему не нужно ничего слушать.
- **Сопряжение по QR-коду:** `linetctl pair` показывает QR с
  `linet://pair?addr=…&port=…&fp=<отпечаток сертификата>&secret=<128 бит>`.
  Телефон сканирует QR, подключается и проверяет отпечаток. Владение одноразовым
  секретом доказывается так: `HMAC(secret, оба отпечатка)`. Секрет живёт 2 минуты.
- **Запасной вариант без камеры:** оба устройства показывают 6-значный SAS
  (`hash(fp_a ‖ fp_b)`), пользователь подтверждает совпадение на обоих.
- После сопряжения работает только mTLS с закреплёнными (pinned) сертификатами.
  Неизвестные пиры отклоняются на этапе рукопожатия.

### Соединение

- QUIC (quinn), ALPN `linet/1`.
- **Поток 0** (двунаправленный) — управляющий. Сообщения в нём предваряются длиной:
  `u32 len ‖ CBOR(Envelope)`.
- **Файлы** передаются в отдельных однонаправленных потоках, по одному на файл.
- Keepalive шлёт только ноутбук, раз в 10 минут; idle-таймаут 30 минут (см. ниже).
- Ключ stateless reset выводится из ключа устройства. Поэтому после перезапуска
  демона телефон сразу получает валидный reset и переподключается, а не ждёт таймаута.
- Телефон перебирает известные адреса ноутбука параллельно; переподключение
  с экспоненциальной задержкой от 2 с до 5 мин.

```rust
struct Envelope {
    id: u64,                 // для корреляции запрос/ответ
    reply_to: Option<u64>,
    body: Message,
}

#[non_exhaustive]
enum Message {
    Hello { name: String, kind: DeviceKind, caps: Vec<Capability>, proto: u16 },
    Ping, Pong,
    FileOffer { transfer: Uuid, name: String, size: u64, mime: String, sha256: [u8; 32] },
    FileAccept { transfer: Uuid, offset: u64 },     // offset > 0 — докачка
    FileReject { transfer: Uuid, reason: String },
    Notification(Notification),
    NotificationDismiss { key: String },
    NotificationAction { key: String, action: String, reply: Option<String> },
    Clipboard { mime: String, data: Bytes, origin_ts: u64 },
    Activity(Activity),                              // Handoff
    UnlockChallenge { nonce: [u8; 32], host: String },
    UnlockResponse { nonce: [u8; 32], sig: Signature },
    UnlockDenied,
}
```

Через `caps` в `Hello` стороны договариваются о фичах. Неизвестные варианты
сообщений логируются и пропускаются, соединение не рвётся.

### Плагины в linetd

```rust
#[async_trait]
trait Plugin: Send + Sync {
    fn capability(&self) -> Capability;
    async fn on_connect(&self, peer: &Peer) -> Result<()>;
    async fn on_message(&self, peer: &Peer, msg: &Message) -> Result<()>;
}
```

Каждая фича — отдельный плагин и отдельная D-Bus-подсистема
(`org.linet.Daemon.Files`, `.Notifications`, …).

## Фичи

### 1. Файлы

- **Телефон → ноутбук:** share target в Android. Приём автоматический от сопряжённых
  устройств в `~/Downloads/Linet/`. По завершении — уведомление с действиями
  «Открыть» и «Показать в папке».
- **Ноутбук → телефон:** `linetctl send <файлы>`, пункт «Отправить на телефон»
  в rofi или файловом менеджере.
- Проверка sha256, докачка по `offset`. Прогресс публикуется в D-Bus, waybar его показывает.

### 2. Уведомления

- **Android:** `NotificationListenerService` → `Notification { key, app, title, text,
  icon_hash, actions, can_reply }`. Иконки приложений кешируются по хешу и передаются один раз.
- **Linux:** `org.freedesktop.Notifications.Notify` с действиями.
  - Inline-reply: swaync его поддерживает. Если демон не умеет, по действию «Ответить»
    открывается поле ввода в fuzzel/rofi.
- Закрытие синхронизируется в обе стороны (`NotificationDismiss`).
- Фильтр приложений настраивается на телефоне. По умолчанию не пересылаются
  постоянные уведомления (ongoing) и тихие.

### 3. Буфер обмена

- **Linux:** протокол `ext-data-control-v1` / `zwlr-data-control-v1` (Hyprland
  поддерживает оба). Это позволяет следить за буфером без фокуса окна.
  Поддерживаются `text/plain` и `image/png`.
- **Ноутбук → телефон:** автоматически. Писать в буфер Android может и из фона,
  через foreground service.
- **Телефон → ноутбук:** с Android 10 фоновое чтение буфера запрещено. Варианты:
  1. QS-плитка «Отправить буфер» и share sheet. Основной путь, работает везде.
  2. Опционально: разрешение `READ_LOGS` через `adb shell pm grant`. Это хак в духе
     KDE Connect: по записи в логе приложение узнаёт, что буфер изменился, и на мгновение
     выводит прозрачную activity, чтобы прочитать его.
- **Защита от эха:** у каждого содержимого есть `origin_ts` и хеш; повторно то же
  самое не отправляем.

### 4. Handoff

Настоящий Handoff с передачей состояния приложения на Android недостижим. Делаем
модель **Activity** — «то, чем я сейчас занят»:

```rust
enum Activity {
    Url { url: String, title: String, scroll: Option<f32> },
    Text { text: String },
    File { transfer: Uuid },
}
```

- **Браузер → телефон:** расширение (горячая клавиша или кнопка) отправляет URL,
  заголовок и позицию прокрутки через native messaging в `linet-nmh` → `linetd` → телефон.
  На телефоне появляется уведомление «Продолжить: <title>», по нажатию открывается браузер.
- **Телефон → ноутбук:** «Поделиться → Linet» на ссылке. В waybar появляется значок
  Handoff, клик выполняет `xdg-open`.
  Позицию прокрутки из Chrome на Android получить нельзя: ограничение платформы.

### 5. Разблокировка ноутбука телефоном

Самая чувствительная фича, поэтому реализуется последней.

- **Охват:** только `hyprlock` (PAM-сервис `hyprlock`). Не `sudo` и не вход в систему:
  при входе пароль нужен для разблокировки связки ключей.
- **Поток:**
  1. `pam_linet.so` → unix-сокет `linetd` (`$XDG_RUNTIME_DIR/linet/pam.sock`,
     с проверкой uid через `SO_PEERCRED`).
  2. `linetd` проверяет: доверенный телефон подключён по QUIC и находится рядом
     (BLE через bluer, RSSI выше порога).
  3. `linetd` отправляет `UnlockChallenge { nonce }`.
  4. Телефон отвечает, только если он сам разблокирован (`KeyguardManager.isDeviceLocked
     == false`). Подпись делается ключом из Keystore с
     `setUserAuthenticationRequired(true, timeout ≈ 5 мин)`, то есть телефон разблокировали недавно.
  5. `linetd` проверяет подпись и nonce → `PAM_SUCCESS`. Таймаут 3 с → `PAM_IGNORE`,
     после чего hyprlock спрашивает пароль как обычно.
- **Почему не только RSSI:** близость по BLE подделывается ретрансляцией (relay-атака).
  Криптографический ответ и требование «телефон разблокирован» закрывают это.
  Близость — лишь дополнительный фильтр.
- В `/etc/pam.d/hyprlock` модуль ставится как `sufficient` перед `pam_unix`, поэтому
  пароль всегда остаётся запасным путём.

## Батарея телефона

- Никаких WakeLock, AlarmManager, WorkManager и опроса.
- Телефон не шлёт keepalive: в глубоком сне его таймеры всё равно стоят, а будить
  CPU ради пинга дорого. Keepalive шлёт ноутбук: приём редкого пакета по Wi-Fi
  почти бесплатен, а соединение остаётся живым во время сна телефона.
- Длинный idle-таймаут (30 мин), чтобы соединение переживало сон.
- Переподключение по событиям: появился Wi-Fi, включился экран, нажата кнопка.
  Пауза между попытками в сне телефона тоже не тикает.
- mDNS-поиск ноутбука только короткими сеансами (15 с) и только если старый
  адрес не сработал: поиск держит Wi-Fi в режиме приёма multicast.
- В Rust-ядре один рабочий поток tokio.

## Android: как выжить в фоне

- `ForegroundService` с типом `connectedDevice` и постоянным тихим уведомлением.
- `CompanionDeviceManager`: связать ноутбук как companion device. Это даёт послабления
  по фоновой работе и `startObservingDevicePresence` (Android 12+) — система будит
  приложение, когда ноутбук появляется рядом по BLE.
- Запрос исключения из оптимизации батареи. Инструкции для MIUI/OneUI вынести в docs.
- Слушать `NsdManager` и смену сети, чтобы переподключаться.

## Интеграция с Hyprland

- `linetd` как `systemd --user` сервис (или `exec-once`).
- Модуль waybar: `linetctl status --json --follow` выводит устройство, батарею телефона,
  активные передачи и значок Handoff.
- Привязки клавиш: `linetctl send-clipboard`, `linetctl handoff`, меню в rofi
  (`linetctl menu`).

## Этапы

| # | Этап | Результат |
|---|---|---|
| ✅ 0 | Скелет | Cargo workspace, Android-проект, сборка `linet-ffi` через cargo-ndk + uniffi |
| ✅ 1 | Связь | mDNS, сопряжение по QR, mTLS/QUIC, `Hello`/`Ping`, `linetctl devices` |
| ✅ 2 | Файлы | Отправка в обе стороны, прогресс (докачка — позже) |
| ✅ 3 | Уведомления | Зеркалирование, действия, ответы, синхронизация закрытия |
| ✅ 4 | Буфер | Ноутбук → телефон автоматически, телефон → ноутбук через плитку |
| 5 | Handoff | Браузерное расширение, Activity, значок в waybar |
| 6 | Разблокировка | BLE-близость, challenge-response, `pam_linet.so` |
| — | Потом | Батарея и медиа-контроль, SMS, запуск scrcpy одной кнопкой, телефон как веб-камера |

## Открытые вопросы

- Сеть без общего Wi-Fi (Wi-Fi Direct / hotspot) — после этапа 6.
- Несколько телефонов или ноутбуков: модель уже позволяет, UI пока рассчитан на одну пару.
- Обновление протокола: поле `proto` в `Hello`, минимальная поддерживаемая версия.
