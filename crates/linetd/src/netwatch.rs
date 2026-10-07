//! Слежение за сетью: подсказки адреса через шлюз и mDNS только в доверенных сетях.
//!
//! Доверенная сеть — подключение NetworkManager (Wi-Fi или Ethernet), через которое
//! уже подключался сопряжённый телефон. Список пополняется сам, настраивать нечего.
//! В остальных сетях ноутбук не объявляет себя по mDNS: в публичном Wi-Fi это
//! раскрывало бы имя хоста и id Linet всем вокруг.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use linet_core::hint;
use linet_core::store::write_private;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use crate::daemon::Daemon;

const TICK: Duration = Duration::from_secs(2);
/// Опрос NetworkManager реже, чем маршрутов: это вызовы D-Bus.
const NM_EVERY_TICKS: u32 = 5;
/// После смены сети подсказки идут чаще: телефону тоже нужно время.
const BURST: &[u64] = &[0, 2, 4, 10, 20, 40];
const HINT_EVERY: Duration = Duration::from_secs(60);
/// Дольше слать в сеть, где телефона, видимо, нет, смысла нет.
const HINT_FOR: Duration = Duration::from_secs(10 * 60);

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ActiveConnection {
    #[zbus(property)]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<String>;
}

pub struct Mdns {
    pub daemon: mdns_sd::ServiceDaemon,
    pub service: mdns_sd::ServiceInfo,
}

pub struct NetWatch {
    daemon: Arc<Daemon>,
    port: u16,
    mdns: Option<Mdns>,
    trusted_path: PathBuf,
    /// uuid → название подключения
    trusted: BTreeMap<String, String>,
    advertising: bool,
    gateway: Option<Ipv4Addr>,
    gateway_since: Instant,
    last_hint: Option<Instant>,
}

impl NetWatch {
    pub fn new(daemon: Arc<Daemon>, port: u16, mdns: Option<Mdns>, data_dir: PathBuf) -> Self {
        let trusted_path = data_dir.join("trusted_networks.json");
        let trusted = std::fs::read(&trusted_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            daemon,
            port,
            mdns,
            trusted_path,
            trusted,
            advertising: false,
            gateway: None,
            gateway_since: Instant::now(),
            last_hint: None,
        }
    }

    pub fn spawn(mut self) {
        tokio::spawn(async move {
            let nm = match zbus::Connection::system().await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("NetworkManager недоступен, mDNS будет включён всегда: {e}");
                    None
                }
            };
            let mut tick = tokio::time::interval(TICK);
            let mut n = 0u32;
            loop {
                tick.tick().await;
                if n.is_multiple_of(NM_EVERY_TICKS) {
                    self.check_trust(nm.as_ref()).await;
                }
                n = n.wrapping_add(1);
                self.maybe_hint().await;
            }
        });
    }

    /// Запоминает текущие сети как доверенные, если телефон подключён, и включает
    /// или выключает mDNS.
    async fn check_trust(&mut self, nm: Option<&zbus::Connection>) {
        let active = match nm {
            Some(conn) => physical_connections(conn).await.map_err(|e| tracing::debug!("NM: {e}")).ok(),
            None => None,
        };
        if let Some(active) = &active
            && self.daemon.connected()
        {
            let mut changed = false;
            for (uuid, name) in active {
                if !self.trusted.contains_key(uuid) {
                    tracing::info!(network = %name, "сеть стала доверенной");
                    self.trusted.insert(uuid.clone(), name.clone());
                    changed = true;
                }
            }
            if changed && let Ok(json) = serde_json::to_vec_pretty(&self.trusted) {
                let _ = write_private(&self.trusted_path, &json);
            }
        }
        // Без NetworkManager отличить сети нельзя — ведём себя как раньше.
        let advertise = match &active {
            Some(active) => active.keys().any(|u| self.trusted.contains_key(u)),
            None => true,
        };
        self.set_advertising(advertise);
    }

    fn set_advertising(&mut self, on: bool) {
        let Some(mdns) = &self.mdns else { return };
        if on == self.advertising {
            return;
        }
        let res = if on {
            mdns.daemon.register(mdns.service.clone()).map(|_| ())
        } else {
            mdns.daemon.unregister(mdns.service.get_fullname()).map(|_| ())
        };
        match res {
            Ok(()) => {
                self.advertising = on;
                tracing::info!(on, "mDNS-анонс");
            }
            Err(e) => tracing::warn!("mDNS: {e}"),
        }
    }

    /// Пока телефон не подключён, подсказываем шлюзу свой адрес.
    async fn maybe_hint(&mut self) {
        let gateway = default_gateway();
        if gateway != self.gateway {
            self.gateway = gateway;
            self.gateway_since = Instant::now();
            self.last_hint = None;
        }
        let Some(gw) = gateway else { return };
        if self.daemon.connected() {
            return;
        }
        let age = self.gateway_since.elapsed();
        if age > HINT_FOR {
            return;
        }
        let due = match self.last_hint {
            None => true,
            Some(last) => {
                let in_burst = BURST.iter().any(|&s| {
                    let at = Duration::from_secs(s);
                    age >= at && last.duration_since(self.gateway_since) < at
                });
                in_burst || last.elapsed() >= HINT_EVERY
            }
        };
        if !due {
            return;
        }
        self.last_hint = Some(Instant::now());
        if let Err(e) = send_hint(&self.daemon, gw, self.port).await {
            tracing::debug!(%gw, "подсказка: {e:#}");
        }
    }
}

async fn send_hint(daemon: &Daemon, gw: Ipv4Addr, port: u16) -> Result<()> {
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.connect((gw, hint::HINT_PORT)).await?;
    let SocketAddr::V4(local) = socket.local_addr()? else { anyhow::bail!("не IPv4") };
    let packet = hint::build(daemon.identity(), *local.ip(), port, linet_core::store::now_unix())?;
    socket.send(&packet).await?;
    tracing::debug!(%gw, "подсказка адреса отправлена");
    Ok(())
}

/// Активные подключения Wi-Fi и Ethernet: uuid → название.
async fn physical_connections(conn: &zbus::Connection) -> Result<BTreeMap<String, String>> {
    let nm = NetworkManagerProxy::builder(conn).cache_properties(CacheProperties::No).build().await?;
    let mut out = BTreeMap::new();
    for path in nm.active_connections().await? {
        let ac = ActiveConnectionProxy::builder(conn)
            .path(path)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        if matches!(ac.kind().await?.as_str(), "802-11-wireless" | "802-3-ethernet") {
            out.insert(ac.uuid().await?, ac.id().await?);
        }
    }
    Ok(out)
}

/// Шлюз по умолчанию через физический интерфейс (VPN-туннели и мосты пропускаем).
fn default_gateway() -> Option<Ipv4Addr> {
    const VIRTUAL: &[&str] = &["tun", "tap", "wg", "docker", "br-", "veth", "virbr", "tailscale", "zt"];
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (iface, dest, gw, metric) = (f.first()?, f.get(1)?, f.get(2)?, f.get(6)?);
            if *dest != "00000000" || VIRTUAL.iter().any(|v| iface.contains(v)) {
                return None;
            }
            // В /proc/net/route адрес записан в hex в порядке байт хоста (little-endian).
            let gw = u32::from_str_radix(gw, 16).ok()?;
            let metric: u32 = metric.parse().ok()?;
            (gw != 0).then(|| (metric, Ipv4Addr::from(gw.to_le_bytes())))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, gw)| gw)
}
