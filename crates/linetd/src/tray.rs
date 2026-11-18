//! Значок в трее (StatusNotifierItem): заряд телефона и состояние связи.
//! Регистрация восстанавливается при появлении/смене владельца watcher.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::OwnedObjectPath;

const PATH: &str = "/StatusNotifierItem";

#[derive(Clone, Default)]
pub struct TrayState {
    pub phone: Option<String>,
    pub battery: Option<(u8, bool)>,
}

impl TrayState {
    fn icon(&self) -> String {
        match (self.phone.as_ref(), self.battery) {
            (None, _) => "phone-disconnected-symbolic".into(),
            (Some(_), None) => "phone-symbolic".into(),
            (Some(_), Some((level, charging))) => {
                let step = (u32::from(level.min(100)) + 5) / 10 * 10;
                let suffix = if charging { "-charging" } else { "" };
                format!("battery-level-{step}{suffix}-symbolic")
            }
        }
    }

    fn tooltip(&self) -> (String, String) {
        match (&self.phone, self.battery) {
            (None, _) => ("Linet".into(), "Телефон не подключён".into()),
            (Some(name), None) => (name.clone(), "Подключено".into()),
            (Some(name), Some((level, charging))) => {
                let state = if charging { ", заряжается" } else { "" };
                (name.clone(), format!("Батарея {level} %{state}"))
            }
        }
    }
}

pub struct Item {
    state: Arc<Mutex<TrayState>>,
    on_activate: Box<dyn Fn() + Send + Sync>,
}

type Pixmap = (i32, i32, Vec<u8>);

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "Hardware"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "linet"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "Linet"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.state.lock().unwrap().icon()
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        let (title, body) = self.state.lock().unwrap().tooltip();
        (String::new(), Vec::new(), title, body)
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/NO_DBUSMENU").expect("валидный путь")
    }

    fn activate(&self, _x: i32, _y: i32) {
        (self.on_activate)();
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait Watcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

#[derive(Clone)]
pub struct Tray {
    state: Arc<Mutex<TrayState>>,
    iface: Option<InterfaceRef<Item>>,
}

impl Tray {
    /// Регистрирует значок. Нет трея — молча работаем без него.
    pub async fn start(conn: &zbus::Connection, on_activate: impl Fn() + Send + Sync + 'static) -> Self {
        let state = Arc::new(Mutex::new(TrayState::default()));
        let item = Item { state: state.clone(), on_activate: Box::new(on_activate) };
        let iface = async {
            conn.object_server().at(PATH, item).await?;
            let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
            conn.request_name(name.as_str()).await?;
            let watcher = WatcherProxy::new(conn).await?;
            let mut owners = watcher.inner().receive_owner_changed().await?;
            if let Err(e) = watcher.register_status_notifier_item(&name).await {
                tracing::debug!("watcher пока недоступен: {e}");
            }
            let registration_conn = conn.clone();
            tokio::spawn(async move {
                while let Some(owner) = owners.next().await {
                    if owner.is_none() { continue; }
                    match WatcherProxy::new(&registration_conn).await {
                        Ok(watcher) => {
                            if let Err(e) = watcher.register_status_notifier_item(&name).await {
                                tracing::warn!("регистрация трея: {e}");
                            }
                        }
                        Err(e) => tracing::warn!("watcher: {e}"),
                    }
                }
            });
            conn.object_server().interface::<_, Item>(PATH).await
        }
        .await;
        match iface {
            Ok(iface) => Self { state, iface: Some(iface) },
            Err(e) => {
                tracing::info!("трей недоступен: {e}");
                Self { state, iface: None }
            }
        }
    }

    pub async fn update(&self, f: impl FnOnce(&mut TrayState)) {
        f(&mut self.state.lock().unwrap());
        if let Some(iface) = &self.iface {
            let emitter = iface.signal_emitter();
            let _ = Item::new_icon(emitter).await;
            let _ = Item::new_tool_tip(emitter).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct MockWatcher(Arc<AtomicUsize>);
    #[zbus::interface(name="org.kde.StatusNotifierWatcher")]
    impl MockWatcher {
        fn register_status_notifier_item(&self, _service: &str) { self.0.fetch_add(1, Ordering::SeqCst); }
    }
    #[tokio::test]
    async fn late_watcher_and_replacement_register_without_restarting_tray() {
        let bus = crate::test_bus::Bus::new();
        let conn = bus.connect().await;
        let tray = Tray::start(&conn, || {}).await;
        assert!(tray.iface.is_some());
        let first = bus.connect().await;
        let first_count = Arc::new(AtomicUsize::new(0));
        first.object_server().at("/StatusNotifierWatcher", MockWatcher(first_count.clone())).await.unwrap();
        first.request_name("org.kde.StatusNotifierWatcher").await.unwrap();
        crate::test_bus::eventually(|| first_count.load(Ordering::SeqCst) > 0).await;
        first.release_name("org.kde.StatusNotifierWatcher").await.unwrap();
        let second = bus.connect().await;
        let second_count = Arc::new(AtomicUsize::new(0));
        second.object_server().at("/StatusNotifierWatcher", MockWatcher(second_count.clone())).await.unwrap();
        second.request_name("org.kde.StatusNotifierWatcher").await.unwrap();
        crate::test_bus::eventually(|| second_count.load(Ordering::SeqCst) > 0).await;
        assert_eq!(second_count.load(Ordering::SeqCst),1);
    }
}
