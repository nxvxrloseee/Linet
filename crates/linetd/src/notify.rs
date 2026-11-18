//! Уведомления рабочего стола (org.freedesktop.Notifications) с действиями.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

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
    handlers: Arc<Mutex<HashMap<u32, (String, Handler)>>>,
    /// Сервер разбирает разметку в теле — тогда текст нужно экранировать.
    generation: Arc<AtomicU64>,
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
        let handlers: Arc<Mutex<HashMap<u32, (String, Handler)>>> = Arc::default();

        let mut invoked = proxy.receive_action_invoked().await?;
        let h = handlers.clone();
        tokio::spawn(async move {
            while let Some(sig) = invoked.next().await {
                let Ok(args) = sig.args() else { continue };
                if let Some((owner, handler)) = h.lock().unwrap().get(&args.id)
                    && sig.message().header().sender().is_some_and(|sender| sender.as_str() == owner)
                {
                    handler(NoticeEvent::Action(&args.action_key));
                }
            }
        });
        let mut closed = proxy.receive_notification_closed().await?;
        let h = handlers.clone();
        tokio::spawn(async move {
            while let Some(sig) = closed.next().await {
                let Ok(args) = sig.args() else { continue };
                let mut handlers = h.lock().unwrap();
                let ours = handlers.get(&args.id).is_some_and(|(owner, _)|
                    sig.message().header().sender().is_some_and(|sender| sender.as_str() == owner));
                if ours && let Some((_, handler)) = handlers.remove(&args.id) {
                    handler(NoticeEvent::Closed(args.reason));
                }
            }
        });
        let generation = Arc::new(AtomicU64::new(0));
        let mut owners = proxy.inner().receive_owner_changed().await?;
        let h = handlers.clone();
        let g = generation.clone();
        tokio::spawn(async move {
            while owners.next().await.is_some() {
                h.lock().unwrap().clear();
                g.fetch_add(1, Ordering::AcqRel);
            }
        });
        Ok(Self { proxy, handlers, generation })
    }

    pub fn generation(&self) -> u64 { self.generation.load(Ordering::Acquire) }

    pub async fn show(&self, notice: Notice<'_>, handler: Option<Handler>) -> zbus::Result<u32> {
        let bus = zbus::fdo::DBusProxy::new(self.proxy.inner().connection()).await?;
        let owner = bus.get_name_owner("org.freedesktop.Notifications".try_into()?).await?;
        // Bind methods and callbacks to one server generation; reused ids are not interchangeable.
        let proxy = NotificationsProxy::builder(self.proxy.inner().connection())
            .destination(owner.clone())?.build().await?;
        let body_markup = proxy.get_capabilities().await.is_ok_and(|caps| caps.iter().any(|c| c == "body-markup"));
        let actions: Vec<&str> = notice.actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
        let mut hints = HashMap::new();
        if notice.app == "Linet" {
            hints.insert("desktop-entry", Value::from("linet"));
        }
        let body = if body_markup { escape_body(notice.body) } else { notice.body.to_string() };
        let (app, summary) = (neutralize_plain(notice.app), neutralize_plain(notice.summary));
        let id = proxy.notify(&app, notice.replaces, notice.icon, &summary, &body, &actions, hints, -1).await?;
        let mut handlers = self.handlers.lock().unwrap();
        match handler {
            Some(handler) => {
                handlers.insert(id, (owner.to_string(), handler));
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
        let owner = self.handlers.lock().unwrap().remove(&id).map(|(owner, _)| owner);
        if let Some(owner) = owner
            && let Ok(builder) = NotificationsProxy::builder(self.proxy.inner().connection()).destination(owner)
            && let Ok(proxy) = builder.build().await
        {
            let _ = proxy.close_notification(id).await;
        }
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct MockServer;
    #[zbus::interface(name="org.freedesktop.Notifications")]
    impl MockServer {
        #[allow(clippy::too_many_arguments)]
        fn notify(&self, _app: &str, _replaces: u32, _icon: &str, _summary: &str, _body: &str,
            _actions: Vec<String>, _hints: HashMap<String,zbus::zvariant::OwnedValue>, _timeout: i32) -> u32 { 1 }
        fn get_capabilities(&self) -> Vec<String> { vec!["actions".into(),"body-markup".into()] }
        fn close_notification(&self, _id: u32) {}
        #[zbus(signal)]
        async fn action_invoked(emitter: &zbus::object_server::SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;
    }
    async fn server(bus: &crate::test_bus::Bus) -> zbus::Connection {
        let conn=bus.connect().await;
        conn.object_server().at("/org/freedesktop/Notifications",MockServer).await.unwrap();
        conn.request_name("org.freedesktop.Notifications").await.unwrap(); conn
    }
    fn notice() -> Notice<'static> {
        Notice { app:"Linet", summary:"test", body:"test", icon:"",actions:&[("default","Open")],replaces:0 }
    }
    #[tokio::test]
    async fn notification_callbacks_follow_server_generation_and_reused_ids_are_safe() {
        let bus=crate::test_bus::Bus::new(); let conn=bus.connect().await;
        let notifier=Notifier::new(&conn).await.unwrap();
        let first=server(&bus).await;
        crate::test_bus::eventually(|| notifier.generation()>0).await;
        let old_actions=Arc::new(AtomicUsize::new(0)); let count=old_actions.clone();
        notifier.show(notice(),Some(Box::new(move |_| {count.fetch_add(1,Ordering::SeqCst);}))).await.unwrap();
        let generation=notifier.generation();
        first.release_name("org.freedesktop.Notifications").await.unwrap();
        crate::test_bus::eventually(|| notifier.generation()>generation).await;
        assert!(notifier.handlers.lock().unwrap().is_empty());
        let generation=notifier.generation(); let second=server(&bus).await;
        crate::test_bus::eventually(|| notifier.generation()>generation).await;
        let new_actions=Arc::new(AtomicUsize::new(0)); let count=new_actions.clone();
        notifier.show(notice(),Some(Box::new(move |_| {count.fetch_add(1,Ordering::SeqCst);}))).await.unwrap();
        let old=first.object_server().interface::<_,MockServer>("/org/freedesktop/Notifications").await.unwrap();
        MockServer::action_invoked(old.signal_emitter(),1,"default").await.unwrap();
        let new=second.object_server().interface::<_,MockServer>("/org/freedesktop/Notifications").await.unwrap();
        MockServer::action_invoked(new.signal_emitter(),1,"default").await.unwrap();
        crate::test_bus::eventually(|| new_actions.load(Ordering::SeqCst)>0).await;
        assert_eq!(new_actions.load(Ordering::SeqCst),1);
        assert_eq!(old_actions.load(Ordering::SeqCst),0);
    }
}
