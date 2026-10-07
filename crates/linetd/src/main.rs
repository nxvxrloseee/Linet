mod autolock;
mod clipboard;
mod daemon;
mod dbus;
mod idle;
mod mirror;
mod netwatch;
mod notify;
mod tray;
mod unlock;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use linet_core::files::Transfers;
use linet_core::{DeviceKind, Hello, Identity, PeerStore, transport};
use tracing_subscriber::EnvFilter;

use crate::daemon::Daemon;

const MDNS_TYPE: &str = "_linet._udp.local.";

#[derive(Parser)]
#[command(version, about = "Демон Linet: связь Linux с Android")]
struct Args {
    /// UDP-порт QUIC
    #[arg(long, default_value_t = linet_core::DEFAULT_PORT)]
    port: u16,
    /// Имя этого компьютера (по умолчанию hostname)
    #[arg(long)]
    name: Option<String>,
    /// Каталог данных (по умолчанию $XDG_DATA_HOME/linet)
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Не анонсировать себя по mDNS
    #[arg(long)]
    no_mdns: bool,
    /// Не синхронизировать буфер обмена
    #[arg(long)]
    no_clipboard: bool,
    /// Не разблокировать экран телефоном
    #[arg(long)]
    no_unlock: bool,
    /// Не блокировать экран, когда телефон уходит
    #[arg(long)]
    no_autolock: bool,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "linux".into())
}

fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(d).join("linet"));
    }
    let home = std::env::var_os("HOME").context("не задан HOME")?;
    Ok(PathBuf::from(home).join(".local/share/linet"))
}

/// Каталог загрузок из user-dirs.dirs, иначе ~/Downloads.
fn downloads_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("не задан HOME")?);
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    if let Ok(dirs) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
        for line in dirs.lines() {
            if let Some(v) = line.strip_prefix("XDG_DOWNLOAD_DIR=") {
                let v = v.trim().trim_matches('"').replace("$HOME", &home.to_string_lossy());
                return Ok(PathBuf::from(v));
            }
        }
    }
    Ok(home.join("Downloads"))
}

/// Описание mDNS-сервиса. Регистрирует его netwatch — только в доверенных сетях.
fn mdns_service(id: &str, name: &str, port: u16) -> Result<netwatch::Mdns> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let host = format!("{}.local.", hostname());
    let props = [("id", id), ("v", "1")];
    let service = mdns_sd::ServiceInfo::new(MDNS_TYPE, name, &host, "", port, &props[..])?.enable_addr_auto();
    Ok(netwatch::Mdns { daemon, service })
}

#[tokio::main]
async fn main() -> Result<()> {
    // Под systemd вывод идёт в журнал, цветовые коды там только мешают.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let args = Args::parse();

    let dir = match args.data_dir {
        Some(d) => d,
        None => data_dir()?,
    };
    let identity = Identity::load_or_create(&dir)?;
    let store = PeerStore::load(dir.join("peers.json"))?;
    let name = args.name.unwrap_or_else(hostname);
    let hello = Hello {
        name: name.clone(),
        kind: DeviceKind::Laptop,
        proto: linet_core::PROTO_VERSION,
        caps: vec!["ping".into(), "files".into(), "notifications".into(), "clipboard".into(), "handoff".into(), "unlock".into()],
    };
    let id = identity.fingerprint().device_id();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = downloads_dir()?.join("Linet");
    let transfers = Transfers::new(inbox, linet_core::files::DEFAULT_MAX_SIZE, Arc::new(move |u| drop(updates_tx.send(u))));
    let clipboard = (!args.no_clipboard).then(clipboard::Clipboard::new);
    let daemon = Daemon::new(identity, store, hello, args.port, transfers, clipboard.clone());
    if let Some(clipboard) = clipboard {
        let d = daemon.clone();
        clipboard.watch(move |event| {
            let msg = match event {
                clipboard::ClipEvent::Text(text) => linet_core::Message::Clipboard { text },
                clipboard::ClipEvent::Cleared => linet_core::Message::Clipboard { text: String::new() },
                clipboard::ClipEvent::Image { mime, data } => {
                    linet_core::Message::ClipboardImage { mime, data: serde_bytes::ByteBuf::from(data) }
                }
            };
            let d = d.clone();
            tokio::spawn(async move { d.broadcast(msg).await });
        });
    }

    let endpoint = transport::server_endpoint(daemon.identity(), daemon.policy(), args.port)
        .with_context(|| format!("не удалось занять UDP-порт {}", args.port))?;
    tracing::info!(%id, %name, port = args.port, "linetd запущен");

    if !args.no_autolock {
        autolock::spawn(daemon.clone());
    }
    let unlocker = unlock::Unlocker::new(daemon.clone());
    if !args.no_unlock {
        unlocker.clone().watch();
    }
    let _dbus = dbus::serve(daemon.clone(), unlocker, updates_rx).await.context("D-Bus: демон уже запущен?")?;
    let mdns = if args.no_mdns {
        None
    } else {
        mdns_service(&id, &name, args.port).map_err(|e| tracing::warn!("mDNS: {e:#}")).ok()
    };
    netwatch::NetWatch::new(daemon.clone(), args.port, mdns, dir.clone()).spawn();

    let server = tokio::spawn(daemon.serve(endpoint.clone()));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = server => {}
    }
    endpoint.close(0u32.into(), b"shutdown");
    endpoint.wait_idle().await;
    Ok(())
}
