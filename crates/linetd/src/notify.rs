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
        Ok(Self { proxy, handlers })
    }

    pub async fn show(&self, notice: Notice<'_>, handler: Option<Handler>) -> zbus::Result<u32> {
        let actions: Vec<&str> = notice.actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
        let mut hints = HashMap::new();
        if notice.app == "Linet" {
            hints.insert("desktop-entry", Value::from("linet"));
        }
        let id = self
            .proxy
            .notify(notice.app, notice.replaces, notice.icon, notice.summary, notice.body, &actions, hints, -1)
            .await?;
        let mut handlers = self.handlers.lock().unwrap();
        match handler {
            Some(handler) => drop(handlers.insert(id, handler)),
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
