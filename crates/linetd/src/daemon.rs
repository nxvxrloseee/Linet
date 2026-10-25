//! Серверная сторона: принимает подключения телефонов, сопряжение, сеансы.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use linet_core::client::CLOSE_UNPAIRED;
use linet_core::files::{SEND_ATTEMPTS, Transfers};
use linet_core::pairing::{self, Invite};
use linet_core::store::now_unix;
use linet_core::tls::{self, ClientPolicy};
use linet_core::{Envelope, Fingerprint, Hello, Identity, Message, PeerRecord, PeerStore, Session, frame};
use tokio::sync::watch;

use crate::clipboard::Clipboard;
use crate::mirror::Mirror;
use crate::notify::{Handler, Notice, NoticeEvent, Notifier};
use crate::stats::Stats;
use crate::tray::Tray;
use crate::vnc::Vnc;

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const PAIRING_CONFIRM: Duration = Duration::from_secs(60);
/// Сколько ждать телефон, чтобы докачать прерванную отправку.
const RESUME_WAIT: Duration = Duration::from_secs(120);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_BAD_HANDSHAKE: u32 = 2;

/// Уведомлять о заряде на этих порогах; выше `LOW_BATTERY_RESET` — сброс.
const LOW_BATTERY: &[u8] = &[20, 10];
const LOW_BATTERY_RESET: u8 = 25;

struct BatteryState {
    level: u8,
    charging: bool,
    /// Последний порог, о котором уже уведомили.
    warned_at: Option<u8>,
}

struct PairingWindow {
    invite: Invite,
    expires: Instant,
}

pub struct Daemon {
    identity: Identity,
    hello: Hello,
    port: u16,
    store: Mutex<PeerStore>,
    pairing: Mutex<Option<PairingWindow>>,
    sessions: Mutex<HashMap<Fingerprint, Arc<Session>>>,
    transfers: Arc<Transfers>,
    /// Появляются после подключения к D-Bus.
    mirror: OnceLock<Arc<Mirror>>,
    notifier: OnceLock<Notifier>,
    clipboard: Option<Arc<Clipboard>>,
    tray: OnceLock<Tray>,
    stats: Stats,
    vnc: OnceLock<Arc<Vnc>>,
    battery: Mutex<HashMap<Fingerprint, BatteryState>>,
    /// Счётчик изменений списка устройств — для D-Bus-сигнала.
    changed: watch::Sender<u64>,
}

pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub connected: bool,
}

impl Daemon {
    pub fn new(
        identity: Identity,
        store: PeerStore,
        hello: Hello,
        port: u16,
        transfers: Arc<Transfers>,
        clipboard: Option<Arc<Clipboard>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            hello,
            port,
            store: Mutex::new(store),
            pairing: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            transfers,
            mirror: OnceLock::new(),
            notifier: OnceLock::new(),
            clipboard,
            tray: OnceLock::new(),
            stats: Stats::new(),
            vnc: OnceLock::new(),
            battery: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
        })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn set_notifier(&self, notifier: Notifier, mirror: Arc<Mirror>) {
        let _ = self.notifier.set(notifier);
        let _ = self.mirror.set(mirror);
    }

    /// (заряд, заряжается) устройства, если оно его сообщало.
    pub fn battery(&self, id: &str) -> Result<Option<(u8, bool)>> {
        let peer = self.find(id)?;
        Ok(self.battery.lock().unwrap().get(&peer.fingerprint).map(|b| (b.level, b.charging)))
    }

    async fn on_battery(&self, session: &Session, level: u8, charging: bool) {
        let fp = session.peer();
        let warn = {
            let mut map = self.battery.lock().unwrap();
            self.stats.battery(level, charging);
            let b = map.entry(fp).or_insert(BatteryState { level, charging, warned_at: None });
            b.level = level;
            b.charging = charging;
            if charging || level > LOW_BATTERY_RESET {
                b.warned_at = None;
                None
            } else {
                // Каждый порог — одно уведомление, пока телефон не поставят на зарядку.
                let threshold = LOW_BATTERY.iter().copied().filter(|&t| level <= t).min();
                match threshold {
                    Some(t) if b.warned_at.is_none_or(|w| t < w) => {
                        b.warned_at = Some(t);
                        Some(level)
                    }
                    _ => None,
                }
            }
        };
        if let Some(tray) = self.tray.get() {
            tray.update(|t| t.battery = Some((level, charging))).await;
        }
        if let (Some(level), Some(n)) = (warn, self.notifier.get()) {
            let summary = format!("{}: батарея {level} %", session.hello().name);
            let notice = Notice {
                app: "Linet",
                summary: &summary,
                body: "Телефон скоро разрядится",
                icon: "battery-caution-symbolic",
                actions: &[],
                replaces: 0,
            };
            let _ = n.show(notice, None).await;
        }
    }

    pub fn set_vnc(&self, vnc: Arc<Vnc>) {
        let _ = self.vnc.set(vnc);
    }

    /// Запрос телефона показать экран ноутбука.
    async fn on_vnc_request(&self, session: &Session, request: u64, extend: bool, width: u32, height: u32) {
        let reply = match self.vnc.get() {
            None => Message::VncResult { url: String::new(), error: "показ экрана выключен".into() },
            Some(vnc) => {
                let ip = session.connection().remote_address().ip();
                let ip = match ip {
                    std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
                    v4 => v4,
                };
                match vnc.start(ip, extend, width, height).await {
                    Ok(url) => Message::VncResult { url, error: String::new() },
                    Err(e) => {
                        tracing::warn!("показ экрана: {e:#}");
                        Message::VncResult { url: String::new(), error: format!("{e:#}") }
                    }
                }
            }
        };
        let _ = session.reply(request, reply).await;
    }

    pub fn set_tray(&self, tray: Tray) {
        let _ = self.tray.set(tray);
    }

    /// Единственный подключённый телефон, если он один.
    pub fn phone_session(&self) -> Option<Arc<Session>> {
        self.session("").ok()
    }

    pub fn session_of(&self, fp: &Fingerprint) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(fp).cloned()
    }

    /// Счётчики для `linetctl stats`.
    pub fn stats(&self) -> Vec<(String, String)> {
        let open: Vec<quinn::Connection> =
            self.sessions.lock().unwrap().values().map(|s| s.connection().clone()).collect();
        let battery = self.battery.lock().unwrap().values().next().map(|b| (b.level, b.charging));
        self.stats.report(&open, battery)
    }

    pub fn connected(&self) -> bool {
        !self.sessions.lock().unwrap().is_empty()
    }

    /// Отправить сообщение всем подключённым устройствам.
    pub async fn broadcast(&self, msg: Message) {
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        for s in sessions {
            if let Err(e) = s.send(msg.clone()).await {
                tracing::debug!(name = %s.hello().name, "рассылка: {e:#}");
            }
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify_changed(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    pub fn policy(self: &Arc<Self>) -> Arc<dyn ClientPolicy> {
        Arc::new(Policy(self.clone()))
    }

    fn pairing_open(&self) -> bool {
        self.pairing.lock().unwrap().as_ref().is_some_and(|w| w.expires > Instant::now())
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        let sessions = self.sessions.lock().unwrap();
        self.store
            .lock()
            .unwrap()
            .peers()
            .iter()
            .map(|p| DeviceInfo {
                id: p.id(),
                name: p.name.clone(),
                kind: p.kind.as_str().to_string(),
                connected: sessions.contains_key(&p.fingerprint),
            })
            .collect()
    }

    /// Открывает окно сопряжения и возвращает URI для QR-кода.
    pub fn start_pairing(&self) -> Result<String> {
        let addrs = local_addrs(self.port);
        if addrs.is_empty() {
            bail!("нет сетевых адресов: ноутбук не подключён к сети?");
        }
        let invite = Invite::new(self.identity.fingerprint(), addrs)?;
        let uri = invite.to_uri();
        *self.pairing.lock().unwrap() = Some(PairingWindow { invite, expires: Instant::now() + PAIRING_WINDOW });
        tracing::info!("окно сопряжения открыто на {PAIRING_WINDOW:?}");
        Ok(uri)
    }

    pub fn cancel_pairing(&self) {
        self.pairing.lock().unwrap().take();
    }

    fn find(&self, id: &str) -> Result<PeerRecord> {
        self.store.lock().unwrap().find(id).cloned().with_context(|| format!("устройство {id} не найдено"))
    }

    /// Сеанс по id; пустой id — единственное подключённое устройство.
    fn session(&self, id: &str) -> Result<Arc<Session>> {
        if id.is_empty() {
            let sessions = self.sessions.lock().unwrap();
            let mut it = sessions.values();
            return match (it.next(), it.next()) {
                (Some(s), None) => Ok(s.clone()),
                (None, _) => bail!("нет подключённых устройств"),
                _ => bail!("подключено несколько устройств, укажи какое"),
            };
        }
        let peer = self.find(id)?;
        let session = self.sessions.lock().unwrap().get(&peer.fingerprint).cloned();
        session.with_context(|| format!("{} не подключено", peer.name))
    }

    /// Ждёт подключения единственного устройства (например, после выхода из сна).
    pub async fn wait_session(&self, timeout: Duration) -> Result<Arc<Session>> {
        let deadline = Instant::now() + timeout;
        let mut changes = self.subscribe();
        loop {
            if let Ok(s) = self.session("") {
                return Ok(s);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, changes.changed()).await.is_err() {
                return self.session("");
            }
        }
    }

    pub async fn send_url(&self, id: &str, url: &str, title: &str) -> Result<()> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            bail!("нужна http(s)-ссылка");
        }
        let session = self.session(id)?;
        session.send(Message::Handoff { url: url.into(), title: title.into() }).await?;
        Ok(())
    }

    /// Попросить телефон включить/выключить беспроводную отладку. Возвращает его IP.
    pub async fn screen_share(&self, enable: bool) -> Result<String> {
        let session = self.session("")?;
        let reply = session.request_slow(Message::ScreenShare { enable }, Duration::from_secs(10)).await?;
        match reply {
            Message::ScreenShareResult { ok: true, .. } => {}
            Message::ScreenShareResult { ok: false, error } => bail!("{error}"),
            other => bail!("неожиданный ответ: {other:?}"),
        }
        let ip = session.connection().remote_address().ip();
        // Демон слушает dual-stack: IPv4 телефона приходит как ::ffff:a.b.c.d.
        let ip = match ip {
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        Ok(ip.to_string())
    }

    pub async fn ping(&self, id: &str) -> Result<Duration> {
        self.session(id)?.ping().await
    }

    /// Начинает отправку и сразу возвращает id передачи; ход — через прогресс-колбэк.
    /// Если связь рвётся посреди передачи, ждёт переподключения и докачивает.
    pub async fn send_file(self: &Arc<Self>, id: &str, path: &std::path::Path, purpose: Option<&str>) -> Result<u64> {
        let session = self.session(id)?;
        let file = std::fs::File::open(path).with_context(|| format!("{path:?}"))?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            bail!("{path:?} — не файл");
        }
        let name = path.file_name().context("нет имени файла")?.to_string_lossy().into_owned();
        let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
        let purpose = purpose.map(str::to_string);
        let transfer = self.transfers.new_id();
        let this = self.clone();
        tokio::spawn(async move {
            let mut session = session;
            for attempt in 1..=SEND_ATTEMPTS {
                let last = attempt == SEND_ATTEMPTS;
                let reader = match file.try_clone() {
                    Ok(f) => tokio::fs::File::from_std(f),
                    Err(e) => return tracing::warn!(%name, "отправка: {e}"),
                };
                let res = this
                    .transfers
                    .send(&session, transfer, reader, &name, meta.len(), &mime, purpose.as_deref(), last)
                    .await;
                match res {
                    Ok(()) => return,
                    Err(e) if last || session.connection().close_reason().is_none() => {
                        return tracing::warn!(%name, "отправка: {e:#}");
                    }
                    Err(e) => {
                        tracing::info!(%name, attempt, "связь оборвалась, докачаю после переподключения: {e:#}");
                        match this.wait_new_session(&session, RESUME_WAIT).await {
                            Some(s) => session = s,
                            None => return tracing::warn!(%name, "телефон не вернулся, отправка прервана"),
                        }
                    }
                }
            }
        });
        Ok(transfer)
    }

    /// Ждёт новый сеанс того же устройства после обрыва.
    async fn wait_new_session(&self, old: &Arc<Session>, timeout: Duration) -> Option<Arc<Session>> {
        let deadline = Instant::now() + timeout;
        let mut changes = self.subscribe();
        loop {
            let current = self.sessions.lock().unwrap().get(&old.peer()).cloned();
            if let Some(s) = current.filter(|s| !Arc::ptr_eq(s, old)) {
                return Some(s);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, changes.changed()).await.is_err() {
                return None;
            }
        }
    }

    pub fn unpair(&self, id: &str) -> Result<()> {
        let peer = self.find(id)?;
        self.store.lock().unwrap().remove(&peer.fingerprint)?;
        if let Some(s) = self.sessions.lock().unwrap().remove(&peer.fingerprint) {
            s.connection().close(CLOSE_UNPAIRED.into(), b"unpaired");
        }
        tracing::info!(name = %peer.name, "сопряжение удалено");
        self.notify_changed();
        Ok(())
    }

    pub async fn serve(self: Arc<Self>, endpoint: quinn::Endpoint) {
        while let Some(incoming) = endpoint.accept().await {
            let this = self.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                if let Err(e) = this.handle(incoming).await {
                    tracing::debug!(%remote, "соединение: {e:#}");
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, incoming: quinn::Incoming) -> Result<()> {
        let conn = incoming.await?;
        let fp = tls::peer_fingerprint(&conn).context("нет сертификата клиента")?;
        let (mut tx, mut rx) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await??;
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, frame::read(&mut rx))
            .await??
            .context("клиент закрыл поток")?;

        match first.body {
            Message::PairRequest { hello, proof } => {
                let reply = match self.try_pair(fp, &hello, &proof).await {
                    Ok(()) => Message::PairAccept(self.hello.clone()),
                    Err(e) => {
                        tracing::warn!(name = %hello.name, "сопряжение отклонено: {e:#}");
                        Message::PairReject { reason: e.to_string() }
                    }
                };
                frame::write(&mut tx, &Envelope { id: 0, reply_to: Some(first.id), body: reply }).await?;
                let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.closed()).await;
                Ok(())
            }
            Message::Hello(hello) if self.store.lock().unwrap().contains(&fp) => {
                let reply = Envelope { id: 0, reply_to: Some(first.id), body: Message::Hello(self.hello.clone()) };
                frame::write(&mut tx, &reply).await?;
                self.run_session(conn, tx, rx, fp, hello).await
            }
            other => {
                conn.close(CLOSE_BAD_HANDSHAKE.into(), b"bad handshake");
                bail!("неожиданное первое сообщение: {other:?}");
            }
        }
    }

    async fn try_pair(&self, fp: Fingerprint, hello: &Hello, proof: &[u8]) -> Result<()> {
        {
            let mut window = self.pairing.lock().unwrap();
            let w = window.as_ref().filter(|w| w.expires > Instant::now()).context("окно сопряжения закрыто")?;
            if !pairing::verify(&w.invite.secret, &self.identity.fingerprint(), &fp, proof) {
                bail!("неверный код сопряжения");
            }
            // Приглашение одноразовое.
            window.take();
        }
        // Открыть окно сопряжения может любой процесс пользователя через D-Bus,
        // поэтому новое устройство подтверждает человек — кнопкой в уведомлении.
        self.confirm_pairing(fp, hello).await?;
        self.store.lock().unwrap().upsert(PeerRecord {
            fingerprint: fp,
            name: hello.name.clone(),
            kind: hello.kind,
            addrs: Vec::new(),
            paired_at: now_unix(),
            cert: None,
        })?;
        tracing::info!(name = %hello.name, id = %fp.device_id(), "новое устройство сопряжено");
        self.notify_changed();
        Ok(())
    }

    async fn confirm_pairing(&self, fp: Fingerprint, hello: &Hello) -> Result<()> {
        let notifier = self.notifier.get().context("нет сервера уведомлений для подтверждения")?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Mutex::new(Some(tx));
        let summary = format!("Сопрячь «{}»?", hello.name);
        let body = format!("Новое устройство, id {}. Оно получит уведомления, буфер обмена и файлы.", fp.device_id());
        let notice = Notice {
            app: "Linet",
            summary: &summary,
            body: &body,
            icon: "dialog-password",
            actions: &[("accept", "Сопрячь"), ("reject", "Отклонить")],
            replaces: 0,
        };
        let handler: Handler = Box::new(move |event| {
            let decision = match event {
                NoticeEvent::Action("accept") => true,
                NoticeEvent::Action(_) | NoticeEvent::Closed(_) => false,
            };
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(decision);
            }
        });
        let id = notifier.show(notice, Some(handler)).await?;
        let decision = tokio::time::timeout(PAIRING_CONFIRM, rx).await;
        if !matches!(decision, Ok(Ok(true))) {
            notifier.close(id).await;
            bail!("сопряжение не подтверждено на ноутбуке");
        }
        Ok(())
    }

    async fn run_session(
        &self,
        conn: quinn::Connection,
        tx: quinn::SendStream,
        rx: quinn::RecvStream,
        fp: Fingerprint,
        hello: Hello,
    ) -> Result<()> {
        let remote = conn.remote_address();
        self.store.lock().unwrap().update(&fp, |p| {
            p.name = hello.name.clone();
            p.kind = hello.kind;
            p.addrs = vec![remote.to_string()];
        })?;
        let (session, mut events) = Session::spawn(conn, tx, rx, fp, hello, 2);
        if let Some(old) = self.sessions.lock().unwrap().insert(fp, session.clone()) {
            old.close("replaced");
        }
        tracing::info!(name = %session.hello().name, %remote, "подключено");
        self.stats.connected();
        self.notify_changed();
        if let Some(tray) = self.tray.get() {
            let name = session.hello().name.clone();
            tray.update(|t| {
                t.phone = Some(name);
                t.battery = None;
            })
            .await;
        }

        while let Some(inbound) = events.recv().await {
            let Some(env) = self.transfers.handle(&session, inbound) else { continue };
            let rest = match (env.body, &self.clipboard) {
                (Message::VncRequest { extend, width, height }, _) => {
                    self.on_vnc_request(&session, env.id, extend, width, height).await;
                    None
                }
                (Message::VncStop, _) => {
                    if let Some(vnc) = self.vnc.get() {
                        vnc.stop().await;
                    }
                    None
                }
                (Message::Battery { level, charging }, _) => {
                    self.on_battery(&session, level, charging).await;
                    None
                }
                (Message::ClipboardImage { mime, data }, Some(clipboard)) => {
                    tracing::debug!(%mime, len = data.len(), "картинка из буфера телефона");
                    clipboard.set_image(&mime, &data).await;
                    None
                }
                (Message::Clipboard { text }, Some(clipboard)) => {
                    tracing::debug!(len = text.len(), "буфер обмена с телефона");
                    clipboard.set(text).await;
                    None
                }
                (body, _) => match self.mirror.get() {
                    Some(mirror) => mirror.handle(&session, body).await,
                    None => Some(body),
                },
            };
            if let Some(msg) = rest {
                tracing::debug!(name = %session.hello().name, "необработанное сообщение: {msg:?}");
            }
        }

        self.stats.disconnected(session.connection());
        let gone = {
            let mut sessions = self.sessions.lock().unwrap();
            let ours = sessions.get(&fp).is_some_and(|s| Arc::ptr_eq(s, &session));
            if ours {
                sessions.remove(&fp);
            }
            ours
        };
        if gone {
            self.battery.lock().unwrap().remove(&fp);
            // Телефон ушёл — экран ноутбука больше никому не показываем.
            if let Some(vnc) = self.vnc.get() {
                vnc.stop().await;
            }
            if let Some(tray) = self.tray.get() {
                tray.update(|t| *t = Default::default()).await;
            }
        }
        tracing::info!(name = %session.hello().name, reason = ?session.connection().close_reason(), "отключено");
        self.notify_changed();
        Ok(())
    }
}

struct Policy(Arc<Daemon>);

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Policy")
    }
}

impl ClientPolicy for Policy {
    fn allow(&self, fp: &Fingerprint) -> bool {
        self.0.store.lock().unwrap().contains(fp) || self.0.pairing_open()
    }
}

/// Адреса, по которым телефон может достучаться до ноутбука.
fn local_addrs(port: u16) -> Vec<SocketAddr> {
    // VPN, туннели и контейнерные мосты телефону недоступны.
    const VIRTUAL: &[&str] = &["docker", "br-", "veth", "virbr", "vnet", "tun", "tap", "wg", "tailscale", "zt"];
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return Vec::new() };
    let mut addrs: Vec<SocketAddr> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback() && !VIRTUAL.iter().any(|v| i.name.contains(v)))
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if !v4.is_link_local() => Some(SocketAddr::new(IpAddr::V4(v4), port)),
            _ => None,
        })
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs
}
