//! D-Bus API демона на сессионной шине: `org.linet.Daemon`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use linet_core::files::{TransferState, TransferUpdate};
use tokio::sync::mpsc;
use zbus::fdo;
use zbus::object_server::{InterfaceRef, SignalEmitter};

use crate::daemon::Daemon;
use crate::mirror::Mirror;
use crate::notify::{Notice, NoticeEvent, Notifier, xdg_open};

pub const SERVICE: &str = "org.linet.Daemon";
pub const PATH: &str = "/org/linet/Daemon";

pub struct Api {
    pub daemon: Arc<Daemon>,
}

fn err(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

#[zbus::interface(name = "org.linet.Daemon1")]
impl Api {
    /// (id, имя, тип, подключено)
    async fn devices(&self) -> Vec<(String, String, String, bool)> {
        self.daemon.devices().into_iter().map(|d| (d.id, d.name, d.kind, d.connected)).collect()
    }

    /// (id, имя) этого компьютера
    async fn info(&self) -> (String, String) {
        (self.daemon.identity().fingerprint().device_id(), self.daemon.hello().name.clone())
    }

    async fn start_pairing(&self) -> fdo::Result<String> {
        self.daemon.start_pairing().map_err(err)
    }

    async fn cancel_pairing(&self) {
        self.daemon.cancel_pairing();
    }

    /// Время отклика в миллисекундах.
    async fn ping(&self, id: &str) -> fdo::Result<f64> {
        let rtt = self.daemon.ping(id).await.map_err(err)?;
        Ok(rtt.as_secs_f64() * 1000.0)
    }

    async fn unpair(&self, id: &str) -> fdo::Result<()> {
        self.daemon.unpair(id).map_err(err)
    }

    /// Начинает отправку файла (абсолютный путь), возвращает id передачи.
    /// Ход передачи — сигналами TransferProgress и TransferFinished.
    async fn send_file(&self, id: &str, path: &str) -> fdo::Result<u64> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err(fdo::Error::InvalidArgs("нужен абсолютный путь".into()));
        }
        self.daemon.send_file(id, path).await.map_err(err)
    }

    #[zbus(signal)]
    pub async fn devices_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn transfer_progress(
        emitter: &SignalEmitter<'_>,
        id: u64,
        peer: &str,
        name: &str,
        incoming: bool,
        done: u64,
        total: u64,
    ) -> zbus::Result<()>;

    /// `detail` — путь сохранённого файла или текст ошибки.
    #[zbus(signal)]
    #[allow(clippy::too_many_arguments)]
    pub async fn transfer_finished(
        emitter: &SignalEmitter<'_>,
        id: u64,
        peer: &str,
        name: &str,
        incoming: bool,
        ok: bool,
        detail: &str,
    ) -> zbus::Result<()>;
}

pub async fn serve(
    daemon: Arc<Daemon>,
    transfers: mpsc::UnboundedReceiver<TransferUpdate>,
) -> anyhow::Result<zbus::Connection> {
    let mut changes = daemon.subscribe();
    let conn = zbus::connection::Builder::session()?
        .name(SERVICE)?
        .serve_at(PATH, Api { daemon: daemon.clone() })?
        .build()
        .await?;
    let iface = conn.object_server().interface::<_, Api>(PATH).await?;

    let devices_iface = iface.clone();
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            if let Err(e) = Api::devices_changed(devices_iface.signal_emitter()).await {
                tracing::warn!("D-Bus сигнал: {e}");
            }
        }
    });

    let notifier = Notifier::new(&conn).await.map_err(|e| tracing::warn!("уведомления недоступны: {e}")).ok();
    if let Some(n) = &notifier {
        daemon.set_mirror(Mirror::new(n.clone(), cache_dir()));
    }
    tokio::spawn(forward_transfers(iface, notifier, transfers));
    Ok(conn)
}

async fn forward_transfers(
    iface: InterfaceRef<Api>,
    notifier: Option<Notifier>,
    mut updates: mpsc::UnboundedReceiver<TransferUpdate>,
) {
    let emitter = iface.signal_emitter();
    while let Some(u) = updates.recv().await {
        let res = match &u.state {
            TransferState::Progress => {
                Api::transfer_progress(emitter, u.id, &u.peer, &u.name, u.incoming, u.done, u.total).await
            }
            TransferState::Done { path } => {
                let detail = path.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
                Api::transfer_finished(emitter, u.id, &u.peer, &u.name, u.incoming, true, &detail).await
            }
            TransferState::Failed(reason) => {
                Api::transfer_finished(emitter, u.id, &u.peer, &u.name, u.incoming, false, reason).await
            }
        };
        if let Err(e) = res {
            tracing::warn!("D-Bus сигнал: {e}");
        }
        if let Some(n) = &notifier
            && u.incoming
        {
            notify_incoming(n, &u).await;
        }
    }
}

/// Уведомление о входящем файле: «Открыть» и «Папка».
async fn notify_incoming(n: &Notifier, u: &TransferUpdate) {
    let summary = format!("Файл от {}", u.peer);
    let res = match &u.state {
        TransferState::Done { path: Some(path) } => {
            let path: PathBuf = path.clone();
            let notice = Notice {
                app: "Linet",
                summary: &summary,
                body: &u.name,
                icon: "document-save",
                actions: &[("default", "Открыть"), ("folder", "Папка")],
                replaces: 0,
            };
            n.show(
                notice,
                Some(Box::new(move |event| match event {
                    NoticeEvent::Action("folder") => xdg_open(path.parent().unwrap_or(&path)),
                    NoticeEvent::Action(_) => xdg_open(&path),
                    NoticeEvent::Closed(_) => {}
                })),
            )
            .await
        }
        TransferState::Failed(reason) => {
            let body = format!("{}: {reason}", u.name);
            let notice = Notice { app: "Linet", summary: &summary, body: &body, icon: "dialog-error", actions: &[], replaces: 0 };
            n.show(notice, None).await
        }
        _ => return,
    };
    if let Err(e) = res {
        tracing::warn!("уведомление: {e}");
    }
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("linet")
}
