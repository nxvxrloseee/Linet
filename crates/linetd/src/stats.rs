//! Счётчики для `linetctl stats`: подключения, трафик, память демона и скорость
//! разряда телефона. Нужны, чтобы вовремя заметить утечку или прожорливость.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Сколько хранить замеров заряда (по одному на процент — хватает на несколько суток).
const MAX_SAMPLES: usize = 512;
/// Окно для скорости разряда: последние часы без зарядки.
const DRAIN_WINDOW: Duration = Duration::from_secs(6 * 3600);
/// Меньше этого промежутка — оценка слишком шумная.
const MIN_DRAIN_SPAN: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct Inner {
    connects: u64,
    disconnects: u64,
    /// Трафик уже закрытых сеансов (байты UDP).
    closed_tx: u64,
    closed_rx: u64,
    /// (когда, заряд) — только пока телефон не заряжается.
    battery: VecDeque<(Instant, u8)>,
}

pub struct Stats {
    started: Instant,
    inner: Mutex<Inner>,
}

impl Stats {
    pub fn new() -> Self {
        Self { started: Instant::now(), inner: Mutex::default() }
    }

    pub fn connected(&self) {
        self.inner.lock().unwrap().connects += 1;
    }

    pub fn disconnected(&self, conn: &quinn::Connection) {
        let s = conn.stats();
        let mut i = self.inner.lock().unwrap();
        i.disconnects += 1;
        i.closed_tx += s.udp_tx.bytes;
        i.closed_rx += s.udp_rx.bytes;
    }

    pub fn battery(&self, level: u8, charging: bool) {
        let mut i = self.inner.lock().unwrap();
        if charging {
            // На зарядке разряд не меряем, а прошлые замеры уже не про текущий разряд.
            i.battery.clear();
            return;
        }
        if i.battery.back().is_none_or(|&(_, l)| l != level) {
            i.battery.push_back((Instant::now(), level));
            if i.battery.len() > MAX_SAMPLES {
                i.battery.pop_front();
            }
        }
    }

    /// Процентов в час за последние часы без зарядки.
    fn drain_per_hour(&self) -> Option<f64> {
        let i = self.inner.lock().unwrap();
        let now = Instant::now();
        let mut recent = i.battery.iter().filter(|(t, _)| now.duration_since(*t) <= DRAIN_WINDOW);
        let &(t0, l0) = recent.next()?;
        let &(t1, l1) = i.battery.back()?;
        let span = t1.duration_since(t0);
        if span < MIN_DRAIN_SPAN {
            return None;
        }
        Some((f64::from(l0) - f64::from(l1)) / span.as_secs_f64() * 3600.0)
    }

    /// Пары (ключ, значение) для D-Bus. `open` — текущие сеансы, их трафик тоже учитываем.
    pub fn report(&self, open: &[quinn::Connection], battery: Option<(u8, bool)>) -> Vec<(String, String)> {
        let (mut tx, mut rx) = {
            let i = self.inner.lock().unwrap();
            (i.closed_tx, i.closed_rx)
        };
        for c in open {
            let s = c.stats();
            tx += s.udp_tx.bytes;
            rx += s.udp_rx.bytes;
        }
        let i = self.inner.lock().unwrap();
        let rss_kb = read_status("VmRSS").unwrap_or(0);
        let hwm_kb = read_status("VmHWM").unwrap_or(0);
        let mut out = vec![
            ("uptime_s".into(), self.started.elapsed().as_secs().to_string()),
            ("connects".into(), i.connects.to_string()),
            ("disconnects".into(), i.disconnects.to_string()),
            ("tx_bytes".into(), tx.to_string()),
            ("rx_bytes".into(), rx.to_string()),
            ("rss_kb".into(), rss_kb.to_string()),
            ("rss_peak_kb".into(), hwm_kb.to_string()),
            ("threads".into(), read_status("Threads").unwrap_or(0).to_string()),
        ];
        drop(i);
        if let Some((level, charging)) = battery {
            out.push(("phone_battery".into(), level.to_string()));
            out.push(("phone_charging".into(), charging.to_string()));
        }
        if let Some(rate) = self.drain_per_hour() {
            out.push(("phone_drain_per_hour".into(), format!("{rate:.2}")));
        }
        out
    }
}

/// Поле из /proc/self/status в килобайтах (или числом для Threads).
fn read_status(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}
