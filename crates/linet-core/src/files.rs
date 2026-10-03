//! Передача файлов.
//!
//! Отправитель: `FileOffer` → ждёт `FileAccept` → открывает однонаправленный
//! поток `u64 id ‖ данные` → `finish` и ожидание подтверждения доставки.
//! Получатель от сопряжённых устройств принимает автоматически, пишет во
//! временный `.part` и переименовывает после проверки размера.
//! Целостность обеспечивает AEAD в QUIC, отдельный хеш не нужен.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quinn::RecvStream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::proto::{Envelope, Message};
use crate::session::Session;

const CHUNK: usize = 64 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Сколько ждать поток после принятого предложения.
const OFFER_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub enum TransferState {
    Progress,
    Done { path: Option<PathBuf> },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TransferUpdate {
    pub id: u64,
    pub peer: String,
    pub name: String,
    pub incoming: bool,
    pub done: u64,
    pub total: u64,
    pub state: TransferState,
}

pub type ProgressFn = Arc<dyn Fn(TransferUpdate) + Send + Sync>;

struct Offer {
    name: String,
    size: u64,
    peer: String,
    accepted: Instant,
}

pub struct Transfers {
    inbox: PathBuf,
    pending: Mutex<HashMap<u64, Offer>>,
    next_id: AtomicU64,
    progress: ProgressFn,
}

impl Transfers {
    pub fn new(inbox: PathBuf, progress: ProgressFn) -> Arc<Self> {
        let seed = crate::store::now_unix() << 20 | u64::from(std::process::id() & 0xfffff);
        Arc::new(Self { inbox, pending: Mutex::new(HashMap::new()), next_id: AtomicU64::new(seed), progress })
    }

    pub fn inbox(&self) -> &Path {
        &self.inbox
    }

    /// Забирает входящее, если оно про файлы; остальные сообщения возвращает.
    pub fn handle(self: &Arc<Self>, session: &Arc<Session>, inbound: crate::Inbound) -> Option<Envelope> {
        match inbound {
            crate::Inbound::Stream(stream) => {
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.receive(stream).await {
                        tracing::warn!("приём файла: {e:#}");
                    }
                });
                None
            }
            crate::Inbound::Message(env) => match env.body {
                Message::FileOffer { id, name, size, mime: _ } => {
                    let reply = self.accept_offer(id, name, size, &session.hello().name);
                    let session = session.clone();
                    tokio::spawn(async move {
                        if let Err(e) = session.reply(env.id, reply).await {
                            tracing::debug!("ответ на FileOffer: {e:#}");
                        }
                    });
                    None
                }
                _ => Some(env),
            },
        }
    }

    fn accept_offer(&self, id: u64, name: String, size: u64, peer: &str) -> Message {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, o| o.accepted.elapsed() < OFFER_TTL);
        let name = sanitize(&name);
        tracing::info!(%peer, %name, size, "входящий файл");
        pending.insert(id, Offer { name, size, peer: peer.to_string(), accepted: Instant::now() });
        Message::FileAccept { id }
    }

    async fn receive(&self, mut stream: RecvStream) -> Result<()> {
        let mut header = [0u8; 8];
        stream.read_exact(&mut header).await?;
        let id = u64::from_be_bytes(header);
        let offer = self.pending.lock().unwrap().remove(&id).context("поток без принятого предложения")?;

        let mut update = TransferUpdate {
            id,
            peer: offer.peer.clone(),
            name: offer.name.clone(),
            incoming: true,
            done: 0,
            total: offer.size,
            state: TransferState::Progress,
        };
        (self.progress)(update.clone());

        tokio::fs::create_dir_all(&self.inbox).await?;
        let part = self.inbox.join(format!(".linet-{id}.part"));
        let result = self.receive_into(&mut stream, &part, &mut update).await;
        update.state = match result {
            Ok(()) => match unique_path(&self.inbox, &offer.name) {
                Ok(path) => match tokio::fs::rename(&part, &path).await {
                    Ok(()) => TransferState::Done { path: Some(path) },
                    Err(e) => TransferState::Failed(e.to_string()),
                },
                Err(e) => TransferState::Failed(format!("{e:#}")),
            },
            Err(e) => {
                let _ = stream.stop(1u32.into());
                TransferState::Failed(format!("{e:#}"))
            }
        };
        if !matches!(update.state, TransferState::Done { .. }) {
            let _ = tokio::fs::remove_file(&part).await;
        }
        tracing::info!(name = %update.name, state = ?update.state, "приём завершён");
        (self.progress)(update);
        Ok(())
    }

    async fn receive_into(&self, stream: &mut RecvStream, part: &Path, update: &mut TransferUpdate) -> Result<()> {
        let mut file = tokio::fs::File::create(part).await?;
        let mut last = Instant::now();
        while let Some(chunk) = stream.read_chunk(CHUNK, true).await? {
            update.done += chunk.bytes.len() as u64;
            if update.done > update.total {
                bail!("пришло больше заявленного размера");
            }
            file.write_all(&chunk.bytes).await?;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                (self.progress)(update.clone());
            }
        }
        if update.done != update.total {
            bail!("получено {} из {} байт", update.done, update.total);
        }
        file.sync_all().await?;
        Ok(())
    }

    pub fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Отправляет файл и ждёт, пока получатель подтвердит доставку всех данных.
    /// `id` берётся из [`Transfers::new_id`], чтобы вызывающий знал его заранее.
    pub async fn send(
        &self,
        session: &Session,
        id: u64,
        mut reader: impl AsyncRead + Unpin,
        name: &str,
        size: u64,
        mime: &str,
    ) -> Result<()> {
        let mut update = TransferUpdate {
            id,
            peer: session.hello().name.clone(),
            name: name.to_string(),
            incoming: false,
            done: 0,
            total: size,
            state: TransferState::Progress,
        };
        (self.progress)(update.clone());

        let result = async {
            let offer = Message::FileOffer { id, name: name.into(), size, mime: mime.into() };
            match session.request(offer).await? {
                Message::FileAccept { id: a } if a == id => {}
                Message::FileReject { reason, .. } => bail!("получатель отказался: {reason}"),
                other => bail!("неожиданный ответ: {other:?}"),
            }
            let mut stream = session.connection().open_uni().await?;
            stream.write_all(&id.to_be_bytes()).await?;
            let mut buf = vec![0u8; CHUNK];
            let mut last = Instant::now();
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                stream.write_all(&buf[..n]).await?;
                update.done += n as u64;
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    (self.progress)(update.clone());
                }
            }
            if update.done != size {
                bail!("файл изменился во время отправки: {} вместо {size} байт", update.done);
            }
            stream.finish()?;
            if let Some(code) = stream.stopped().await? {
                bail!("получатель прервал приём (код {code})");
            }
            anyhow::Ok(())
        }
        .await;

        update.state = match &result {
            Ok(()) => TransferState::Done { path: None },
            Err(e) => TransferState::Failed(format!("{e:#}")),
        };
        (self.progress)(update);
        result
    }
}

/// Имя файла без путей и управляющих символов.
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    let mut out: String = cleaned.chars().take(200).collect();
    if out.is_empty() {
        out = "file".into();
    }
    out
}

/// `name`, а если занято — `name (1)`, `name (2)`, …
fn unique_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for i in 1..10_000 {
        let p = dir.join(format!("{stem} ({i}){ext}"));
        if !p.exists() {
            return Ok(p);
        }
    }
    bail!("не удалось подобрать имя для {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("C:\\Users\\x\\photo.jpg"), "photo.jpg");
        assert_eq!(sanitize(".bashrc"), "bashrc");
        assert_eq!(sanitize("a\nb.txt"), "ab.txt");
        assert_eq!(sanitize(""), "file");
        assert_eq!(sanitize(".."), "file");
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("linet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"").unwrap();
        std::fs::write(dir.join("a (1).txt"), b"").unwrap();
        assert_eq!(unique_path(&dir, "a.txt").unwrap(), dir.join("a (2).txt"));
        assert_eq!(unique_path(&dir, "b").unwrap(), dir.join("b"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
