//! Разблокировка экрана телефоном.
//!
//! PAM не нужен: экран блокировки Caelestia снимается его же IPC
//! (`qs -c caelestia ipc call lock unlock`), что любой процесс пользователя может
//! сделать и без Linet. Linet лишь решает, когда это делать: по согласию
//! сопряжённого телефона, полученному по mTLS-каналу. Пароль работает всегда.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::StreamExt;
use linet_core::Message;
use tokio::process::Command;

use crate::daemon::Daemon;

/// Сколько ждать решения на телефоне (уведомление с подтверждением).
const DECISION_TIMEOUT: Duration = Duration::from_secs(30);
/// После выхода из сна телефону нужно время переподключиться.
const RECONNECT_WAIT: Duration = Duration::from_secs(15);
const NUDGE_EVERY: Duration = Duration::from_millis(300);
const NUDGES: usize = 40;

pub struct Unlocker {
    daemon: Arc<Daemon>,
    busy: AtomicBool,
}

impl Unlocker {
    pub fn new(daemon: Arc<Daemon>) -> Arc<Self> {
        Arc::new(Self { daemon, busy: AtomicBool::new(false) })
    }

    /// Попытка разблокировки. `Ok(false)` — не заблокировано, уже идёт попытка или отказ.
    pub async fn attempt(&self, why: &str) -> Result<bool> {
        if !is_locked().await {
            return Ok(false);
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        let result = self.ask_and_unlock(why).await;
        self.busy.store(false, Ordering::Release);
        result
    }

    async fn ask_and_unlock(&self, why: &str) -> Result<bool> {
        let session = self.daemon.wait_session(RECONNECT_WAIT).await?;
        tracing::info!(why, phone = %session.hello().name, "запрос разблокировки");
        let request = Message::UnlockRequest { host: self.daemon.hello().name.clone() };
        // После сна сеть поднимается не сразу, а QUIC повторяет потерянные пакеты
        // с нарастающими паузами (до ~10 с). Частые мелкие пакеты дают ему узнать,
        // что сеть ожила, и сразу переслать запрос.
        let nudger = {
            let session = session.clone();
            tokio::spawn(async move {
                for _ in 0..NUDGES {
                    tokio::time::sleep(NUDGE_EVERY).await;
                    let _ = session.send(Message::Ping).await;
                }
            })
        };
        let answer = session.request_slow(request, DECISION_TIMEOUT).await;
        nudger.abort();
        match answer? {
            Message::UnlockResponse { approved: true, reason } => {
                // Пока ждали, могли ввести пароль.
                if !is_locked().await {
                    return Ok(false);
                }
                unlock().await?;
                tracing::info!(%reason, "экран разблокирован телефоном");
                Ok(true)
            }
            Message::UnlockResponse { approved: false, reason } => {
                tracing::info!(%reason, "телефон отклонил разблокировку");
                Ok(false)
            }
            other => bail!("неожиданный ответ: {other:?}"),
        }
    }

    /// Открытие крышки и выход из сна — моменты, когда ты садишься за ноутбук.
    pub fn watch(self: Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.watch_sleep().await {
                tracing::warn!("logind недоступен: {e:#}");
            }
        });
        tokio::spawn(async move {
            if let Err(e) = self.watch_lid().await {
                tracing::warn!("UPower недоступен: {e:#}");
            }
        });
    }

    async fn watch_sleep(self: Arc<Self>) -> Result<()> {
        let conn = zbus::Connection::system().await?;
        let logind = LoginManagerProxy::new(&conn).await?;
        let mut signals = logind.receive_prepare_for_sleep().await?;
        while let Some(sig) = signals.next().await {
            if !sig.args()?.start {
                self.spawn_attempt("выход из сна");
            }
        }
        Ok(())
    }

    async fn watch_lid(self: Arc<Self>) -> Result<()> {
        let conn = zbus::Connection::system().await?;
        let upower = UPowerProxy::new(&conn).await?;
        if !upower.lid_is_present().await? {
            return Ok(());
        }
        let mut changes = upower.receive_lid_is_closed_changed().await;
        while let Some(change) = changes.next().await {
            if !change.get().await? {
                self.spawn_attempt("крышка открыта");
            }
        }
        Ok(())
    }

    fn spawn_attempt(self: &Arc<Self>, why: &'static str) {
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.attempt(why).await {
                tracing::info!(why, "разблокировка не удалась: {e:#}");
            }
        });
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    #[zbus(property)]
    fn lid_is_present(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn lid_is_closed(&self) -> zbus::Result<bool>;
}

async fn caelestia(call: &str) -> Option<String> {
    let out = Command::new("qs").args(["-c", "caelestia", "ipc", "call", "lock", call]).output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn hyprlock_running() -> bool {
    Command::new("pgrep").args(["-x", "hyprlock"]).output().await.is_ok_and(|o| o.status.success())
}

pub async fn is_locked() -> bool {
    match caelestia("isLocked").await {
        Some(answer) => answer == "true",
        None => hyprlock_running().await,
    }
}

/// Заблокировать экран (Caelestia, иначе через logind — его слушают hyprlock и др.).
pub async fn lock() -> Result<()> {
    if caelestia("lock").await.is_some() {
        return Ok(());
    }
    let status = Command::new("loginctl").arg("lock-session").status().await?;
    if !status.success() {
        bail!("loginctl lock-session не сработал");
    }
    Ok(())
}

async fn unlock() -> Result<()> {
    if caelestia("isLocked").await.is_some() {
        caelestia("unlock").await.ok_or_else(|| anyhow::anyhow!("Caelestia не разблокировал экран"))?;
        return Ok(());
    }
    // hyprlock снимает блокировку по SIGUSR1.
    let status = Command::new("pkill").args(["-USR1", "-x", "hyprlock"]).status().await?;
    if !status.success() {
        bail!("не найден экран блокировки");
    }
    Ok(())
}
