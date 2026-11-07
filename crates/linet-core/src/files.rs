//! Передача файлов с докачкой.
//!
//! Отправитель: `FileOffer` → ждёт `FileAccept { offset }` → перематывает файл на
//! `offset` → однонаправленный поток `u64 id ‖ данные` → `finish` и ожидание
//! подтверждения проверки и сохранения (`FileResult`) у нового получателя.
//! Получатель от сопряжённых устройств принимает автоматически и пишет во
//! временный `.part`, имя которого выводится из (отправитель, имя, размер, SHA-256). Если
//! передача оборвалась, `.part` остаётся, и повторная отправка того же файла
//! продолжается с места обрыва. SHA-256 проверяется перед сохранением; старые
//! отправители передают без докачки.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use quinn::RecvStream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::oneshot;

use crate::identity::Fingerprint;
use crate::proto::{Envelope, Message};
use crate::session::Session;

const CHUNK: usize = 64 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Сколько ждать поток после принятого предложения.
const OFFER_TTL: Duration = Duration::from_secs(60);
const MAX_PENDING: usize = 64;
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);
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

#[derive(Clone)]
struct Offer {
    name: String,
    size: u64,
    peer: String,
    part: PathBuf,
    offset: u64,
    purpose: Option<String>,
    accepted: Instant,
    sha256: Option<String>,
    active: bool,
    connection: quinn::Connection,
}

type TransferKey = (usize, u64);
type ResultWaiters = HashMap<TransferKey, oneshot::Sender<Option<String>>>;

pub struct Transfers {
    inbox: PathBuf,
    max_size: u64,
    pending: Mutex<HashMap<TransferKey, Offer>>,
    results: Mutex<ResultWaiters>,
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
            results: Mutex::new(HashMap::new()),
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
                let session = session.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.receive(&session, stream).await {
                        tracing::warn!("приём файла: {e:#}");
                    }
                });
                None
            }
            crate::Inbound::Message(env) => match env.body {
                Message::FileResult { id, error } => {
                    if let Some(waiter) = self.results.lock().unwrap().remove(&(session.connection().stable_id(), id)) {
                        let _ = waiter.send(error);
                    }
                    None
                }
                Message::FileOffer { id, name, size, mime: _, purpose, sha256 } => {
                    let reply = self.accept_offer(id, name, size, purpose, sha256, session);
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

    #[allow(clippy::too_many_arguments)]
    fn accept_offer(&self, id: u64, name: String, size: u64, purpose: Option<String>, sha256: Option<String>, session: &Session) -> Message {
        let peer = session.hello().name.clone();
        if size > self.max_size {
            tracing::warn!(%peer, %name, size, "файл больше лимита");
            return Message::FileReject { id, reason: format!("файл больше {} МБ", self.max_size >> 20) };
        }
        if sha256.as_ref().is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Message::FileReject { id, reason: "неверный SHA-256".into() };
        }
        let sha256 = sha256.map(|h| h.to_ascii_lowercase());
        let key = (session.connection().stable_id(), id);
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, o| o.active || (o.accepted.elapsed() < OFFER_TTL && o.connection.close_reason().is_none()));
        if pending.contains_key(&key) {
            return Message::FileReject { id, reason: "id передачи уже занят".into() };
        }
        if pending.len() >= MAX_PENDING {
            return Message::FileReject { id, reason: "слишком много передач сразу".into() };
        }
        prune_parts(&self.inbox, pending.values().map(|o| o.part.as_path()));

        let name = sanitize(&name);
        let part = self.inbox.join(part_name(&session.peer(), &name, size, sha256.as_deref()));
        // Тот же файл уже ждёт своего потока — второй раз не берём (и не трогаем его .part).
        if pending.values().any(|o| o.part == part) {
            return Message::FileReject { id, reason: "этот файл уже принимается".into() };
        }
        // Начало этого же файла уже есть — продолжаем с места обрыва.
        let offset = match std::fs::metadata(&part) {
            Ok(m) if sha256.is_some() && m.len() <= size => m.len(),
            Ok(_) => {
                let _ = std::fs::remove_file(&part);
                0
            }
            Err(_) => 0,
        };
        tracing::info!(%peer, %name, size, offset, "входящий файл");
        pending.insert(key, Offer {
            name, size, peer, part, offset, purpose, accepted: Instant::now(),
            sha256: sha256.clone(), active: false, connection: session.connection().clone(),
        });
        Message::FileAccept { id, offset, sha256 }
    }

    async fn receive(&self, session: &Session, mut stream: RecvStream) -> Result<()> {
        let mut header = [0u8; 8];
        stream.read_exact(&mut header).await?;
        let id = u64::from_be_bytes(header);
        let key = (session.connection().stable_id(), id);
        let offer = {
            let mut pending = self.pending.lock().unwrap();
            let offer = pending.get_mut(&key).context("поток без принятого предложения")?;
            if offer.active || offer.accepted.elapsed() >= OFFER_TTL {
                bail!("поток повторный или предложение просрочено");
            }
            offer.active = true;
            offer.clone()
        };
        let reservation = IncomingReservation { transfers: self, key };

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

        let result = async {
            tokio::fs::create_dir_all(&self.inbox).await?;
            self.receive_into(&mut stream, &offer, &mut update).await
        }.await;
        let keep_part = match &result {
            // Оборвалось посреди — оставляем начало для докачки.
            Err(_) => update.done > 0 && update.done < update.total,
            // Полностью проверенный файл не удаляем, если не удалось подобрать имя/сохранить.
            Ok(()) => true,
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
        let error = match &update.state {
            TransferState::Failed(error) => Some(error.clone()),
            _ => None,
        };
        // Освобождаем путь до подтверждения: следующая передача уже может начаться.
        drop(reservation);
        (self.progress)(update);
        if offer.sha256.is_some() {
            session.send(Message::FileResult { id, error }).await?;
        }
        Ok(())
    }

    async fn receive_into(&self, stream: &mut RecvStream, offer: &Offer, update: &mut TransferUpdate) -> Result<()> {
        let mut file = tokio::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&offer.part).await?;
        file.set_len(offer.offset).await?;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        // Учитываем в хеше и уже сохранённый префикс.
        hash_into(&mut (&mut file).take(offer.offset), &mut hash).await?;
        file.seek(SeekFrom::Start(offer.offset)).await?;
        let mut last = Instant::now();
        while let Some(chunk) = stream.read_chunk(CHUNK, true).await? {
            update.done += chunk.bytes.len() as u64;
            if update.done > update.total {
                bail!("пришло больше заявленного размера");
            }
            file.write_all(&chunk.bytes).await?;
            hash.update(&chunk.bytes);
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
        if let Some(expected) = &offer.sha256 {
            let actual = hex::encode(hash.finish());
            if &actual != expected {
                bail!("SHA-256 не совпал: файл изменился или повреждён");
            }
        }
        Ok(())
    }

    pub fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Отправляет файл и ждёт, пока получатель подтвердит доставку всех данных.
    /// `id` берётся из [`Transfers::new_id`], чтобы вызывающий знал его заранее.
    /// Конечную ошибку после повторов сообщает вызывающий через `fail_outgoing`.
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

        let key = (session.connection().stable_id(), id);
        let (result_tx, result_rx) = oneshot::channel();
        self.results.lock().unwrap().insert(key, result_tx);
        let _reservation = OutgoingReservation { transfers: self, key };
        let result = async {
            reader.seek(SeekFrom::Start(0)).await?;
            let (sha256, actual_size) = hash_reader(&mut reader).await?;
            if actual_size != size {
                bail!("файл изменился: {actual_size} вместо {size} байт");
            }
            let offer = Message::FileOffer {
                id,
                name: name.into(),
                size,
                mime: mime.into(),
                sha256: Some(sha256.clone()),
                purpose: purpose.map(Into::into),
            };
            let (offset, verified) = match session.request(offer).await? {
                Message::FileAccept { id: a, offset, sha256: accepted_hash } if a == id => {
                    if offset > 0 && accepted_hash.as_deref() != Some(sha256.as_str()) {
                        bail!("получатель не поддерживает безопасную докачку; обнови Linet на получателе");
                    }
                    (offset, accepted_hash.as_deref() == Some(sha256.as_str()))
                },
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
            if verified {
                let result = tokio::select! {
                    result = tokio::time::timeout(RESULT_TIMEOUT, result_rx) => result,
                    _ = session.closed() => bail!("сеанс закрыт до подтверждения сохранения"),
                };
                match result {
                    Ok(Ok(None)) => {},
                    Ok(Ok(Some(error))) => bail!("получатель не сохранил файл: {error}"),
                    Ok(Err(_)) => bail!("сеанс закрыт до подтверждения сохранения"),
                    Err(_) => bail!("нет подтверждения сохранения файла"),
                }
            }
            anyhow::Ok(())
        }
        .await;

        if result.is_ok() {
            update.state = TransferState::Done { path: None };
            (self.progress)(update);
        }
        result
    }

    pub fn fail_outgoing(&self, mut update: TransferUpdate, error: &anyhow::Error) {
        update.state = TransferState::Failed(format!("{error:#}"));
        (self.progress)(update);
    }
}

/// Резерв остаётся до завершения записи и освобождается также при отмене задачи.
struct IncomingReservation<'a> {
    transfers: &'a Transfers,
    key: TransferKey,
}

impl Drop for IncomingReservation<'_> {
    fn drop(&mut self) {
        self.transfers.pending.lock().unwrap().remove(&self.key);
    }
}

struct OutgoingReservation<'a> {
    transfers: &'a Transfers,
    key: TransferKey,
}

impl Drop for OutgoingReservation<'_> {
    fn drop(&mut self) {
        self.transfers.results.lock().unwrap().remove(&self.key);
    }
}

async fn hash_reader(reader: &mut (impl AsyncRead + Unpin)) -> Result<(String, u64)> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let size = hash_into(reader, &mut hash).await?;
    Ok((hex::encode(hash.finish()), size))
}

async fn hash_into(reader: &mut (impl AsyncRead + Unpin), hash: &mut ring::digest::Context) -> Result<u64> {
    let mut buffer = vec![0; CHUNK];
    let mut size = 0;
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 { break; }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok(size)
}

/// Сколько раз пробовать отправку, если связь рвётся посреди передачи.
pub const SEND_ATTEMPTS: usize = 3;

/// Имя временного файла: одинаковое для повторной отправки того же файла
/// тем же устройством — так и находится, что докачивать.
fn part_name(peer: &Fingerprint, name: &str, size: u64, sha256: Option<&str>) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(&peer.0);
    ctx.update(name.as_bytes());
    ctx.update(&size.to_be_bytes());
    if let Some(hash) = sha256 { ctx.update(hash.as_bytes()); }
    format!(".linet-{}.part", hex::encode(&ctx.finish().as_ref()[..12]))
}

/// Удаляет брошенные недокачанные файлы.
fn prune_parts<'a>(dir: &Path, reserved: impl Iterator<Item = &'a Path>) {
    let reserved: std::collections::HashSet<_> = reserved.collect();
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if reserved.contains(e.path().as_path()) { continue; }
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
        assert_eq!(part_name(&a, "x.bin", 10, None), part_name(&a, "x.bin", 10, None));
        assert_ne!(part_name(&a, "x.bin", 10, None), part_name(&b, "x.bin", 10, None));
        assert_ne!(part_name(&a, "x.bin", 10, None), part_name(&a, "x.bin", 11, None));
        assert!(part_name(&a, "x.bin", 10, None).starts_with(".linet-"));
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

#[cfg(test)]
#[path = "files_regression.rs"]
mod regression;
