//! Передача файлов с докачкой.
//!
//! Отправитель: `FileOffer` → ждёт `FileAccept { offset }` → перематывает файл на
//! `offset` → однонаправленный поток `u64 id ‖ данные` → `finish` и ожидание
//! подтверждения доставки.
//! Получатель от сопряжённых устройств принимает автоматически и пишет во
//! временный `.part`, имя которого выводится из (отправитель, имя, размер). Если
//! передача оборвалась, `.part` остаётся, и повторная отправка того же файла
//! продолжается с места обрыва. Целостность обеспечивает AEAD в QUIC.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use quinn::RecvStream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};

use crate::identity::Fingerprint;
use crate::proto::{Envelope, Message};
use crate::session::Session;

const CHUNK: usize = 64 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Сколько ждать поток после принятого предложения.
const OFFER_TTL: Duration = Duration::from_secs(60);
const MAX_PENDING: usize = 64;
/// Недокачанные файлы старше этого удаляются.
const PART_TTL: Duration = Duration::from_secs(48 * 3600);
/// Лимит размера по умолчанию.
pub const DEFAULT_MAX_SIZE: u64 = 8 << 30;
/// Назначение «обновление приложения» для `FileOffer::purpose`.
pub const PURPOSE_APP_UPDATE: &str = "app-update";

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
    pub purpose: Option<String>,
    pub state: TransferState,
}

pub type ProgressFn = Arc<dyn Fn(TransferUpdate) + Send + Sync>;

struct Offer {
    name: String,
    size: u64,
    peer: String,
    part: PathBuf,
    offset: u64,
    purpose: Option<String>,
    accepted: Instant,
}

pub struct Transfers {
    inbox: PathBuf,
    max_size: u64,
    pending: Mutex<HashMap<u64, Offer>>,
    next_id: AtomicU64,
    progress: ProgressFn,
}

impl Transfers {
    /// `max_size` — самый большой принимаемый файл: защита от заполнения диска.
    pub fn new(inbox: PathBuf, max_size: u64, progress: ProgressFn) -> Arc<Self> {
        let seed = crate::store::now_unix() << 20 | u64::from(std::process::id() & 0xfffff);
        Arc::new(Self {
            inbox,
            max_size,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(seed),
            progress,
        })
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
                Message::FileOffer { id, name, size, mime: _, purpose } => {
                    let reply = self.accept_offer(id, name, size, purpose, session);
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

    fn accept_offer(&self, id: u64, name: String, size: u64, purpose: Option<String>, session: &Session) -> Message {
        let peer = session.hello().name.clone();
        if size > self.max_size {
            tracing::warn!(%peer, %name, size, "файл больше лимита");
            return Message::FileReject { id, reason: format!("файл больше {} МБ", self.max_size >> 20) };
        }
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING {
            return Message::FileReject { id, reason: "слишком много передач сразу".into() };
        }
        pending.retain(|_, o| o.accepted.elapsed() < OFFER_TTL);
        prune_parts(&self.inbox);

        let name = sanitize(&name);
        let part = self.inbox.join(part_name(&session.peer(), &name, size));
        // Тот же файл уже ждёт своего потока — второй раз не берём (и не трогаем его .part).
        if pending.values().any(|o| o.part == part) {
            return Message::FileReject { id, reason: "этот файл уже принимается".into() };
        }
        // Начало этого же файла уже есть — продолжаем с места обрыва.
        let offset = match std::fs::metadata(&part) {
            Ok(m) if m.len() < size => m.len(),
            Ok(_) => {
                let _ = std::fs::remove_file(&part);
                0
            }
            Err(_) => 0,
        };
        tracing::info!(%peer, %name, size, offset, "входящий файл");
        pending.insert(id, Offer { name, size, peer, part, offset, purpose, accepted: Instant::now() });
        Message::FileAccept { id, offset }
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
            done: offer.offset,
            total: offer.size,
            purpose: offer.purpose.clone(),
            state: TransferState::Progress,
        };
        (self.progress)(update.clone());

        tokio::fs::create_dir_all(&self.inbox).await?;
        let result = self.receive_into(&mut stream, &offer, &mut update).await;
        let keep_part = match &result {
            // Оборвалось посреди — оставляем начало для докачки.
            Err(_) => update.done > 0 && update.done < update.total,
            Ok(()) => false,
        };
        update.state = match result {
            Ok(()) => match persist(&offer.part, &self.inbox, &offer.name) {
                Ok(path) => TransferState::Done { path: Some(path) },
                Err(e) => TransferState::Failed(format!("{e:#}")),
            },
            Err(e) => {
                let _ = stream.stop(1u32.into());
                TransferState::Failed(format!("{e:#}"))
            }
        };
        if !matches!(update.state, TransferState::Done { .. }) && !keep_part {
            let _ = tokio::fs::remove_file(&offer.part).await;
        }
        tracing::info!(name = %update.name, state = ?update.state, keep_part, "приём завершён");
        (self.progress)(update);
        Ok(())
    }

    async fn receive_into(&self, stream: &mut RecvStream, offer: &Offer, update: &mut TransferUpdate) -> Result<()> {
        let mut file = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&offer.part).await?;
        file.set_len(offer.offset).await?;
        file.seek(SeekFrom::Start(offer.offset)).await?;
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
        file.flush().await?;
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
    /// `last_attempt == false` — при ошибке не сообщать о провале: вызывающий повторит.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        session: &Session,
        id: u64,
        mut reader: impl AsyncRead + AsyncSeek + Unpin,
        name: &str,
        size: u64,
        mime: &str,
        purpose: Option<&str>,
        last_attempt: bool,
    ) -> Result<()> {
        let mut update = TransferUpdate {
            id,
            peer: session.hello().name.clone(),
            name: name.to_string(),
            incoming: false,
            done: 0,
            total: size,
            purpose: purpose.map(Into::into),
            state: TransferState::Progress,
        };
        (self.progress)(update.clone());

        let result = async {
            let offer = Message::FileOffer {
                id,
                name: name.into(),
                size,
                mime: mime.into(),
                purpose: purpose.map(Into::into),
            };
            let offset = match session.request(offer).await? {
                Message::FileAccept { id: a, offset } if a == id => offset,
                Message::FileReject { reason, .. } => bail!("получатель отказался: {reason}"),
                other => bail!("неожиданный ответ: {other:?}"),
            };
            if offset > size {
                bail!("получатель просит продолжить с {offset} при размере {size}");
            }
            reader.seek(SeekFrom::Start(offset)).await?;
            update.done = offset;
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

        match &result {
            Ok(()) => {
                update.state = TransferState::Done { path: None };
                (self.progress)(update);
            }
            Err(e) if last_attempt => {
                update.state = TransferState::Failed(format!("{e:#}"));
                (self.progress)(update);
            }
            Err(_) => {}
        }
        result
    }
}

/// Сколько раз пробовать отправку, если связь рвётся посреди передачи.
pub const SEND_ATTEMPTS: usize = 3;

/// Имя временного файла: одинаковое для повторной отправки того же файла
/// тем же устройством — так и находится, что докачивать.
fn part_name(peer: &Fingerprint, name: &str, size: u64) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(&peer.0);
    ctx.update(name.as_bytes());
    ctx.update(&size.to_be_bytes());
    format!(".linet-{}.part", hex::encode(&ctx.finish().as_ref()[..12]))
}

/// Удаляет брошенные недокачанные файлы.
fn prune_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(".linet-") && name.ends_with(".part")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > PART_TTL);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
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

/// Переносит `.part` в `dir` под именем `name`, а если оно занято — `name (1)`, `name (2)`, …
/// Жёсткая ссылка создаётся атомарно и не перезаписывает существующий файл,
/// так что два одновременных приёма с одним именем не затрут друг друга.
fn persist(part: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for i in 0..10_000 {
        let candidate = if i == 0 { dir.join(name) } else { dir.join(format!("{stem} ({i}){ext}")) };
        match std::fs::hard_link(part, &candidate) {
            Ok(()) => {
                std::fs::remove_file(part)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // Файловая система без жёстких ссылок: проверка и rename (маленькое окно гонки).
            Err(_) if !candidate.exists() => {
                std::fs::rename(part, &candidate)?;
                return Ok(candidate);
            }
            Err(e) => return Err(e).with_context(|| format!("{candidate:?}")),
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
    fn part_name_is_stable_and_distinct() {
        let (a, b) = (Fingerprint([1; 32]), Fingerprint([2; 32]));
        assert_eq!(part_name(&a, "x.bin", 10), part_name(&a, "x.bin", 10));
        assert_ne!(part_name(&a, "x.bin", 10), part_name(&b, "x.bin", 10));
        assert_ne!(part_name(&a, "x.bin", 10), part_name(&a, "x.bin", 11));
        assert!(part_name(&a, "x.bin", 10).starts_with(".linet-"));
    }

    #[test]
    fn persist_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("linet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"old").unwrap();
        std::fs::write(dir.join("a (1).txt"), b"old").unwrap();
        let part = dir.join(".x.part");
        std::fs::write(&part, b"new").unwrap();
        assert_eq!(persist(&part, &dir, "a.txt").unwrap(), dir.join("a (2).txt"));
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"old");
        assert!(!part.exists());
        std::fs::write(&part, b"new").unwrap();
        assert_eq!(persist(&part, &dir, "b").unwrap(), dir.join("b"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
