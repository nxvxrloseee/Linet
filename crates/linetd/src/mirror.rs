//! Зеркалирование уведомлений телефона на рабочий стол.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use linet_core::{Message, Notification, Session};

use crate::notify::{CLOSED_BY_USER, Handler, Notice, NoticeEvent, Notifier, prompt};

pub struct Mirror {
    notifier: Notifier,
    icons: PathBuf,
    /// Ключ уведомления на телефоне → id уведомления на рабочем столе.
    shown: Arc<Mutex<HashMap<String, u32>>>,
}

impl Mirror {
    pub fn new(notifier: Notifier, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { notifier, icons: cache_dir.join("icons"), shown: Arc::default() })
    }

    /// Обрабатывает сообщение, если оно про уведомления; иначе возвращает его.
    pub async fn handle(&self, session: &Arc<Session>, msg: Message) -> Option<Message> {
        match msg {
            Message::Notification(n) => self.show(session, n).await,
            Message::NotificationRemoved { key } => {
                let id = self.shown.lock().unwrap().remove(&key);
                if let Some(id) = id {
                    self.notifier.close(id).await;
                }
            }
            other => return Some(other),
        }
        None
    }

    fn icon_path(&self, package: &str, png: Option<&[u8]>) -> String {
        let safe: String = package.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_').collect();
        let path = self.icons.join(format!("{safe}.png"));
        if let Some(png) = png {
            let _ = std::fs::create_dir_all(&self.icons);
            if let Err(e) = std::fs::write(&path, png) {
                tracing::debug!("иконка {package}: {e}");
            }
        }
        if path.exists() { path.display().to_string() } else { "phone".into() }
    }

    async fn show(&self, session: &Arc<Session>, n: Notification) {
        let icon = self.icon_path(&n.package, n.icon.as_ref().map(|b| b.as_slice()));
        let keys: Vec<(String, String)> = n
            .actions
            .iter()
            .map(|a| (format!("{}{}", if a.reply { 'r' } else { 'a' }, a.index), a.title.clone()))
            .collect();
        let actions: Vec<(&str, &str)> = keys.iter().map(|(k, l)| (k.as_str(), l.as_str())).collect();
        let replaces = self.shown.lock().unwrap().get(&n.key).copied().unwrap_or(0);
        let notice =
            Notice { app: &n.app, summary: &n.title, body: &n.text, icon: &icon, actions: &actions, replaces };
        match self.notifier.show(notice, Some(handler(Arc::downgrade(session), &n, self.shown.clone()))).await {
            Ok(id) => drop(self.shown.lock().unwrap().insert(n.key, id)),
            Err(e) => tracing::warn!("уведомление: {e}"),
        }
    }
}

/// Кнопки и закрытие на рабочем столе → сообщения телефону.
/// Сеанс держим слабой ссылкой: после переподключения старое действие просто теряется.
fn handler(session: Weak<Session>, n: &Notification, shown: Arc<Mutex<HashMap<String, u32>>>) -> Handler {
    let key = n.key.clone();
    let (app, clearable) = (n.app.clone(), n.clearable);
    // Многие серверы уведомлений (quickshell/Caelestia) после нажатия кнопки сами
    // закрывают уведомление с причиной «пользователь». Это не смахивание.
    let acted = AtomicBool::new(false);
    Box::new(move |event| {
        let Some(session) = session.upgrade() else { return };
        let key = key.clone();
        match event {
            NoticeEvent::Action(action) => {
                tracing::info!(%key, %action, "нажата кнопка уведомления");
                acted.store(true, Ordering::Relaxed);
                let (kind, index) = action.split_at(1.min(action.len()));
                let Ok(index) = index.parse::<u32>() else { return };
                let reply_needed = kind == "r";
                let app = app.clone();
                tokio::spawn(async move {
                    let reply = if reply_needed {
                        match prompt(&format!("Ответ в {app}:")).await {
                            Some(text) => Some(text),
                            None => {
                                tracing::info!("ввод ответа отменён");
                                return;
                            }
                        }
                    } else {
                        None
                    };
                    let has_reply = reply.is_some();
                    match session.send(Message::NotificationAction { key, action: index, reply }).await {
                        Ok(_) => tracing::info!(index, has_reply, "действие отправлено на телефон"),
                        Err(e) => tracing::warn!("действие уведомления: {e:#}"),
                    }
                });
            }
            NoticeEvent::Closed(reason) => {
                tracing::debug!(%key, reason, "уведомление закрыто");
                shown.lock().unwrap().remove(&key);
                if reason == CLOSED_BY_USER && clearable && !acted.load(Ordering::Relaxed) {
                    tokio::spawn(async move {
                        let _ = session.send(Message::NotificationDismiss { key }).await;
                    });
                }
            }
        }
    })
}
