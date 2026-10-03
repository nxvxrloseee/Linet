use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};

/// SHA-256 от DER-сертификата устройства. Это и есть его идентичность.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of_cert(der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, der);
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_ref());
        Self(out)
    }

    /// Короткий идентификатор устройства для UI и CLI.
    pub fn device_id(&self) -> String {
        hex::encode(&self.0[..8])
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).context("отпечаток не в hex")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("отпечаток должен быть 32 байта"))?;
        Ok(Self(arr))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.device_id())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Долговременный ключ Ed25519 и самоподписанный сертификат устройства.
pub struct Identity {
    cert: CertificateDer<'static>,
    key_pkcs8: Vec<u8>,
    fingerprint: Fingerprint,
}

const CERT_FILE: &str = "identity.crt.der";
const KEY_FILE: &str = "identity.key.der";

impl Identity {
    pub fn generate() -> Result<Self> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let params = rcgen::CertificateParams::new(vec![crate::SERVER_NAME.to_string()])?;
        let cert = params.self_signed(&key)?;
        Ok(Self::from_parts(cert.der().to_vec(), key.serialize_der()))
    }

    fn from_parts(cert: Vec<u8>, key_pkcs8: Vec<u8>) -> Self {
        let fingerprint = Fingerprint::of_cert(&cert);
        Self { cert: CertificateDer::from(cert), key_pkcs8, fingerprint }
    }

    /// Загружает идентичность из `dir` или создаёт новую при первом запуске.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        if cert_path.exists() && key_path.exists() {
            let cert = std::fs::read(&cert_path).with_context(|| format!("чтение {cert_path:?}"))?;
            let key = std::fs::read(&key_path).with_context(|| format!("чтение {key_path:?}"))?;
            return Ok(Self::from_parts(cert, key));
        }
        std::fs::create_dir_all(dir)?;
        let id = Self::generate()?;
        crate::store::write_private(&key_path, &id.key_pkcs8)?;
        std::fs::write(&cert_path, id.cert.as_ref())?;
        tracing::info!(id = %id.fingerprint.device_id(), "создана новая идентичность");
        Ok(id)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn cert(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }

    /// Ключ для stateless reset в QUIC, детерминированный от ключа устройства.
    /// Благодаря ему после перезапуска демона телефон сразу узнаёт, что старое
    /// соединение мертво, а не ждёт idle-таймаута.
    pub fn reset_key(&self) -> ring::hmac::Key {
        let salt = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"linet-quic-reset-v1");
        let derived = ring::hmac::sign(&salt, &self.key_pkcs8);
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, derived.as_ref())
    }
}
