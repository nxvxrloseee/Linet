//! Настройки QUIC с упором на экономию батареи телефона.
//!
//! Телефон всегда клиент и сам keepalive не шлёт: в глубоком сне его таймеры
//! всё равно не срабатывают, а будить CPU ради пинга дорого. Keepalive шлёт
//! ноутбук — редкие входящие пакеты Wi-Fi-модуль принимает почти бесплатно.
//! Idle-таймаут длинный, чтобы соединение переживало сон телефона.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, TransportConfig};

use crate::identity::{Fingerprint, Identity};
use crate::tls::{self, ClientPolicy, PinnedClientVerifier, PinnedServerVerifier};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const SERVER_KEEPALIVE: Duration = Duration::from_secs(10 * 60);

fn transport(keepalive: Option<Duration>) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("таймаут в пределах VarInt")));
    t.keep_alive_interval(keepalive);
    t
}

static TLS13: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

/// Серверный endpoint демона. Слушает dual-stack, если IPv6 доступен.
pub fn server_endpoint(identity: &Identity, policy: Arc<dyn ClientPolicy>, port: u16) -> Result<Endpoint> {
    let mut crypto = rustls::ServerConfig::builder_with_provider(tls::provider())
        .with_protocol_versions(TLS13)?
        .with_client_cert_verifier(Arc::new(PinnedClientVerifier::new(policy)))
        .with_single_cert(vec![identity.cert()], identity.key())?;
    crypto.alpn_protocols = vec![crate::ALPN.to_vec()];

    let mut server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    server.transport_config(Arc::new(transport(Some(SERVER_KEEPALIVE))));

    let socket = UdpSocket::bind(SocketAddr::from(([0u16; 8], port)))
        .or_else(|_| UdpSocket::bind(SocketAddr::from(([0u8; 4], port))))?;
    let config = quinn::EndpointConfig::new(Arc::new(identity.reset_key()));
    Ok(Endpoint::new(config, Some(server), socket, Arc::new(quinn::TokioRuntime))?)
}

pub fn client_endpoint() -> Result<Endpoint> {
    Ok(Endpoint::client(SocketAddr::from(([0u8; 4], 0)))?)
}

/// Конфигурация клиента, которая доверяет только серверу с отпечатком `expected`.
pub fn client_config(identity: &Identity, expected: Fingerprint) -> Result<quinn::ClientConfig> {
    let mut crypto = rustls::ClientConfig::builder_with_provider(tls::provider())
        .with_protocol_versions(TLS13)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier::new(expected)))
        .with_client_auth_cert(vec![identity.cert()], identity.key())?;
    crypto.alpn_protocols = vec![crate::ALPN.to_vec()];

    let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    client.transport_config(Arc::new(transport(None)));
    Ok(client)
}
