//! Буфер обмена Wayland через wl-clipboard (протокол data-control, Hyprland его поддерживает).
//! Синхронизируются текст и картинки.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use linet_core::proto::MAX_CLIPBOARD_IMAGE;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Больше в буфер обычно кладут случайно (лог, дамп); такое не синхронизируем.
pub const MAX_TEXT: usize = 256 * 1024;
const RESTART_DELAY: Duration = Duration::from_secs(5);
const CLEAR_POLL_EVERY: Duration = Duration::from_secs(5);
/// 2 минуты — столько телефон готов очищать полученный текст.
const CLEAR_POLLS: usize = 24;

/// Что поменялось в буфере ноутбука.
pub enum ClipEvent {
    Text(String),
    Image { mime: String, data: Vec<u8> },
    /// Буфер очищен — очистить и копию на телефоне.
    Cleared,
}

type OnChange = Arc<dyn Fn(ClipEvent) + Send + Sync>;

/// Текст: wl-paste на каждое изменение запускает команду с содержимым на stdin —
/// `состояние\n` + текст + `\0`. NUL в text/plain не встречается.
const TEXT_SCRIPT: &str = r#"printf '%s\n' "$CLIPBOARD_STATE"; cat; printf '\0'"#;

/// Картинка: в двоичных данных NUL бывает, поэтому сначала строка-заголовок
/// `состояние размер mime\n`, затем ровно `размер` байт.
const IMAGE_SCRIPT: &str = r#"f=$(mktemp) || exit 0; cat > "$f"
mime=$(wl-paste --list-types 2>/dev/null | grep -m1 '^image/')
printf '%s %s %s\n' "$CLIPBOARD_STATE" "$(stat -c %s "$f")" "${mime:-image/png}"
cat "$f"; rm -f "$f""#;

pub struct Clipboard {
    /// Хеш последнего синхронизированного содержимого — защита от эха.
    last: Mutex<u64>,
}

fn hash(data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

impl Clipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { last: Mutex::new(0) })
    }

    /// Запоминает содержимое; `false`, если оно уже было последним (эхо или повтор).
    fn remember(&self, data: &[u8]) -> bool {
        let h = hash(data);
        let mut last = self.last.lock().unwrap();
        if *last == h {
            return false;
        }
        *last = h;
        true
    }

    /// Следит за буфером и вызывает `on_change` для нового содержимого, скопированного на ноутбуке.
    pub fn watch(self: Arc<Self>, on_change: impl Fn(ClipEvent) + Send + Sync + 'static) {
        let on_change: OnChange = Arc::new(on_change);
        for images in [false, true] {
            let (this, on_change) = (self.clone(), on_change.clone());
            tokio::spawn(async move {
                loop {
                    let res = if images {
                        this.run_image_watcher(&on_change).await
                    } else {
                        this.run_text_watcher(&on_change).await
                    };
                    match res {
                        Ok(()) => tracing::debug!(images, "wl-paste завершился, перезапуск"),
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
    }

    /// Буфер очищен — очищаем и копию на телефоне, если там лежит текст отсюда.
    fn cleared(&self, on_change: &OnChange) {
        if std::mem::take(&mut *self.last.lock().unwrap()) != 0 {
            tracing::info!("буфер очищен, очищаю копию на телефоне");
            on_change(ClipEvent::Cleared);
        }
    }

    fn spawn_watcher(kind: &str, script: &str) -> std::io::Result<tokio::process::Child> {
        Command::new("wl-paste")
            .args(["--type", kind, "--watch", "sh", "-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }

    async fn run_text_watcher(self: &Arc<Self>, on_change: &OnChange) -> std::io::Result<()> {
        let mut child = Self::spawn_watcher("text", TEXT_SCRIPT)?;
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
                self.remember(&body);
                continue;
            }
            // "sensitive" — пароль из менеджера паролей с пометкой: не пересылаем.
            if state.trim() != "data" {
                continue;
            }
            let Ok(text) = String::from_utf8(body) else { continue };
            // Многие менеджеры паролей «очищают» буфер, записывая пустую строку.
            if text.trim().is_empty() {
                self.cleared(on_change);
                continue;
            }
            if text.len() > MAX_TEXT || !self.remember(text.as_bytes()) {
                continue;
            }
            on_change(ClipEvent::Text(text));
            self.clone().watch_for_clear(on_change.clone());
        }
        child.wait().await?;
        Ok(())
    }

    async fn run_image_watcher(self: &Arc<Self>, on_change: &OnChange) -> std::io::Result<()> {
        let mut child = Self::spawn_watcher("image", IMAGE_SCRIPT)?;
        let mut out = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut first = true;
        loop {
            let mut header = String::new();
            if out.read_line(&mut header).await? == 0 {
                break;
            }
            let mut parts = header.split_whitespace();
            let (Some(state), Some(size), Some(mime)) = (parts.next(), parts.next(), parts.next()) else {
                // Поток рассинхронизировался — перезапускаем наблюдателя.
                break;
            };
            let Ok(size) = size.parse::<usize>() else { break };
            let mut data = vec![0u8; size];
            out.read_exact(&mut data).await?;
            if std::mem::take(&mut first) {
                self.remember(&data);
                continue;
            }
            if state != "data" || data.is_empty() || data.len() > MAX_CLIPBOARD_IMAGE || !self.remember(&data) {
                continue;
            }
            tracing::debug!(%mime, size, "картинка в буфере");
            on_change(ClipEvent::Image { mime: mime.to_string(), data });
        }
        child.wait().await?;
        Ok(())
    }

    /// Настоящую очистку (`wl-copy --clear`) wl-paste --watch не сообщает, поэтому
    /// после отправки текста какое-то время проверяем буфер сами. Дольше нет смысла:
    /// телефон очищает свою копию только в течение того же окна.
    fn watch_for_clear(self: Arc<Self>, on_change: OnChange) {
        let sent = *self.last.lock().unwrap();
        tokio::spawn(async move {
            for _ in 0..CLEAR_POLLS {
                tokio::time::sleep(CLEAR_POLL_EVERY).await;
                // Скопировали что-то новое — за ним следит уже другая проверка.
                if *self.last.lock().unwrap() != sent {
                    return;
                }
                let empty = Command::new("wl-paste")
                    .arg("--list-types")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|s| !s.success());
                if empty {
                    self.cleared(&on_change);
                    return;
                }
            }
        });
    }

    /// Кладёт текст с телефона в буфер ноутбука.
    pub async fn set(&self, text: String) {
        if text.is_empty() || text.len() > MAX_TEXT || !self.remember(text.as_bytes()) {
            return;
        }
        copy(None, text.as_bytes()).await;
    }

    /// Кладёт картинку с телефона в буфер ноутбука.
    pub async fn set_image(&self, mime: &str, data: &[u8]) {
        if !mime.starts_with("image/") || data.len() > MAX_CLIPBOARD_IMAGE || !self.remember(data) {
            return;
        }
        copy(Some(mime), data).await;
    }
}

async fn copy(mime: Option<&str>, data: &[u8]) {
    let mut cmd = Command::new("wl-copy");
    if let Some(mime) = mime {
        cmd.args(["--type", mime]);
    }
    let child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("wl-copy: {e}");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(data).await;
    }
    // wl-copy форкается и сам обслуживает буфер; родитель завершается сразу.
    let _ = child.wait().await;
}
