//! Серверная сторона: принимает подключения телефонов, сопряжение, сеансы.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use linet_core::client::CLOSE_UNPAIRED;
use linet_core::files::Transfers;
use linet_core::pairing::{self, Invite};
use linet_core::store::now_unix;
use linet_core::tls::{self, ClientPolicy};
use linet_core::{Envelope, Fingerprint, Hello, Identity, Message, PeerRecord, PeerStore, Session, frame};
use tokio::sync::watch;

use crate::mirror::Mirror;

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_BAD_HANDSHAKE: u32 = 2;

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
    /// Появляется после подключения к D-Bus.
    mirror: OnceLock<Arc<Mirror>>,
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
    pub fn new(identity: Identity, store: PeerStore, hello: Hello, port: u16, transfers: Arc<Transfers>) -> Arc<Self> {
        Arc::new(Self {
            identity,
            hello,
            port,
            store: Mutex::new(store),
            pairing: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            transfers,
            mirror: OnceLock::new(),
            changed: watch::channel(0).0,
        })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn set_mirror(&self, mirror: Arc<Mirror>) {
        let _ = self.mirror.set(mirror);
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

    fn session(&self, id: &str) -> Result<Arc<Session>> {
        let peer = self.find(id)?;
        let session = self.sessions.lock().unwrap().get(&peer.fingerprint).cloned();
        session.with_context(|| format!("{} не подключено", peer.name))
    }

    pub async fn ping(&self, id: &str) -> Result<Duration> {
        self.session(id)?.ping().await
    }

    /// Начинает отправку и сразу возвращает id передачи; ход — через прогресс-колбэк.
    pub async fn send_file(&self, id: &str, path: &std::path::Path) -> Result<u64> {
        let session = self.session(id)?;
        let file = tokio::fs::File::open(path).await.with_context(|| format!("{path:?}"))?;
        let meta = file.metadata().await?;
        if !meta.is_file() {
            bail!("{path:?} — не файл");
        }
        let name = path.file_name().context("нет имени файла")?.to_string_lossy().into_owned();
        let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
        let transfers = self.transfers.clone();
        let transfer = transfers.new_id();
        tokio::spawn(async move {
            if let Err(e) = transfers.send(&session, transfer, file, &name, meta.len(), &mime).await {
                tracing::warn!(%name, "отправка: {e:#}");
            }
        });
        Ok(transfer)
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
                let reply = match self.try_pair(fp, &hello, &proof) {
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

    fn try_pair(&self, fp: Fingerprint, hello: &Hello, proof: &[u8]) -> Result<()> {
        let mut window = self.pairing.lock().unwrap();
        let w = window.as_ref().filter(|w| w.expires > Instant::now()).context("окно сопряжения закрыто")?;
        if !pairing::verify(&w.invite.secret, &self.identity.fingerprint(), &fp, proof) {
            bail!("неверный код сопряжения");
        }
        // Приглашение одноразовое.
        window.take();
        drop(window);
        self.store.lock().unwrap().upsert(PeerRecord {
            fingerprint: fp,
            name: hello.name.clone(),
            kind: hello.kind,
            addrs: Vec::new(),
            paired_at: now_unix(),
        })?;
        tracing::info!(name = %hello.name, id = %fp.device_id(), "новое устройство сопряжено");
        self.notify_changed();
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
        self.notify_changed();

        while let Some(inbound) = events.recv().await {
            let Some(env) = self.transfers.handle(&session, inbound) else { continue };
            let rest = match self.mirror.get() {
                Some(mirror) => mirror.handle(&session, env.body).await,
                None => Some(env.body),
            };
            if let Some(msg) = rest {
                tracing::debug!(name = %session.hello().name, "необработанное сообщение: {msg:?}");
            }
        }

        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.get(&fp).is_some_and(|s| Arc::ptr_eq(s, &session)) {
                sessions.remove(&fp);
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
