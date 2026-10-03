use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;

#[zbus::proxy(
    interface = "org.linet.Daemon1",
    default_service = "org.linet.Daemon",
    default_path = "/org/linet/Daemon"
)]
trait Daemon {
    fn devices(&self) -> zbus::Result<Vec<(String, String, String, bool)>>;
    fn info(&self) -> zbus::Result<(String, String)>;
    fn start_pairing(&self) -> zbus::Result<String>;
    fn cancel_pairing(&self) -> zbus::Result<()>;
    fn ping(&self, id: &str) -> zbus::Result<f64>;
    fn unpair(&self, id: &str) -> zbus::Result<()>;
    fn send_file(&self, id: &str, path: &str) -> zbus::Result<u64>;

    #[zbus(signal)]
    fn transfer_progress(&self, id: u64, peer: String, name: String, incoming: bool, done: u64, total: u64)
    -> zbus::Result<()>;

    #[zbus(signal)]
    fn transfer_finished(&self, id: u64, peer: String, name: String, incoming: bool, ok: bool, detail: String)
    -> zbus::Result<()>;
}

#[derive(Parser)]
#[command(version, about = "Управление Linet")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Сопряжённые устройства
    Devices,
    /// Сопряжение: показать QR-код для телефона
    Pair {
        /// Только вывести URI, без QR
        #[arg(long)]
        uri: bool,
        /// QR для терминала со светлым фоном
        #[arg(long)]
        light: bool,
    },
    /// Проверить связь с устройством
    Ping { id: String },
    /// Удалить сопряжение
    Unpair { id: String },
    /// Отправить файлы на телефон
    Send {
        /// Устройство (по умолчанию — единственное подключённое)
        #[arg(long)]
        to: Option<String>,
        #[arg(required = true)]
        files: Vec<std::path::PathBuf>,
    },
    /// Состояние одной строкой JSON (для waybar)
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let conn = zbus::Connection::session().await?;
    let daemon = DaemonProxy::new(&conn).await?;
    daemon.info().await.context("linetd не отвечает — он запущен?")?;

    match args.cmd {
        Cmd::Devices => {
            let devices = daemon.devices().await?;
            if devices.is_empty() {
                println!("Нет сопряжённых устройств. Запусти `linetctl pair`.");
            }
            for (id, name, kind, connected) in devices {
                let state = if connected { "● подключено" } else { "○ не в сети" };
                println!("{id}  {name:<20} {kind:<8} {state}");
            }
        }
        Cmd::Pair { uri, light } => pair(&daemon, uri, light).await?,
        Cmd::Ping { id } => {
            let ms = daemon.ping(&id).await?;
            println!("{ms:.1} мс");
        }
        Cmd::Unpair { id } => {
            daemon.unpair(&id).await?;
            println!("Сопряжение удалено");
        }
        Cmd::Send { to, files } => send(&daemon, to, files).await?,
        Cmd::Status => {
            let devices = daemon.devices().await?;
            let online: Vec<&str> =
                devices.iter().filter(|d| d.3).map(|d| d.1.as_str()).collect();
            let status = serde_json::json!({
                "text": online.join(", "),
                "tooltip": if online.is_empty() { "Linet: нет подключений".to_string() } else { format!("Linet: {}", online.join(", ")) },
                "class": if online.is_empty() { "disconnected" } else { "connected" },
            });
            println!("{status}");
        }
    }
    Ok(())
}

async fn pair(daemon: &DaemonProxy<'_>, uri_only: bool, light: bool) -> Result<()> {
    let known: Vec<String> = daemon.devices().await?.into_iter().map(|d| d.0).collect();
    let uri = daemon.start_pairing().await?;
    if uri_only {
        println!("{uri}");
    } else {
        let code = QrCode::new(uri.as_bytes())?;
        // В тёмном терминале «светлые» модули рисуем блоками, иначе QR выйдет инвертированным.
        let (dark, bright) =
            if light { (Dense1x2::Dark, Dense1x2::Light) } else { (Dense1x2::Light, Dense1x2::Dark) };
        let image = code.render::<Dense1x2>().dark_color(dark).light_color(bright).quiet_zone(true).build();
        println!("{image}");
        println!("Отсканируй код в приложении Linet на телефоне. Код действует 2 минуты.");
    }

    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let devices = daemon.devices().await?;
        if let Some(new) = devices.iter().find(|d| !known.contains(&d.0)) {
            println!("Сопряжено: {} ({})", new.1, new.0);
            return Ok(());
        }
    }
    daemon.cancel_pairing().await?;
    bail!("время ожидания истекло");
}

async fn send(daemon: &DaemonProxy<'_>, to: Option<String>, files: Vec<std::path::PathBuf>) -> Result<()> {
    use std::collections::HashSet;
    use std::io::Write;

    use futures_util::StreamExt;

    let to = match to {
        Some(id) => id,
        None => {
            let online: Vec<_> = daemon.devices().await?.into_iter().filter(|d| d.3).collect();
            match online.as_slice() {
                [one] => one.0.clone(),
                [] => bail!("нет подключённых устройств"),
                _ => bail!("подключено несколько устройств, укажи --to"),
            }
        }
    };
    // Подписываемся до начала отправки, чтобы не пропустить быстрые сигналы.
    let mut progress = daemon.receive_transfer_progress().await?;
    let mut finished = daemon.receive_transfer_finished().await?;

    let mut pending = HashSet::new();
    for f in &files {
        let path = std::fs::canonicalize(f).with_context(|| format!("{f:?}"))?;
        pending.insert(daemon.send_file(&to, &path.to_string_lossy()).await?);
    }

    let mut failed = 0;
    while !pending.is_empty() {
        tokio::select! {
            Some(sig) = progress.next() => {
                let a = sig.args()?;
                if pending.contains(&a.id) && a.total > 0 {
                    let pct = a.done * 100 / a.total;
                    print!("\r{}: {pct}% ({} / {})   ", a.name, human(a.done), human(a.total));
                    std::io::stdout().flush()?;
                }
            }
            Some(sig) = finished.next() => {
                let a = sig.args()?;
                if pending.remove(&a.id) {
                    if a.ok {
                        println!("\r{}: отправлено ({})          ", a.name, a.peer);
                    } else {
                        failed += 1;
                        println!("\r{}: ошибка: {}          ", a.name, a.detail);
                    }
                }
            }
            else => bail!("демон пропал с шины"),
        }
    }
    if failed > 0 {
        bail!("не отправлено файлов: {failed}");
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["Б", "КБ", "МБ", "ГБ"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} Б") } else { format!("{v:.1} {}", UNITS[unit]) }
}
