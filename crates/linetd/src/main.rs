mod daemon;
mod dbus;
mod mirror;
mod notify;

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

fn advertise(id: &str, name: &str, port: u16) -> Result<mdns_sd::ServiceDaemon> {
    let mdns = mdns_sd::ServiceDaemon::new()?;
    let host = format!("{}.local.", hostname());
    let props = [("id", id), ("v", "1")];
    let info = mdns_sd::ServiceInfo::new(MDNS_TYPE, name, &host, "", port, &props[..])?.enable_addr_auto();
    mdns.register(info)?;
    Ok(mdns)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
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
        caps: vec!["ping".into(), "files".into(), "notifications".into()],
    };
    let id = identity.fingerprint().device_id();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = downloads_dir()?.join("Linet");
    let transfers = Transfers::new(inbox, Arc::new(move |u| drop(updates_tx.send(u))));
    let daemon = Daemon::new(identity, store, hello, args.port, transfers);

    let endpoint = transport::server_endpoint(daemon.identity(), daemon.policy(), args.port)
        .with_context(|| format!("не удалось занять UDP-порт {}", args.port))?;
    tracing::info!(%id, %name, port = args.port, "linetd запущен");

    let _dbus = dbus::serve(daemon.clone(), updates_rx).await.context("D-Bus: демон уже запущен?")?;
    let _mdns = if args.no_mdns {
        None
    } else {
        advertise(&id, &name, args.port).map_err(|e| tracing::warn!("mDNS: {e:#}")).ok()
    };

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
