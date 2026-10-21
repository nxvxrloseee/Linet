//! Уведомления рабочего стола (org.freedesktop.Notifications) с действиями.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// Что произошло с показанным уведомлением.
pub enum NoticeEvent<'a> {
    Action(&'a str),
    /// Причина по спецификации: 1 — истекло, 2 — закрыто пользователем, 3 — закрыто нами.
    Closed(u32),
}

pub const CLOSED_BY_USER: u32 = 2;
/// Сколько уведомлений помнить с обработчиками кнопок.
const MAX_HANDLERS: usize = 200;

pub type Handler = Box<dyn Fn(NoticeEvent) + Send + Sync>;

pub struct Notice<'a> {
    pub app: &'a str,
    pub summary: &'a str,
    pub body: &'a str,
    pub icon: &'a str,
    /// (ключ, подпись). Ключ "default" — клик по самому уведомлению.
    pub actions: &'a [(&'a str, &'a str)],
    pub replaces: u32,
}

#[derive(Clone)]
pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    handlers: Arc<Mutex<HashMap<u32, Handler>>>,
    /// Сервер разбирает разметку в теле — тогда текст нужно экранировать.
    body_markup: bool,
}

/// Текст уведомлений приходит от чужих людей (имя и сообщение в мессенджере),
/// поэтому разметку в нём нужно обезвредить.
///
/// Тело: числовые HTML-сущности для всех символов разметки HTML и Markdown.
/// Их декодируют и HTML, и Markdown (Caelestia), но как разметку не исполняют.
pub fn escape_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' | '<' | '>' | '"' | '\'' | '[' | ']' | '(' | ')' | '*' | '_' | '`' | '#' | '!' | '\\' | '~' | '|' => {
                out.push_str(&format!("&#{};", c as u32))
            }
            c => out.push(c),
        }
    }
    out
}

/// Заголовок по спецификации — простой текст, но некоторые серверы (Qt AutoText)
/// распознают в нём HTML по «<». Сущности там не декодируются, поэтому заменяем
/// угловые скобки похожими символами.
pub fn neutralize_plain(text: &str) -> String {
    text.replace('<', "‹").replace('>', "›")
}

impl Notifier {
    pub async fn new(conn: &zbus::Connection) -> zbus::Result<Self> {
        let proxy = NotificationsProxy::new(conn).await?;
        let handlers: Arc<Mutex<HashMap<u32, Handler>>> = Arc::default();

        let mut invoked = proxy.receive_action_invoked().await?;
        let h = handlers.clone();
        tokio::spawn(async move {
            while let Some(sig) = invoked.next().await {
                let Ok(args) = sig.args() else { continue };
                if let Some(handler) = h.lock().unwrap().get(&args.id) {
                    handler(NoticeEvent::Action(&args.action_key));
                }
            }
        });
        let mut closed = proxy.receive_notification_closed().await?;
        let h = handlers.clone();
        tokio::spawn(async move {
            while let Some(sig) = closed.next().await {
                let Ok(args) = sig.args() else { continue };
                let handler = h.lock().unwrap().remove(&args.id);
                if let Some(handler) = handler {
                    handler(NoticeEvent::Closed(args.reason));
                }
            }
        });
        let body_markup = proxy.get_capabilities().await.is_ok_and(|caps| caps.iter().any(|c| c == "body-markup"));
        Ok(Self { proxy, handlers, body_markup })
    }

    pub async fn show(&self, notice: Notice<'_>, handler: Option<Handler>) -> zbus::Result<u32> {
        let actions: Vec<&str> = notice.actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
        let mut hints = HashMap::new();
        if notice.app == "Linet" {
            hints.insert("desktop-entry", Value::from("linet"));
        }
        let body = if self.body_markup { escape_body(notice.body) } else { notice.body.to_string() };
        let (app, summary) = (neutralize_plain(notice.app), neutralize_plain(notice.summary));
        let id = self.proxy.notify(&app, notice.replaces, notice.icon, &summary, &body, &actions, hints, -1).await?;
        let mut handlers = self.handlers.lock().unwrap();
        match handler {
            Some(handler) => {
                handlers.insert(id, handler);
                // Caelestia хранит уведомления и не закрывает их, так что обработчики
                // копились бы бесконечно. Старым кнопкам за таким пределом уже не до нажатий.
                while handlers.len() > MAX_HANDLERS {
                    let Some(&oldest) = handlers.keys().min() else { break };
                    handlers.remove(&oldest);
                }
            }
            None => drop(handlers.remove(&id)),
        }
        Ok(id)
    }

    /// Закрыть без вызова обработчика: закрытие инициировали мы, а не пользователь.
    pub async fn close(&self, id: u32) {
        self.handlers.lock().unwrap().remove(&id);
        let _ = self.proxy.close_notification(id).await;
    }
}

/// Спросить у пользователя строку (fuzzel → rofi → zenity). `None` — отмена.
pub async fn prompt(title: &str) -> Option<String> {
    use tokio::process::Command;
    let attempts: [(&str, Vec<String>); 3] = [
        ("fuzzel", vec!["--dmenu".into(), format!("--prompt-only={title} ")]),
        ("rofi", vec!["-dmenu".into(), "-p".into(), title.into(), "-l".into(), "0".into()]),
        ("zenity", vec!["--entry".into(), "--title=Linet".into(), format!("--text={title}")]),
    ];
    for (cmd, args) in attempts {
        let out = match Command::new(cmd).args(&args).stdin(std::process::Stdio::null()).output().await {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!("{cmd}: {e}");
                continue;
            }
        };
        if !out.status.success() && !out.stderr.is_empty() {
            tracing::debug!("{cmd}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (out.status.success() && !text.is_empty()).then_some(text);
    }
    tracing::warn!("нет fuzzel, rofi или zenity для ввода ответа");
    None
}

/// Открыть файл или каталог приложением по умолчанию.
pub fn xdg_open(path: &std::path::Path) {
    match std::process::Command::new("xdg-open")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // Дождаться в отдельном потоке, чтобы не оставлять зомби.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => tracing::warn!("xdg-open {path:?}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_markup_is_inert() {
        let evil = "[тык](https://evil) ![x](https://evil/p.png) <img src=x> **b** `c`";
        let escaped = escape_body(evil);
        for c in ['[', ']', '(', ')', '<', '>', '*', '`', '!'] {
            assert!(!escaped.contains(c), "остался {c}: {escaped}");
        }
        assert_eq!(escape_body("Привет, как дела?"), "Привет, как дела?");
    }

    #[test]
    fn plain_has_no_tags() {
        assert_eq!(neutralize_plain("<b>Вася</b>"), "‹b›Вася‹/b›");
    }
}
