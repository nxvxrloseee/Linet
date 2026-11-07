//! Обёртка linet-core для Android (Kotlin через uniffi).
//!
//! Внутри свой tokio-рантайм с одним рабочим потоком: телефону не нужна
//! параллельность, а лишние потоки — лишние пробуждения. Блокирующие методы
//! Kotlin вызывает из `Dispatchers.IO`.

use std::path::PathBuf;
use std::sync::{Arc, Once};

use linet_core::client::{Client, ClientEvent};
use linet_core::files::{TransferState, TransferUpdate, Transfers};
use linet_core::pairing::Invite;
use linet_core::{DeviceKind, Hello, Message, Notification, NotificationButton, PeerRecord};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum LinetError {
    #[error("{0}")]
    Failed(String),
}

impl From<anyhow::Error> for LinetError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(format!("{e:#}"))
    }
}

type Result<T> = std::result::Result<T, LinetError>;

#[derive(uniffi::Record)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
}

impl From<&PeerRecord> for PeerInfo {
    fn from(p: &PeerRecord) -> Self {
        Self { id: p.id(), name: p.name.clone(), kind: p.kind.as_str().into() }
    }
}

#[derive(uniffi::Enum)]
pub enum ConnState {
    Connected { peer: PeerInfo },
    Disconnected { reason: String },
    Unpaired,
}

#[derive(uniffi::Enum)]
pub enum TransferStatus {
    Progress,
    /// `path` — где лежит принятый файл (для исходящих — None).
    Done { path: Option<String> },
    Failed { reason: String },
}

#[derive(uniffi::Record)]
pub struct TransferInfo {
    pub id: u64,
    pub peer: String,
    pub name: String,
    pub incoming: bool,
    pub done: u64,
    pub total: u64,
    /// Назначение передачи: "app-update" — обновление приложения.
    pub purpose: Option<String>,
    pub status: TransferStatus,
}

impl From<TransferUpdate> for TransferInfo {
    fn from(u: TransferUpdate) -> Self {
        let status = match u.state {
            TransferState::Progress => TransferStatus::Progress,
            TransferState::Done { path } => TransferStatus::Done { path: path.map(|p| p.display().to_string()) },
            TransferState::Failed(reason) => TransferStatus::Failed { reason },
        };
        Self {
            id: u.id,
            peer: u.peer,
            name: u.name,
            incoming: u.incoming,
            done: u.done,
            total: u.total,
            purpose: u.purpose,
            status,
        }
    }
}

#[derive(uniffi::Record)]
pub struct NotificationAction {
    pub index: u32,
    pub title: String,
    pub reply: bool,
}

#[derive(uniffi::Record)]
pub struct NotificationInfo {
    pub key: String,
    pub package: String,
    pub app: String,
    pub title: String,
    pub text: String,
    /// PNG иконки приложения; слать только с первым уведомлением пакета за сеанс.
    pub icon: Option<Vec<u8>>,
    pub actions: Vec<NotificationAction>,
    pub clearable: bool,
}

impl From<NotificationInfo> for Notification {
    fn from(n: NotificationInfo) -> Self {
        Self {
            key: n.key,
            package: n.package,
            app: n.app,
            title: n.title,
            text: n.text,
            icon: n.icon.map(serde_bytes::ByteBuf::from),
            actions: n
                .actions
                .into_iter()
                .map(|a| NotificationButton { index: a.index, title: a.title, reply: a.reply })
                .collect(),
            clearable: n.clearable,
        }
    }
}

/// Колбэки вызываются из потока Rust.
#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_state(&self, state: ConnState);
    fn on_transfer(&self, transfer: TransferInfo);
    /// На ноутбуке нажали кнопку уведомления (`reply` — текст ответа).
    fn on_notification_action(&self, key: String, action: u32, reply: Option<String>);
    /// На ноутбуке смахнули уведомление.
    fn on_notification_dismiss(&self, key: String);
    /// На ноутбуке скопировали текст.
    fn on_clipboard(&self, text: String);
    /// На ноутбуке скопировали картинку.
    fn on_clipboard_image(&self, mime: String, data: Vec<u8>);
    /// Ноутбук передал страницу («продолжить на телефоне»).
    fn on_handoff(&self, url: String, title: String);
    /// Ноутбук просит включить/выключить беспроводную отладку. Ответить — `answer_screen_share`.
    fn on_screen_share(&self, request_id: u64, enable: bool);
    /// Ноутбук просит разблокировать экран. Ответить — `answer_unlock(request_id, …)`.
    fn on_unlock_request(&self, request_id: u64, host: String);
}

#[derive(uniffi::Object)]
pub struct LinetClient {
    client: Arc<Client>,
    rt: Runtime,
}

fn init_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "android")]
        {
            use tracing_subscriber::prelude::*;
            if let Ok(layer) = tracing_android::layer("linet") {
                let _ = tracing_subscriber::registry()
                    .with(tracing_subscriber::filter::LevelFilter::INFO)
                    .with(layer)
                    .try_init();
            }
        }
    });
}

impl LinetClient {
    fn send_quietly(&self, msg: Message) {
        let Some(session) = self.client.session() else { return };
        self.rt.spawn(async move {
            if let Err(e) = session.send(msg).await {
                tracing::debug!("отправка: {e:#}");
            }
        });
    }
}

#[uniffi::export]
impl LinetClient {
    #[uniffi::constructor]
    /// `inbox_dir` — куда складывать принятые файлы (дальше их забирает Kotlin).
    pub fn new(
        data_dir: String,
        inbox_dir: String,
        device_name: String,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<Self>> {
        init_logging();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .thread_name("linet")
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)?;
        let hello = Hello {
            name: device_name,
            kind: DeviceKind::Phone,
            proto: linet_core::PROTO_VERSION,
            caps: vec!["ping".into(), "files".into(), "notifications".into(), "clipboard".into(), "handoff".into(), "unlock".into(), "vnc".into()],
        };
        let progress = listener.clone();
        // Файл сначала ложится в приватный каталог, потом копируется в «Загрузки»: нужен двойной запас места.
        let transfers = Transfers::new(PathBuf::from(inbox_dir), 4 << 30, Arc::new(move |u| progress.on_transfer(u.into())));
        let client = Arc::new(Client::new(&PathBuf::from(data_dir), hello, transfers)?);

        let (tx, mut rx) = mpsc::channel(16);
        let runner = client.clone();
        rt.spawn(async move { runner.run(tx).await });
        rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                let state = match ev {
                    ClientEvent::Connected(peer) => ConnState::Connected { peer: (&peer).into() },
                    ClientEvent::Disconnected(reason) => ConnState::Disconnected { reason },
                    ClientEvent::Unpaired => ConnState::Unpaired,
                    ClientEvent::Message(env) => {
                        match env.body {
                            Message::NotificationAction { key, action, reply } => {
                                listener.on_notification_action(key, action, reply)
                            }
                            Message::NotificationDismiss { key } => listener.on_notification_dismiss(key),
                            Message::Clipboard { text } => listener.on_clipboard(text),
                            Message::ClipboardImage { mime, data } => {
                                listener.on_clipboard_image(mime, data.into_vec())
                            }
                            Message::Handoff { url, title } => listener.on_handoff(url, title),
                            Message::UnlockRequest { host } => listener.on_unlock_request(env.id, host),
                            Message::ScreenShare { enable } => listener.on_screen_share(env.id, enable),
                            other => tracing::debug!("сообщение: {other:?}"),
                        }
                        continue;
                    }
                };
                listener.on_state(state);
            }
        });
        Ok(Arc::new(Self { client, rt }))
    }

    /// Короткий id этого телефона.
    pub fn device_id(&self) -> String {
        self.client.fingerprint().device_id()
    }

    pub fn paired(&self) -> Option<PeerInfo> {
        self.client.paired().as_ref().map(Into::into)
    }

    pub fn is_connected(&self) -> bool {
        self.client.session().is_some()
    }

    /// Блокирует до завершения сопряжения.
    pub fn pair(&self, invite_uri: String) -> Result<PeerInfo> {
        let invite = Invite::parse(&invite_uri)?;
        let peer = self.rt.block_on(self.client.pair(&invite))?;
        Ok((&peer).into())
    }

    pub fn unpair(&self) -> Result<()> {
        Ok(self.client.unpair()?)
    }

    /// Попробовать подключиться прямо сейчас (появилась сеть, включился экран).
    pub fn kick(&self) {
        self.client.kick();
    }

    /// Адрес ноутбука, найденный через NSD (mDNS), например "192.168.1.5:47321".
    pub fn add_address(&self, addr: String) -> Result<()> {
        let addr = addr.parse().map_err(|e| LinetError::Failed(format!("{addr}: {e}")))?;
        Ok(self.client.add_address(addr)?)
    }

    /// Отправляет файл по открытому дескриптору (владение переходит к Rust).
    /// Блокирует до доставки; ход передачи приходит в `on_transfer`.
    pub fn send_fd(&self, fd: i32, name: String, size: u64, mime: String) -> Result<u64> {
        // SAFETY: Kotlin передаёт fd из ParcelFileDescriptor.detachFd() и больше его не использует.
        let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let id = self.rt.block_on(self.client.send_file(file, &name, size, &mime, None))?;
        Ok(id)
    }

    /// Переслать уведомление на ноутбук. Не блокирует; без соединения — молча пропускает:
    /// копить уведомления в очереди ради батареи не стоит.
    pub fn post_notification(&self, notification: NotificationInfo) {
        self.send_quietly(Message::Notification(notification.into()));
    }

    pub fn remove_notification(&self, key: String) {
        self.send_quietly(Message::NotificationRemoved { key });
    }

    /// Отправить текст в буфер обмена ноутбука. `false` — нет соединения.
    pub fn send_clipboard(&self, text: String) -> bool {
        let connected = self.client.session().is_some();
        self.send_quietly(Message::Clipboard { text });
        connected
    }

    /// Отправить картинку в буфер обмена ноутбука. `false` — нет соединения или слишком большая.
    pub fn send_clipboard_image(&self, mime: String, data: Vec<u8>) -> bool {
        if data.len() > linet_core::proto::MAX_CLIPBOARD_IMAGE {
            return false;
        }
        let connected = self.client.session().is_some();
        self.send_quietly(Message::ClipboardImage { mime, data: serde_bytes::ByteBuf::from(data) });
        connected
    }

    /// Открыть ссылку на ноутбуке. `false` — нет соединения.
    pub fn send_handoff(&self, url: String, title: String) -> bool {
        let connected = self.client.session().is_some();
        self.send_quietly(Message::Handoff { url, title });
        connected
    }

    /// Попросить ноутбук показать свой экран (`extend` — вторым монитором размером
    /// `width`×`height`). Блокирует до ответа; возвращает ссылку `vnc://`.
    pub fn request_vnc(&self, extend: bool, width: u32, height: u32) -> Result<String> {
        let session = self.client.session().ok_or_else(|| LinetError::Failed("нет соединения с ноутбуком".into()))?;
        let reply = self.rt.block_on(session.request_slow(
            Message::VncRequest { extend, width, height },
            std::time::Duration::from_secs(20),
        ))?;
        match reply {
            Message::VncResult { url, error } if error.is_empty() => Ok(url),
            Message::VncResult { error, .. } => Err(LinetError::Failed(error)),
            other => Err(LinetError::Failed(format!("неожиданный ответ: {other:?}"))),
        }
    }

    pub fn stop_vnc(&self) {
        self.send_quietly(Message::VncStop);
    }

    pub fn answer_screen_share(&self, request_id: u64, ok: bool, error: String) {
        let Some(session) = self.client.session() else { return };
        self.rt.spawn(async move {
            let _ = session.reply(request_id, Message::ScreenShareResult { ok, error }).await;
        });
    }

    pub fn answer_unlock(&self, request_id: u64, approved: bool, reason: String) {
        let Some(session) = self.client.session() else { return };
        self.rt.spawn(async move {
            let reply = Message::UnlockResponse { approved, reason };
            if let Err(e) = session.reply(request_id, reply).await {
                tracing::debug!("ответ на разблокировку: {e:#}");
            }
        });
    }

    /// Сообщить ноутбуку заряд батареи. Без соединения — ничего.
    pub fn send_battery(&self, level: u8, charging: bool) {
        self.send_quietly(Message::Battery { level, charging });
    }

    /// Время отклика ноутбука в миллисекундах.
    pub fn ping(&self) -> Result<f64> {
        let session = self.client.session().ok_or_else(|| LinetError::Failed("нет соединения".into()))?;
        let rtt = self.rt.block_on(session.ping())?;
        Ok(rtt.as_secs_f64() * 1000.0)
    }
}
