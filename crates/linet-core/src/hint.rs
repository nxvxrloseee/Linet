//! Подсказка адреса через шлюз.
//!
//! Когда ноутбук подключён к точке доступа телефона, телефон — его шлюз по
//! умолчанию. Ноутбук шлёт на шлюз короткий UDP-пакет «я здесь, порт такой-то»,
//! чтобы телефон нашёл его, даже если подсеть точки доступа сменилась, а mDNS
//! в собственной точке доступа Android не работает.
//!
//! Подсказка лишь говорит, куда стучаться: само подключение проверяется по
//! закреплённому сертификату. Тем не менее она подписана ключом ноутбука, чтобы
//! чужие пакеты не гоняли телефон по ложным адресам. В пакете нет ни имени, ни id.
//!
//! Формат (82 байта): `"LNH1" ‖ порт u16 ‖ время u64 ‖ IPv4 ноутбука ‖ подпись Ed25519`.

use std::net::Ipv4Addr;

use anyhow::Result;

use crate::identity::Identity;

pub const HINT_PORT: u16 = 47322;
pub const HINT_LEN: usize = 4 + 2 + 8 + 4 + 64;
const MAGIC: &[u8; 4] = b"LNH1";
/// Допустимое расхождение часов и задержка доставки.
const MAX_SKEW_SECS: u64 = 300;

fn signed_part(port: u16, ts: u64, ip: Ipv4Addr) -> [u8; 18] {
    let mut out = [0u8; 18];
    out[..4].copy_from_slice(MAGIC);
    out[4..6].copy_from_slice(&port.to_be_bytes());
    out[6..14].copy_from_slice(&ts.to_be_bytes());
    out[14..18].copy_from_slice(&ip.octets());
    out
}

pub fn build(identity: &Identity, ip: Ipv4Addr, port: u16, ts: u64) -> Result<[u8; HINT_LEN]> {
    let head = signed_part(port, ts, ip);
    let sig = identity.sign(&head)?;
    let mut out = [0u8; HINT_LEN];
    out[..18].copy_from_slice(&head);
    out[18..].copy_from_slice(&sig);
    Ok(out)
}

/// Проверяет подсказку от `src` подписью из сертификата ноутбука.
/// Возвращает QUIC-порт ноутбука, если подсказка подлинная и свежая.
pub fn verify(packet: &[u8], src: Ipv4Addr, laptop_cert: &[u8], now: u64) -> Option<u16> {
    if packet.len() != HINT_LEN || &packet[..4] != MAGIC {
        return None;
    }
    let port = u16::from_be_bytes(packet[4..6].try_into().ok()?);
    let ts = u64::from_be_bytes(packet[6..14].try_into().ok()?);
    let ip = Ipv4Addr::from(<[u8; 4]>::try_from(&packet[14..18]).ok()?);
    // Адрес внутри подписи должен совпасть с отправителем: чужой не перешлёт
    // перехваченную подсказку со своего адреса.
    if ip != src || ts.abs_diff(now) > MAX_SKEW_SECS || port == 0 {
        return None;
    }
    let der = rustls::pki_types::CertificateDer::from(laptop_cert);
    let cert = webpki::EndEntityCert::try_from(&der).ok()?;
    cert.verify_signature(webpki::ring::ED25519, &packet[..18], &packet[18..]).ok()?;
    Some(port)
}

/// IPv4-адреса интерфейсов точки доступа (ap0, swlan0, softap0, …) и USB/Bluetooth-модема.
pub fn tether_addrs() -> Vec<Ipv4Addr> {
    const TETHER: &[&str] = &["ap", "swlan", "softap", "wlan1", "rndis", "usb", "ncm", "bt-pan"];
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| TETHER.iter().any(|t| i.name.starts_with(t)))
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_hint_roundtrip() {
        let id = Identity::generate().unwrap();
        let other = Identity::generate().unwrap();
        let ip = Ipv4Addr::new(10, 75, 214, 34);
        let pkt = build(&id, ip, 47321, 1000).unwrap();
        let cert = id.cert();
        assert_eq!(verify(&pkt, ip, &cert, 1100), Some(47321));
        // чужой ключ, другой отправитель, старая подсказка, испорченный пакет
        assert_eq!(verify(&pkt, ip, &other.cert(), 1100), None);
        assert_eq!(verify(&pkt, Ipv4Addr::new(10, 75, 214, 99), &cert, 1100), None);
        assert_eq!(verify(&pkt, ip, &cert, 1000 + MAX_SKEW_SECS + 1), None);
        let mut bad = pkt;
        bad[5] ^= 1;
        assert_eq!(verify(&bad, ip, &cert, 1100), None);
        assert_eq!(verify(&pkt[..80], ip, &cert, 1100), None);
    }
}
