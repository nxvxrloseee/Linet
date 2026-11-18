//! Установленный сеанс с пиром: управляющий поток, запросы с ответами,
//! автоматический ответ на `Ping`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{mpsc, oneshot};

use crate::frame;
use crate::identity::Fingerprint;
use crate::proto::{Envelope, Hello, Message};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Входящее от пира, кроме ответов на запросы и `Ping`.
#[derive(Debug)]
pub enum Inbound {
    Message(Envelope),
    /// Однонаправленный поток с данными (например, содержимое файла).
    Stream(RecvStream),
}

pub struct Session {
    conn: Connection,
    tx: tokio::sync::Mutex<SendStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Message>>>,
    peer: Fingerprint,
    hello: Hello,
}

impl Session {
    /// Запускает чтение управляющего потока и приём однонаправленных потоков.
    /// Всё, что не является ответом и не `Ping`, приходит в возвращённый канал.
    /// Канал закрывается вместе с сеансом.
    pub fn spawn(
        conn: Connection,
        tx: SendStream,
        mut rx: RecvStream,
        peer: Fingerprint,
        hello: Hello,
        first_id: u64,
    ) -> (Arc<Self>, mpsc::Receiver<Inbound>) {
        let session = Arc::new(Self {
            conn,
            tx: tokio::sync::Mutex::new(tx),
            next_id: AtomicU64::new(first_id),
            pending: Mutex::new(HashMap::new()),
            peer,
            hello,
        });
        // Each message may contain a 12 MiB clipboard image. Keep the queue
        // small so a paired peer cannot retain 64 such allocations (~1 GiB).
        let (events_tx, events_rx) = mpsc::channel(2);
        let streams_conn = session.conn.clone();
        let streams_tx = events_tx.clone();
        tokio::spawn(async move {
            while let Ok(stream) = streams_conn.accept_uni().await {
                if streams_tx.send(Inbound::Stream(stream)).await.is_err() {
                    break;
                }
            }
        });
        let weak = Arc::downgrade(&session);
        tokio::spawn(async move {
            loop {
                let env = match frame::read(&mut rx).await {
                    Ok(Some(env)) => env,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("чтение управляющего потока: {e:#}");
                        break;
                    }
                };
                let Some(session) = weak.upgrade() else { break };
                session.dispatch(env, &events_tx).await;
            }
            if let Some(session) = weak.upgrade() {
                session.pending.lock().unwrap().clear();
                // close() перезаписывает причину закрытия, поэтому только если пир её ещё не задал.
                if session.conn.close_reason().is_none() {
                    session.conn.close(0u32.into(), b"control stream closed");
                }
            }
        });
        (session, events_rx)
    }

    async fn dispatch(&self, env: Envelope, events: &mpsc::Sender<Inbound>) {
        if let Some(to) = env.reply_to
            && let Some(waiter) = self.pending.lock().unwrap().remove(&to) {
                let _ = waiter.send(env.body);
                return;
            }
        match env.body {
            Message::Ping => {
                if let Err(e) = self.reply(env.id, Message::Pong).await {
                    tracing::debug!("ответ на ping: {e:#}");
                }
            }
            // Pong без ожидающего запроса — ответ на ping, отправленный через send().
            Message::Pong => {}
            Message::Unknown => tracing::debug!(id = env.id, "неизвестное сообщение пропущено"),
            _ => {
                // Если получатель не успевает, ждём — это естественный backpressure.
                let _ = events.send(Inbound::Message(env)).await;
            }
        }
    }

    pub fn peer(&self) -> Fingerprint {
        self.peer
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    async fn write(&self, env: Envelope) -> Result<()> {
        let mut tx = self.tx.lock().await;
        frame::write(&mut tx, &env).await
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(2, Ordering::Relaxed)
    }

    pub async fn send(&self, body: Message) -> Result<u64> {
        let id = self.alloc_id();
        self.write(Envelope { id, reply_to: None, body }).await?;
        Ok(id)
    }

    pub async fn reply(&self, to: u64, body: Message) -> Result<()> {
        let id = self.alloc_id();
        self.write(Envelope { id, reply_to: Some(to), body }).await
    }

    pub async fn request(&self, body: Message) -> Result<Message> {
        self.request_with(body, REQUEST_TIMEOUT, true).await
    }

    /// Запрос, ответ на который ждёт решения человека: долгий таймаут,
    /// и его истечение не означает, что соединение мертво.
    pub async fn request_slow(&self, body: Message, timeout: Duration) -> Result<Message> {
        self.request_with(body, timeout, false).await
    }

    async fn request_with(&self, body: Message, timeout: Duration, close_on_timeout: bool) -> Result<Message> {
        let id = self.alloc_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(Envelope { id, reply_to: None, body }).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(_)) => Err(anyhow!("сеанс закрыт")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                if close_on_timeout {
                    // Пир не ответил даже из сна Wi-Fi — считаем соединение мёртвым
                    // (например, процесс приложения убит), чтобы клиент переподключился.
                    self.close("request timeout");
                }
                Err(anyhow!("нет ответа за {timeout:?}"))
            }
        }
    }

    pub async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        match self.request(Message::Ping).await? {
            Message::Pong => Ok(start.elapsed()),
            other => bail!("ожидался Pong, пришло {other:?}"),
        }
    }

    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }

    pub async fn closed(&self) {
        self.conn.closed().await;
    }
}
