//! Буфер обмена Wayland через wl-clipboard (протокол data-control, Hyprland его поддерживает).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Больше в буфер обычно кладут случайно (лог, дамп); такое не синхронизируем.
pub const MAX_TEXT: usize = 256 * 1024;
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// На каждое изменение wl-paste запускает эту команду с содержимым на stdin:
/// `состояние\n` + текст + `\0`. NUL в text/plain не встречается.
const WATCH_SCRIPT: &str = r#"printf '%s\n' "$CLIPBOARD_STATE"; cat; printf '\0'"#;

pub struct Clipboard {
    /// Хеш последнего синхронизированного текста — защита от эха.
    last: Mutex<u64>,
}

fn hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl Clipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { last: Mutex::new(0) })
    }

    /// Запоминает текст; `false`, если он уже был последним (эхо или повтор).
    fn remember(&self, text: &str) -> bool {
        let h = hash(text);
        let mut last = self.last.lock().unwrap();
        if *last == h {
            return false;
        }
        *last = h;
        true
    }

    /// Следит за буфером и вызывает `on_change` для нового текста, скопированного на ноутбуке.
    pub fn watch(self: Arc<Self>, on_change: impl Fn(String) + Send + Sync + 'static) {
        tokio::spawn(async move {
            loop {
                match self.run_watcher(&on_change).await {
                    Ok(()) => tracing::debug!("wl-paste завершился, перезапуск"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        tracing::warn!("wl-paste не найден, синхронизация буфера отключена");
                        return;
                    }
                    Err(e) => tracing::warn!("wl-paste: {e}"),
                }
                tokio::time::sleep(RESTART_DELAY).await;
            }
        });
    }

    async fn run_watcher(&self, on_change: &(impl Fn(String) + Send + Sync)) -> std::io::Result<()> {
        let mut child = Command::new("wl-paste")
            .args(["--type", "text", "--watch", "sh", "-c", WATCH_SCRIPT])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut out = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut first = true;
        loop {
            let mut state = String::new();
            if out.read_line(&mut state).await? == 0 {
                break;
            }
            let mut body = Vec::new();
            if out.read_until(0, &mut body).await? == 0 {
                break;
            }
            body.pop();
            // Первое событие — то, что лежало в буфере до запуска: это не новое копирование.
            if std::mem::take(&mut first) {
                if let Ok(text) = String::from_utf8(body) {
                    self.remember(&text);
                }
                continue;
            }
            if state.trim() != "data" {
                // "sensitive" — пароль из менеджера паролей, "nil"/"clear" — пусто.
                continue;
            }
            let Ok(text) = String::from_utf8(body) else { continue };
            if text.trim().is_empty() || text.len() > MAX_TEXT || !self.remember(&text) {
                continue;
            }
            on_change(text);
        }
        child.wait().await?;
        Ok(())
    }

    /// Кладёт текст с телефона в буфер ноутбука.
    pub async fn set(&self, text: String) {
        if text.len() > MAX_TEXT || !self.remember(&text) {
            return;
        }
        let child = Command::new("wl-copy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("wl-copy: {e}");
                return;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        // wl-copy форкается и сам обслуживает буфер; родитель завершается сразу.
        let _ = child.wait().await;
    }
}
