//! Экран ноутбука на телефоне через wayvnc.
//!
//! Запускается по запросу телефона и только на время просмотра: wayvnc слушает
//! лишь адрес ноутбука в той сети, где сейчас телефон, с одноразовым паролем и
//! шифрованием (TLS или RSA-AES — что выберет клиент). Клиент отключился или так
//! и не подключился за минуту — wayvnc закрывается. В режиме «второй монитор»
//! Hyprland создаёт виртуальный экран по размеру телефона и удаляет его после.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const PORT: u16 = 5910;
const HEADLESS: &str = "LINET-PHONE";
/// Сколько ждать подключения клиента, прежде чем закрыть wayvnc.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Высота второго монитора в логических точках, на которую рассчитаны окна
/// настольных приложений: меньше — и диалоги не помещаются.
const MIN_LOGICAL_HEIGHT: u32 = 720;
const MAX_FPS: &str = "30";

struct Running {
    child: Child,
    extend: bool,
    generation: u64,
}

pub struct Vnc {
    keys: PathBuf,
    host: String,
    running: Mutex<Option<Running>>,
    generation: std::sync::atomic::AtomicU64,
}

impl Vnc {
    pub fn new(data_dir: &Path, host: String) -> Arc<Self> {
        Arc::new(Self {
            keys: data_dir.join("vnc"),
            host,
            running: Mutex::new(None),
            generation: Default::default(),
        })
    }

    /// Запускает wayvnc и возвращает ссылку `vnc://` для клиента на телефоне.
    pub async fn start(self: &Arc<Self>, phone: IpAddr, extend: bool, width: u32, height: u32) -> Result<String> {
        self.stop().await;
        self.ensure_keys().await?;
        let local = local_ip_towards(phone)?;
        let password = random_password()?;

        let output = if extend {
            create_headless(width, height).await?;
            HEADLESS.to_string()
        } else {
            focused_monitor().await?
        };

        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let config = runtime.join("linet-wayvnc.cfg");
        let socket = runtime.join("linet-wayvncctl");
        let body = format!(
            "address={local}\nport={PORT}\nenable_auth=true\nusername=linet\npassword={password}\n\
             private_key_file={k}/tls_key.pem\ncertificate_file={k}/tls_cert.pem\nrsa_private_key_file={k}/rsa_key.pem\n",
            k = self.keys.display()
        );
        let xkb_dir = self.keys.join("xkb");
        let body = body + &keyboard_config(&xkb_dir).await;
        linet_core::store::write_private(&config, body.as_bytes())?;

        let child = Command::new("wayvnc")
            // Отсюда libxkbcommon берёт нашу раскладку «linet».
            .env("XKB_CONFIG_EXTRA_PATH", &xkb_dir)
            .arg(format!("--config={}", config.display()))
            .arg(format!("--output={output}"))
            .arg(format!("--socket={}", socket.display()))
            .arg(format!("--max-fps={MAX_FPS}"))
            .arg("--render-cursor")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("нужен wayvnc: sudo pacman -S wayvnc")?;
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        *self.running.lock().await = Some(Running { child, extend, generation });

        if let Err(e) = wait_listening(SocketAddr::new(local, PORT)).await {
            self.stop().await;
            return Err(e);
        }
        tracing::info!(%output, extend, "экран ноутбука открыт для телефона");
        self.clone().watch(socket, generation);

        Ok(format!(
            "vnc://{ip}:{PORT}?ConnectionName={name}&VncUsername=linet&VncPassword={password}",
            ip = local,
            name = urlencode(&self.host),
        ))
    }

    pub async fn stop(&self) {
        let Some(mut run) = self.running.lock().await.take() else { return };
        let _ = run.child.kill().await;
        if run.extend {
            let _ = Command::new("hyprctl").args(["output", "remove", HEADLESS]).output().await;
        }
        tracing::info!("показ экрана ноутбука закончен");
    }

    /// Закрывает wayvnc, когда клиент ушёл или так и не пришёл.
    fn watch(self: Arc<Self>, socket: PathBuf, generation: u64) {
        tokio::spawn(async move {
            let still_ours = |vnc: &Arc<Self>| {
                let vnc = vnc.clone();
                async move { vnc.running.lock().await.as_ref().is_some_and(|r| r.generation == generation) }
            };
            let events = Command::new("wayvncctl")
                .arg(format!("--socket={}", socket.display()))
                .args(["--json", "event-receive"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let Ok(mut events) = events else {
                tokio::time::sleep(CONNECT_TIMEOUT).await;
                return;
            };
            let mut lines = tokio::io::BufReader::new(events.stdout.take().expect("stdout")).lines();
            let mut connected = false;
            loop {
                let line = tokio::select! {
                    l = lines.next_line() => l,
                    _ = tokio::time::sleep(CONNECT_TIMEOUT), if !connected => {
                        tracing::info!("клиент VNC не подключился");
                        break;
                    }
                };
                let Ok(Some(line)) = line else { break };
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let count = event["params"]["connection_count"].as_u64();
                match event["method"].as_str() {
                    Some("client-connected") => connected = true,
                    Some("client-disconnected") if count == Some(0) => break,
                    _ => {}
                }
            }
            if still_ours(&self).await {
                self.stop().await;
            }
        });
    }

    /// Ключи для шифрования: создаются один раз (openssl).
    async fn ensure_keys(&self) -> Result<()> {
        let k = &self.keys;
        if k.join("tls_cert.pem").exists() && k.join("rsa_key.pem").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(k)?;
        let run = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            async move {
                let st = Command::new("openssl").args(&args).stdout(Stdio::null()).stderr(Stdio::null()).status().await?;
                if !st.success() {
                    bail!("openssl {}", args.join(" "));
                }
                anyhow::Ok(())
            }
        };
        let tls_key = k.join("tls_key.pem").display().to_string();
        let tls_cert = k.join("tls_cert.pem").display().to_string();
        let rsa_key = k.join("rsa_key.pem").display().to_string();
        run(&["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-sha256", "-days", "3650",
            "-nodes", "-subj", "/CN=linet", "-keyout", &tls_key, "-out", &tls_cert]).await?;
        run(&["genrsa", "-traditional", "-out", &rsa_key, "2048"]).await?;
        for f in [&tls_key, &rsa_key] {
            std::fs::set_permissions(f, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// Раскладки клавиатуры для wayvnc. По VNC телефон шлёт символы, а wayvnc ищет
/// для них клавишу в раскладке своей виртуальной клавиатуры.
///
/// С двумя раскладками (`us,ru`) wayvnc путается: набрав кириллицу, он остаётся
/// в группе `ru`, а точку ищет в `us` — и вместо «.» печатается «ю». Поэтому при
/// русской раскладке даём ему одну группу, где на каждой клавише и латиница,
/// и кириллица (уровни 3–4), — переключать группы не придётся.
async fn keyboard_config(xkb_dir: &Path) -> String {
    async fn option(name: &str) -> Option<String> {
        let out = Command::new("hyprctl").args(["getoption", name, "-j"]).output().await.ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let s = v["str"].as_str()?.trim().to_string();
        (!s.is_empty() && s != "[[EMPTY]]").then_some(s)
    }
    let layouts = option("input:kb_layout").await.unwrap_or_default();
    if layouts.split(',').any(|l| l.trim() == "ru") && write_combined_layout(xkb_dir).is_ok() {
        return format!("xkb_layout={COMBINED_LAYOUT}\n");
    }
    let mut cfg = String::new();
    for (hypr, key) in [("input:kb_layout", "xkb_layout"), ("input:kb_variant", "xkb_variant")] {
        if let Some(v) = option(hypr).await {
            cfg += &format!("{key}={v}\n");
        }
    }
    cfg
}

const COMBINED_LAYOUT: &str = "linet";

/// ЙЦУКЕН поверх QWERTY: клавиша → (строчная, заглавная).
const RU_KEYS: &[(&str, &str, &str)] = &[
    ("TLDE", "Cyrillic_io", "Cyrillic_IO"),
    ("AD01", "Cyrillic_shorti", "Cyrillic_SHORTI"),
    ("AD02", "Cyrillic_tse", "Cyrillic_TSE"),
    ("AD03", "Cyrillic_u", "Cyrillic_U"),
    ("AD04", "Cyrillic_ka", "Cyrillic_KA"),
    ("AD05", "Cyrillic_ie", "Cyrillic_IE"),
    ("AD06", "Cyrillic_en", "Cyrillic_EN"),
    ("AD07", "Cyrillic_ghe", "Cyrillic_GHE"),
    ("AD08", "Cyrillic_sha", "Cyrillic_SHA"),
    ("AD09", "Cyrillic_shcha", "Cyrillic_SHCHA"),
    ("AD10", "Cyrillic_ze", "Cyrillic_ZE"),
    ("AD11", "Cyrillic_ha", "Cyrillic_HA"),
    ("AD12", "Cyrillic_hardsign", "Cyrillic_HARDSIGN"),
    ("AC01", "Cyrillic_ef", "Cyrillic_EF"),
    ("AC02", "Cyrillic_yeru", "Cyrillic_YERU"),
    ("AC03", "Cyrillic_ve", "Cyrillic_VE"),
    ("AC04", "Cyrillic_a", "Cyrillic_A"),
    ("AC05", "Cyrillic_pe", "Cyrillic_PE"),
    ("AC06", "Cyrillic_er", "Cyrillic_ER"),
    ("AC07", "Cyrillic_o", "Cyrillic_O"),
    ("AC08", "Cyrillic_el", "Cyrillic_EL"),
    ("AC09", "Cyrillic_de", "Cyrillic_DE"),
    ("AC10", "Cyrillic_zhe", "Cyrillic_ZHE"),
    ("AC11", "Cyrillic_e", "Cyrillic_E"),
    ("AB01", "Cyrillic_ya", "Cyrillic_YA"),
    ("AB02", "Cyrillic_che", "Cyrillic_CHE"),
    ("AB03", "Cyrillic_es", "Cyrillic_ES"),
    ("AB04", "Cyrillic_em", "Cyrillic_EM"),
    ("AB05", "Cyrillic_i", "Cyrillic_I"),
    ("AB06", "Cyrillic_te", "Cyrillic_TE"),
    ("AB07", "Cyrillic_softsign", "Cyrillic_SOFTSIGN"),
    ("AB08", "Cyrillic_be", "Cyrillic_BE"),
    ("AB09", "Cyrillic_yu", "Cyrillic_YU"),
    ("AE03", "numerosign", "numerosign"),
];

/// Пишет `<xkb_dir>/symbols/linet`: US, а кириллица — на третьем-четвёртом уровне.
fn write_combined_layout(xkb_dir: &Path) -> std::io::Result<()> {
    let mut body = String::from(
        "// Создано Linet для wayvnc: латиница и кириллица в одной группе.\n\
         default partial alphanumeric_keys\n\
         xkb_symbols \"basic\" {\n    include \"us(basic)\"\n    name[Group1] = \"Linet (US + RU)\";\n",
    );
    for (key, lower, upper) in RU_KEYS {
        body += &format!("    key <{key}> {{ type[Group1] = \"FOUR_LEVEL\", [ NoSymbol, NoSymbol, {lower}, {upper} ] }};\n");
    }
    body += "    include \"level3(ralt_switch)\"\n};\n";
    let dir = xkb_dir.join("symbols");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(COMBINED_LAYOUT), body)
}

/// Адрес ноутбука на интерфейсе, через который виден телефон.
fn local_ip_towards(phone: IpAddr) -> Result<IpAddr> {
    let socket = std::net::UdpSocket::bind(match phone {
        IpAddr::V4(_) => SocketAddr::from(([0u8; 4], 0)),
        IpAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
    })?;
    socket.connect((phone, 9))?;
    Ok(socket.local_addr()?.ip())
}

fn random_password() -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("нет источника случайности"))?;
    Ok(bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn focused_monitor() -> Result<String> {
    let out = Command::new("hyprctl").args(["monitors", "-j"]).output().await?;
    let monitors: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    monitors
        .as_array()
        .and_then(|m| m.iter().find(|m| m["focused"] == true).or_else(|| m.first()))
        .and_then(|m| m["name"].as_str())
        .map(str::to_string)
        .context("не найден монитор")
}

/// Виртуальный монитор справа от основного, по размеру экрана телефона.
async fn create_headless(width: u32, height: u32) -> Result<()> {
    let _ = Command::new("hyprctl").args(["output", "remove", HEADLESS]).output().await;
    let out = Command::new("hyprctl").args(["output", "create", "headless", HEADLESS]).output().await?;
    if !String::from_utf8_lossy(&out.stdout).contains("ok") {
        bail!("Hyprland не создал виртуальный монитор");
    }
    // Всегда альбомный: настольные приложения под узкий портретный экран не рассчитаны.
    let (w, h) = (width.max(height).clamp(640, 3840), width.min(height).clamp(360, 2160));
    let scale = pick_scale(w, h);
    // Lua-конфиг Hyprland: настройка монитора через eval, а не keyword.
    let lua = format!(
        r#"hl.monitor({{ output = "{HEADLESS}", mode = "{w}x{h}@60", position = "auto-right", scale = {scale} }})"#
    );
    Command::new("hyprctl").args(["eval", &lua]).output().await?;
    // Hyprland применяет режим не мгновенно. Если запустить wayvnc раньше, он
    // захватит монитор ещё в размере по умолчанию, и картинка выйдет обрезанной.
    for _ in 0..30 {
        let out = Command::new("hyprctl").args(["monitors", "-j"]).output().await?;
        let monitors: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let ready = monitors.as_array().into_iter().flatten().any(|m| {
            m["name"] == HEADLESS && m["width"].as_u64() == Some(u64::from(w)) && m["height"].as_u64() == Some(u64::from(h))
        });
        if ready {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("Hyprland не применил размер {w}×{h} к виртуальному монитору")
}

/// Самый крупный масштаб (чтобы на маленьком экране не было мелко), при котором
/// высота не меньше `MIN_LOGICAL_HEIGHT`, а размеры делятся без остатка —
/// Hyprland принимает только такие масштабы.
fn pick_scale(w: u32, h: u32) -> f64 {
    const CANDIDATES: &[f64] = &[2.0, 1.875, 1.8, 1.75, 1.6, 1.5, 1.25, 1.2, 1.0];
    let exact = |v: u32, s: f64| ((f64::from(v) / s).fract()).abs() < 1e-9;
    CANDIDATES
        .iter()
        .copied()
        .find(|&s| f64::from(h) / s >= f64::from(MIN_LOGICAL_HEIGHT) && exact(w, s) && exact(h, s))
        .unwrap_or(1.0)
}

async fn wait_listening(addr: SocketAddr) -> Result<()> {
    for _ in 0..30 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("wayvnc не запустился")
}

#[cfg(test)]
mod tests {
    #[test]
    fn scale_fits_desktop_apps() {
        assert_eq!(super::pick_scale(2704, 1216), 1.6); // POCO X6 Pro → 1690×760
        assert_eq!(super::pick_scale(2400, 1080), 1.5); // 1600×720
        assert_eq!(super::pick_scale(1280, 720), 1.0);
    }

    #[test]
    fn combined_layout_written() {
        let dir = std::env::temp_dir().join(format!("linet-xkb-{}", std::process::id()));
        super::write_combined_layout(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join("symbols/linet")).unwrap();
        assert!(text.contains("Cyrillic_zhe") && text.contains("include \"us(basic)\""));
        if let Ok(out) = std::env::var("LINET_XKB_KEEP") {
            let _ = std::fs::create_dir_all(&out);
            std::fs::copy(dir.join("symbols/linet"), std::path::Path::new(&out).join("linet")).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
