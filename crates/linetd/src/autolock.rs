//! Автоблокировка: телефон ушёл — ноутбук заблокировался.
//!
//! Пока ты работаешь за ноутбуком, телефон не трогаем вовсе. Только когда ввода
//! нет дольше `IDLE_AFTER`, ноутбук раз в `PROBE_EVERY` пингует телефон; не ответил
//! (вне сети, выключен) — экран блокируется. Если телефона не было уже в момент,
//! когда ты отошёл, ничего не делаем: значит, ты без него и работаешь.

use std::sync::Arc;
use std::time::Duration;

use crate::daemon::Daemon;
use crate::unlock;

const IDLE_AFTER: Duration = Duration::from_secs(60);
const PROBE_EVERY: Duration = Duration::from_secs(20);

pub fn spawn(daemon: Arc<Daemon>) {
    let mut idle = crate::idle::watch(IDLE_AFTER);
    tokio::spawn(async move {
        loop {
            if idle.wait_for(|i| *i).await.is_err() {
                return;
            }
            let Some(phone) = daemon.phone_session().map(|s| s.peer()) else {
                // Отошёл без телефона рядом — блокировать не из-за чего.
                let _ = idle.wait_for(|i| !*i).await;
                continue;
            };
            loop {
                tokio::select! {
                    res = idle.wait_for(|i| !*i) => {
                        if res.is_err() { return; }
                        break;
                    }
                    _ = tokio::time::sleep(PROBE_EVERY) => {}
                }
                // Экран уже заблокирован (например, по таймеру Ryoku) — проверять незачем.
                if unlock::state().await != unlock::LockState::Unlocked {
                    break;
                }
                let alive = match daemon.session_of(&phone) {
                    Some(s) => s.ping().await.is_ok(),
                    None => false,
                };
                if !alive {
                    tracing::info!("телефон пропал из сети, блокирую экран");
                    if let Err(e) = unlock::lock().await {
                        tracing::warn!("автоблокировка: {e:#}");
                    }
                    break;
                }
            }
            let _ = idle.wait_for(|i| !*i).await;
        }
    });
}
