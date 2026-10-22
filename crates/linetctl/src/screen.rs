//! Экран телефона на ноутбуке через scrcpy по беспроводной отладке.
//!
//! Беспроводная отладка на телефоне включается только на время трансляции:
//! Linet просит телефон включить её, находит порт через mDNS (`adb mdns services`),
//! подключается, запускает scrcpy и после закрытия окна всё выключает.
//! Сопряжение кодом не нужно, если ключ ноутбука уже разрешён по USB.
//! Если телефон подключён проводом, используется USB — без всякой отладки по сети.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use linetctl::DaemonProxy;

const DISCOVER_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn run(daemon: &DaemonProxy<'_>, app: Option<String>, pick_app: bool) -> Result<()> {
    // Телефон на проводе — сразу по USB: быстрее и работает в любой сети,
    // в том числе когда телефон раздаёт точку доступа.
    if let Some(serial) = usb_device() {
        return mirror(&serial, app, pick_app, false);
    }
    let ip = daemon.screen_share(true).await.context("телефон не включил беспроводную отладку")?;
    let result = connect(&ip).and_then(|serial| mirror(&serial, app, pick_app, true));
    // Закрываем отладку в любом случае, даже если трансляция не удалась.
    let _ = daemon.screen_share(false).await;
    result
}

/// Устройство, подключённое по USB (в `adb devices -l` у него есть `usb:`).
fn usb_device() -> Option<String> {
    let out = Command::new("adb").args(["devices", "-l"]).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(" device ") && l.contains(" usb:"))
        .find_map(|l| l.split_whitespace().next().map(str::to_string))
}

/// `wireless` — подключение по Wi-Fi, его нужно закрыть после трансляции.
fn mirror(serial: &str, app: Option<String>, pick_app: bool, wireless: bool) -> Result<()> {
    let done = |serial: &str| {
        if wireless {
            disconnect(serial);
        }
    };
    let app = match (app, pick_app) {
        (Some(pkg), _) => Some(pkg),
        (None, true) => match pick(serial)? {
            Some(pkg) => Some(pkg),
            None => {
                done(serial);
                return Ok(());
            }
        },
        (None, false) => None,
    };
    let mut cmd = Command::new("scrcpy");
    cmd.args(["-s", serial, "--window-title=Телефон"]);
    if let Some(pkg) = &app {
        // Приложение — в отдельном виртуальном экране, телефон при этом свободен.
        cmd.args(["--new-display", &format!("--start-app={pkg}")]);
    }
    let status = cmd.status().context("нужен scrcpy: sudo pacman -S scrcpy")?;
    done(serial);
    if !status.success() {
        bail!("scrcpy завершился с ошибкой");
    }
    Ok(())
}

/// Ждёт, пока телефон объявит порт беспроводной отладки, и подключается.
fn connect(ip: &str) -> Result<String> {
    let deadline = Instant::now() + DISCOVER_TIMEOUT;
    loop {
        if let Some(addr) = find_port(ip) {
            let out = Command::new("adb").args(["connect", &addr]).output().context("нужен adb")?;
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains("connected to") {
                return Ok(addr);
            }
        }
        if Instant::now() >= deadline {
            bail!("телефон не появился в сети по беспроводной отладке (он в той же Wi-Fi сети?)");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn find_port(ip: &str) -> Option<String> {
    let out = Command::new("adb").args(["mdns", "services"]).stderr(Stdio::null()).output().ok()?;
    let prefix = format!("{ip}:");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("_adb-tls-connect._tcp"))
        .filter_map(|l| l.split_whitespace().last())
        .find(|addr| addr.starts_with(&prefix))
        .map(str::to_string)
}

fn disconnect(serial: &str) {
    let _ = Command::new("adb").args(["disconnect", serial]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Список приложений телефона в fuzzel. `None` — отмена.
fn pick(serial: &str) -> Result<Option<String>> {
    let out = Command::new("scrcpy").args(["-s", serial, "--list-apps"]).stderr(Stdio::null()).output()?;
    let apps: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            // Строки вида «* Telegram    org.telegram.messenger» (* — системное, - — установленное).
            let rest = l.strip_prefix("* ").or_else(|| l.strip_prefix("- "))?;
            let (name, pkg) = rest.trim_end().rsplit_once(char::is_whitespace)?;
            Some((name.trim().to_string(), pkg.to_string()))
        })
        .collect();
    if apps.is_empty() {
        bail!("не удалось получить список приложений");
    }
    let menu: String = apps.iter().map(|(n, p)| format!("{n}  ({p})\n")).collect();
    let mut child = Command::new("fuzzel")
        .args(["--dmenu", "--prompt=Приложение с телефона: "])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("нужен fuzzel")?;
    std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), menu.as_bytes())?;
    let out = child.wait_with_output()?;
    let choice = String::from_utf8_lossy(&out.stdout);
    let choice = choice.trim();
    Ok(apps.iter().find(|(n, p)| choice == format!("{n}  ({p})")).map(|(_, p)| p.clone()))
}
