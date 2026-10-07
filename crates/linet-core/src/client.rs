//! Клиентская сторона (телефон): сопряжение и поддержание соединения с ноутбуком.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use quinn::{Connection, Endpoint};
use serde_bytes::ByteBuf;
use tokio::sync::{Notify, mpsc};

use crate::files::{SEND_ATTEMPTS, Transfers};
use crate::identity::{Fingerprint, Identity};
use crate::pairing::{self, Invite};
use crate::proto::{Envelope, Hello, Message};
use crate::session::{Inbound, Session};
use crate::store::{PeerRecord, PeerStore, now_unix};
use crate::{frame, transport};

const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
/// Потолок паузы между попытками. Во сне телефона таймер всё равно стоит,
/// так что лишних пробуждений это не вызывает.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// Сколько ждать переподключения, чтобы продолжить прерванную отправку.
const RESUME_WAIT: Duration = Duration::from_secs(120);

/// Код закрытия QUIC: сервер забыл это устройство.
pub const CLOSE_UNPAIRED: u32 = 1;

#[derive(Debug)]
pub enum ClientEvent {
    Connected(PeerRecord),
    Disconnected(String),
    Unpaired,
    Message(Envelope),
}

pub struct Client {
    identity: Identity,
    store: Mutex<PeerStore>,
    hello: Hello,
    endpoint: Mutex<Option<Endpoint>>,
    session: Mutex<Option<Arc<Session>>>,
    kick: Notify,
    transfers: Arc<Transfers>,
}

impl Client {
    pub fn new(data_dir: &Path, hello: Hello, transfers: Arc<Transfers>) -> Result<Self> {
        Ok(Self {
            identity: Identity::load_or_create(data_dir)?,
            store: Mutex::new(PeerStore::load(data_dir.join("peers.json"))?),
            hello,
            endpoint: Mutex::new(None),
            session: Mutex::new(None),
            kick: Notify::new(),
            transfers,
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.identity.fingerprint()
    }

    /// MVP поддерживает одно сопряжённое устройство.
    pub fn paired(&self) -> Option<PeerRecord> {
        self.store.lock().unwrap().peers().first().cloned()
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone()
    }

    /// Прервать паузу между попытками: сеть появилась, экран включился и т. п.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Новый адрес ноутбука (например, из mDNS) ставится первым.
    pub fn add_address(&self, addr: SocketAddr) -> Result<()> {
        let Some(peer) = self.paired() else { return Ok(()) };
        self.store.lock().unwrap().update(&peer.fingerprint, |p| promote(&mut p.addrs, addr))?;
        self.kick();
        Ok(())
    }

    pub fn unpair(&self) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        let fps: Vec<_> = store.peers().iter().map(|p| p.fingerprint).collect();
        for fp in fps {
            store.remove(&fp)?;
        }
        drop(store);
        if let Some(s) = self.session.lock().unwrap().take() {
            s.close("unpaired");
        }
        Ok(())
    }

    fn endpoint(&self) -> Result<Endpoint> {
        let mut ep = self.endpoint.lock().unwrap();
        if ep.is_none() {
            *ep = Some(transport::client_endpoint()?);
        }
        Ok(ep.clone().unwrap())
    }

    /// Пробует все адреса параллельно, побеждает первый успешный.
    async fn dial(&self, fp: Fingerprint, addrs: &[SocketAddr]) -> Result<(Connection, SocketAddr)> {
        let endpoint = self.endpoint()?;
        let config = transport::client_config(&self.identity, fp)?;
        let mut attempts = tokio::task::JoinSet::new();
        for &addr in addrs {
            let (endpoint, config) = (endpoint.clone(), config.clone());
            attempts.spawn(async move {
                let attempt = async {
                    let connecting = endpoint.connect_with(config, addr, crate::SERVER_NAME)?;
                    anyhow::Ok(connecting.await?)
                };
                match tokio::time::timeout(DIAL_TIMEOUT, attempt).await {
                    Ok(Ok(conn)) => Ok((conn, addr)),
                    Ok(Err(e)) => Err(e.context(format!("{addr}"))),
                    Err(_) => Err(anyhow!("{addr}: таймаут")),
                }
            });
        }
        let mut last_err = anyhow!("нет адресов");
        while let Some(res) = attempts.join_next().await {
            match res? {
                Ok(found) => return Ok(found),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    pub async fn pair(&self, invite: &Invite) -> Result<PeerRecord> {
        let (conn, addr) = self.dial(invite.fingerprint, &invite.addrs).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        let proof = pairing::proof(&invite.secret, &invite.fingerprint, &self.identity.fingerprint());
        let request = Message::PairRequest { hello: self.hello.clone(), proof: ByteBuf::from(proof) };
        frame::write(&mut tx, &Envelope { id: 0, reply_to: None, body: request }).await?;
        let reply = frame::read(&mut rx).await?.context("сервер закрыл соединение")?;
        conn.close(0u32.into(), b"paired");
        let hello = match reply.body {
            Message::PairAccept(hello) => hello,
            Message::PairReject { reason } => bail!("сопряжение отклонено: {reason}"),
            other => bail!("неожиданный ответ: {other:?}"),
        };
        let mut addrs: Vec<String> = invite.addrs.iter().map(|a| a.to_string()).collect();
        promote(&mut addrs, addr);
        let record = PeerRecord {
            fingerprint: invite.fingerprint,
            name: hello.name,
            kind: hello.kind,
            addrs,
            paired_at: now_unix(),
            cert: crate::tls::peer_cert(&conn).map(|c| hex::encode(c.as_ref())),
        };
        self.store.lock().unwrap().upsert(record.clone())?;
        self.kick();
        Ok(record)
    }

    pub fn transfers(&self) -> &Arc<Transfers> {
        &self.transfers
    }

    /// Отправляет файл на ноутбук. Возвращает id передачи после доставки.
    /// Отправляет файл на ноутбук. Если связь рвётся посреди передачи, ждёт
    /// переподключения и продолжает с места обрыва (до `SEND_ATTEMPTS` раз).
    /// Возвращает id передачи после доставки.
    pub async fn send_file(
        &self,
        file: std::fs::File,
        name: &str,
        size: u64,
        mime: &str,
        purpose: Option<&str>,
    ) -> Result<u64> {
        let id = self.transfers.new_id();
        for attempt in 1..=SEND_ATTEMPTS {
            let last = attempt == SEND_ATTEMPTS;
            let session = self.wait_session(None).await.context("нет соединения с ноутбуком")?;
            let reader = tokio::fs::File::from_std(file.try_clone()?);
            match self.transfers.send(&session, id, reader, name, size, mime, purpose, last).await {
                Ok(()) => return Ok(id),
                // Сеанс жив — ошибка не от обрыва связи, повтор не поможет.
                Err(e) if last || session.connection().close_reason().is_none() => return Err(e),
                Err(e) => {
                    tracing::info!(%name, attempt, "связь оборвалась, продолжу после переподключения: {e:#}");
                    if self.wait_session(Some(&session)).await.is_none() {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("последняя попытка всегда возвращает результат")
    }

    /// Ждёт сеанс (новый, если передан старый) до `RESUME_WAIT`.
    async fn wait_session(&self, old: Option<&Arc<Session>>) -> Option<Arc<Session>> {
        let deadline = tokio::time::Instant::now() + RESUME_WAIT;
        loop {
            if let Some(s) = self.session()
                && old.is_none_or(|o| !Arc::ptr_eq(o, &s))
            {
                return Some(s);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            self.kick();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn connect(&self, peer: &PeerRecord) -> Result<(Arc<Session>, mpsc::Receiver<Inbound>)> {
        let addrs: Vec<SocketAddr> = peer.addrs.iter().filter_map(|a| a.parse().ok()).collect();
        let (conn, addr) = self.dial(peer.fingerprint, &addrs).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        let hello = Envelope { id: 0, reply_to: None, body: Message::Hello(self.hello.clone()) };
        frame::write(&mut tx, &hello).await?;
        let reply = frame::read(&mut rx).await?.context("сервер закрыл соединение")?;
        let Message::Hello(server_hello) = reply.body else {
            bail!("ожидался Hello, пришло {:?}", reply.body);
        };
        let cert = crate::tls::peer_cert(&conn).map(|c| hex::encode(c.as_ref()));
        if addrs.first() != Some(&addr) || peer.cert != cert {
            self.store.lock().unwrap().update(&peer.fingerprint, |p| {
                promote(&mut p.addrs, addr);
                p.cert = cert;
            })?;
        }
        Ok(Session::spawn(conn, tx, rx, peer.fingerprint, server_hello, 1))
    }

    /// Бесконечно поддерживает соединение с сопряжённым устройством.
    pub async fn run(&self, events: mpsc::Sender<ClientEvent>) {
        let mut backoff = BACKOFF_MIN;
        let mut reported_down = false;
        loop {
            let Some(peer) = self.paired() else {
                self.kick.notified().await;
                continue;
            };
            match self.connect(&peer).await {
                Ok((session, mut rx)) => {
                    backoff = BACKOFF_MIN;
                    reported_down = false;
                    *self.session.lock().unwrap() = Some(session.clone());
                    let _ = events.send(ClientEvent::Connected(peer.clone())).await;
                    while let Some(inbound) = rx.recv().await {
                        if let Some(env) = self.transfers.handle(&session, inbound) {
                            let _ = events.send(ClientEvent::Message(env)).await;
                        }
                    }
                    self.session.lock().unwrap().take();
                    let reason = session.connection().close_reason();
                    if let Some(quinn::ConnectionError::ApplicationClosed(c)) = &reason
                        && c.error_code == CLOSE_UNPAIRED.into() {
                            let _ = self.store.lock().unwrap().remove(&peer.fingerprint);
                            let _ = events.send(ClientEvent::Unpaired).await;
                            continue;
                        }
                    let reason = reason.map(|r| r.to_string()).unwrap_or_else(|| "закрыто".into());
                    let _ = events.send(ClientEvent::Disconnected(reason)).await;
                    reported_down = true;
                }
                Err(e) => {
                    tracing::debug!("подключение к {}: {e:#}", peer.name);
                    if !reported_down {
                        let _ = events.send(ClientEvent::Disconnected(format!("{e:#}"))).await;
                        reported_down = true;
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.kick.notified() => backoff = BACKOFF_MIN,
                Some(addr) = wait_hint(&peer) => {
                    tracing::info!(%addr, "подсказка адреса от ноутбука");
                    let _ = self.store.lock().unwrap().update(&peer.fingerprint, |p| promote(&mut p.addrs, addr));
                    backoff = BACKOFF_MIN;
                    continue;
                }
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

fn promote(addrs: &mut Vec<String>, addr: SocketAddr) {
    let s = addr.to_string();
    addrs.retain(|a| a != &s);
    addrs.insert(0, s);
}

/// Ждёт подписанную подсказку адреса ноутбука на интерфейсе своей точки доступа.
/// Сокет открыт только пока нет соединения и только если точка доступа включена:
/// в чужих сетях телефон этот порт не слушает.
async fn wait_hint(peer: &PeerRecord) -> Option<SocketAddr> {
    let Some(cert) = peer.cert.as_deref().and_then(|c| hex::decode(c).ok()) else {
        return std::future::pending().await;
    };
    let Some(local) = crate::hint::tether_addrs().into_iter().next() else {
        return std::future::pending().await;
    };
    let socket = match tokio::net::UdpSocket::bind((local, crate::hint::HINT_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("порт подсказок: {e}");
            return std::future::pending().await;
        }
    };
    let mut buf = [0u8; 128];
    loop {
        let Ok((n, src)) = socket.recv_from(&mut buf).await else { continue };
        let SocketAddr::V4(src) = src else { continue };
        if let Some(port) = crate::hint::verify(&buf[..n], *src.ip(), &cert, now_unix()) {
            return Some(SocketAddr::from((*src.ip(), port)));
        }
    }
}
