use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;

use linetctl::DaemonProxy;

mod screen;
mod status;

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
    /// Структурированный снимок состояния, JSON schema_version=1.
    Snapshot,
    /// Непрерывный поток снимков NDJSON, включая состояние недоступного демона.
    Watch,
    /// Возможности и состояние backend блокировки (JSON).
    LockStatus,
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
    /// Открыть ссылку на телефоне (Handoff)
    Open {
        url: String,
        #[arg(long, default_value = "")]
        title: String,
        /// Устройство (по умолчанию — единственное подключённое)
        #[arg(long, default_value = "")]
        to: String,
    },
    /// Разблокировать экран телефоном (для горячей клавиши на экране блокировки)
    Unlock,
    /// Отправить файлы на телефон
    Send {
        /// Устройство (по умолчанию — единственное подключённое)
        #[arg(long)]
        to: Option<String>,
        /// Выбрать файлы в окне и сообщить результат уведомлением (для трея и меню)
        #[arg(long)]
        pick: bool,
        /// Сообщить результат уведомлением, а не в терминал («Открыть с помощью» в файловом менеджере)
        #[arg(long)]
        notify: bool,
        files: Vec<std::path::PathBuf>,
    },
    /// Экран телефона на ноутбуке (scrcpy по Wi-Fi)
    Screen {
        /// Открыть отдельное приложение в своём окне (имя пакета)
        #[arg(long)]
        app: Option<String>,
        /// Выбрать приложение из списка
        #[arg(long)]
        pick_app: bool,
        /// Сообщать об ошибках уведомлением (запуск из меню)
        #[arg(long)]
        notify: bool,
    },
    /// Обновить приложение на телефоне (по умолчанию — свежий release-APK из android/)
    UpdatePhone {
        apk: Option<std::path::PathBuf>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Состояние одной строкой JSON (для waybar)
    Status,
    /// Счётчики: подключения, трафик, память демона, разряд телефона
    Stats {
        /// Вывести JSON (для скриптов)
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let conn = zbus::Connection::session().await?;
    let daemon = DaemonProxy::new(&conn).await?;
    match args.cmd {
        Cmd::Snapshot => return status::once(&daemon).await,
        Cmd::Watch => return status::watch(&daemon).await,
        Cmd::LockStatus => { println!("{}", daemon.lock_status().await?); return Ok(()); },
        _ => {},
    }
    daemon.info().await.context("linetd не отвечает — он запущен?")?;

    match args.cmd {
        Cmd::Snapshot | Cmd::Watch | Cmd::LockStatus => unreachable!(),
        Cmd::Devices => {
            let devices = daemon.devices().await?;
            if devices.is_empty() {
                println!("Нет сопряжённых устройств. Запусти `linetctl pair`.");
            }
            for (id, name, kind, connected) in devices {
                let state = if connected { "● подключено" } else { "○ не в сети" };
                let battery = match daemon.battery(&id).await {
                    Ok((level, charging)) if level >= 0 => {
                        format!("  🔋{level}%{}", if charging { " ⚡" } else { "" })
                    }
                    _ => String::new(),
                };
                println!("{id}  {name:<20} {kind:<8} {state}{battery}");
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
        Cmd::Send { to, pick, notify, mut files } => {
            let gui = pick || notify;
            if pick || files.is_empty() {
                files = pick_files()?;
                if files.is_empty() {
                    return Ok(());
                }
            }
            let result = send(&daemon, to, files, Kind::File, !gui).await;
            if gui {
                desktop_notify(&result);
            }
            result?;
        }
        Cmd::Screen { app, pick_app, notify } => {
            let result = screen::run(&daemon, app, pick_app).await;
            if notify && let Err(e) = &result {
                desktop_notify(&Err(anyhow::anyhow!("{e:#}")));
            }
            result?;
        }
        Cmd::UpdatePhone { apk, to } => {
            let apk = match apk {
                Some(p) => p,
                None => std::path::PathBuf::from(DEFAULT_APK),
            };
            if !apk.is_file() {
                bail!("нет {apk:?}: собери release-APK или укажи путь");
            }
            send(&daemon, to, vec![apk], Kind::AppUpdate, true).await?;
            println!("Подтверди установку в уведомлении на телефоне.");
        }
        Cmd::Unlock => {
            if daemon.request_unlock().await? {
                println!("Разблокировано");
            } else {
                bail!("не разблокировано: экран не заблокирован или телефон отказал");
            }
        }
        Cmd::Open { url, title, to } => {
            daemon.send_url(&to, &url, &title).await?;
            println!("Открыто на телефоне");
        }
        Cmd::Stats { json } => {
            let stats: std::collections::BTreeMap<String, String> = daemon.stats().await?.into_iter().collect();
            if json {
                println!("{}", serde_json::to_string(&stats)?);
                return Ok(());
            }
            let num = |k: &str| stats.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let up = num("uptime_s");
            println!("Демон работает     {}ч {}м", up / 3600, up / 60 % 60);
            println!("Подключений        {} (обрывов {})", num("connects"), num("disconnects"));
            println!("Трафик             ↑ {}  ↓ {}", human(num("tx_bytes")), human(num("rx_bytes")));
            println!(
                "Память демона      {} (пик {}), потоков {}",
                human(num("rss_kb") * 1024),
                human(num("rss_peak_kb") * 1024),
                num("threads")
            );
            if let Some(level) = stats.get("phone_battery") {
                let charging = stats.get("phone_charging").is_some_and(|c| c == "true");
                println!("Батарея телефона   {level}%{}", if charging { ", заряжается" } else { "" });
            }
            match stats.get("phone_drain_per_hour") {
                Some(rate) => println!("Разряд телефона    {rate} %/ч (последние часы без зарядки)"),
                None => println!("Разряд телефона    пока мало данных (нужно ≥30 мин без зарядки)"),
            }
        }
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
        println!("Затем подтверди новое устройство в уведомлении на ноутбуке.");
    }

    // 2 минуты на сканирование и ещё минута на подтверждение в уведомлении.
    let deadline = Instant::now() + Duration::from_secs(180);
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

/// Путь к release-APK относительно корня проекта.
const DEFAULT_APK: &str = "android/app/build/outputs/apk/release/app-release.apk";

#[derive(Clone, Copy)]
enum Kind {
    File,
    AppUpdate,
}

/// Окно выбора файлов (zenity). Пустой список — отмена.
fn pick_files() -> Result<Vec<std::path::PathBuf>> {
    let out = std::process::Command::new("zenity")
        .args(["--file-selection", "--multiple", "--separator=\n", "--title=Отправить на телефон"])
        .output()
        .context("нужен zenity для выбора файлов")?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(Into::into).collect())
}

/// Итог отправки уведомлением: когда запускали из трея или файлового менеджера, терминала нет.
fn desktop_notify(result: &Result<String>) {
    let (summary, body, icon) = match result {
        Ok(summary) => (summary.clone(), String::new(), "phone-symbolic"),
        Err(e) => ("Не удалось отправить".to_string(), format!("{e:#}"), "dialog-error-symbolic"),
    };
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Linet", "-i", icon, &summary, &body])
        .status();
}

/// Отправляет файлы и ждёт итога. Возвращает строку-итог («Отправлено 2 файла на …»).
async fn send(
    daemon: &DaemonProxy<'_>,
    to: Option<String>,
    files: Vec<std::path::PathBuf>,
    kind: Kind,
    tty: bool,
) -> Result<String> {
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
        let path = path.to_string_lossy();
        let id = match kind {
            Kind::File => daemon.send_file(&to, &path).await?,
            Kind::AppUpdate => daemon.send_app_update(&to, &path).await?,
        };
        pending.insert(id);
    }
    let total = pending.len();
    let mut peer = String::new();

    let mut failed = 0;
    while !pending.is_empty() {
        tokio::select! {
            Some(sig) = progress.next() => {
                let a = sig.args()?;
                if tty && pending.contains(&a.id) && a.total > 0 {
                    let pct = a.done * 100 / a.total;
                    print!("\r{}: {pct}% ({} / {})   ", a.name, human(a.done), human(a.total));
                    std::io::stdout().flush()?;
                }
            }
            Some(sig) = finished.next() => {
                let a = sig.args()?;
                if pending.remove(&a.id) {
                    peer = a.peer.clone();
                    if a.ok {
                        if tty {
                            println!("\r{}: отправлено ({})          ", a.name, a.peer);
                        }
                    } else {
                        failed += 1;
                        if tty {
                            println!("\r{}: ошибка: {}          ", a.name, a.detail);
                        }
                    }
                }
            }
            else => bail!("демон пропал с шины"),
        }
    }
    if failed > 0 {
        bail!("не отправлено файлов: {failed} из {total}");
    }
    Ok(if total == 1 { format!("Отправлено на {peer}") } else { format!("Отправлено файлов: {total}, на {peer}") })
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
