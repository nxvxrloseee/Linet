//! Сопряжение по QR-коду.
//!
//! Ноутбук показывает `linet://pair?v=1&fp=<hex>&s=<hex>&a=<addr>,<addr>`.
//! Телефон подключается, проверяя сертификат сервера по `fp`, и доказывает
//! знание одноразового секрета: `HMAC-SHA256(s, "linet-pair-v1" ‖ fp_server ‖ fp_client)`.
//! Отпечаток клиента входит в доказательство, поэтому перехваченное
//! доказательство не подходит для другого устройства.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

use crate::identity::Fingerprint;

pub const SECRET_LEN: usize = 16;
const PREFIX: &str = "linet://pair?";
const DOMAIN: &[u8] = b"linet-pair-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub fingerprint: Fingerprint,
    pub secret: [u8; SECRET_LEN],
    pub addrs: Vec<SocketAddr>,
}

impl Invite {
    pub fn new(fingerprint: Fingerprint, addrs: Vec<SocketAddr>) -> Result<Self> {
        let mut secret = [0u8; SECRET_LEN];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| anyhow::anyhow!("нет источника случайности"))?;
        Ok(Self { fingerprint, secret, addrs })
    }

    pub fn to_uri(&self) -> String {
        let addrs: Vec<String> = self.addrs.iter().map(|a| a.to_string()).collect();
        format!(
            "{PREFIX}v=1&fp={}&s={}&a={}",
            self.fingerprint.to_hex(),
            hex::encode(self.secret),
            addrs.join(",")
        )
    }

    pub fn parse(uri: &str) -> Result<Self> {
        let query = uri.trim().strip_prefix(PREFIX).context("это не приглашение Linet")?;
        let (mut fp, mut secret, mut addrs, mut version) = (None, None, Vec::new(), None);
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "v" => version = Some(v),
                "fp" => fp = Some(Fingerprint::from_hex(v)?),
                "s" => {
                    let bytes = hex::decode(v).context("секрет не в hex")?;
                    secret = Some(
                        <[u8; SECRET_LEN]>::try_from(bytes.as_slice())
                            .map_err(|_| anyhow::anyhow!("неверная длина секрета"))?,
                    );
                }
                "a" => {
                    for a in v.split(',').filter(|a| !a.is_empty()) {
                        addrs.push(a.parse().with_context(|| format!("неверный адрес {a}"))?);
                    }
                }
                _ => {}
            }
        }
        if version != Some("1") {
            bail!("неподдерживаемая версия приглашения");
        }
        if addrs.is_empty() {
            bail!("в приглашении нет адресов");
        }
        Ok(Self {
            fingerprint: fp.context("в приглашении нет отпечатка")?,
            secret: secret.context("в приглашении нет секрета")?,
            addrs,
        })
    }
}

fn message(server: &Fingerprint, client: &Fingerprint) -> Vec<u8> {
    [DOMAIN, &server.0, &client.0].concat()
}

pub fn proof(secret: &[u8; SECRET_LEN], server: &Fingerprint, client: &Fingerprint) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::sign(&key, &message(server, client)).as_ref().to_vec()
}

/// Сравнение за постоянное время.
pub fn verify(secret: &[u8; SECRET_LEN], server: &Fingerprint, client: &Fingerprint, proof: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, &message(server, client), proof).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let inv = Invite::new(
            Fingerprint([7; 32]),
            vec!["192.168.1.5:47321".parse().unwrap(), "[fd00::1]:47321".parse().unwrap()],
        )
        .unwrap();
        assert_eq!(Invite::parse(&inv.to_uri()).unwrap(), inv);
    }

    #[test]
    fn proof_binds_client() {
        let s = [1u8; SECRET_LEN];
        let (srv, a, b) = (Fingerprint([1; 32]), Fingerprint([2; 32]), Fingerprint([3; 32]));
        let p = proof(&s, &srv, &a);
        assert!(verify(&s, &srv, &a, &p));
        assert!(!verify(&s, &srv, &b, &p));
        assert!(!verify(&[0; SECRET_LEN], &srv, &a, &p));
    }
}
