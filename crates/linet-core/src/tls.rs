//! TLS без центров сертификации: доверие строится на закреплённых отпечатках.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};

use crate::identity::Fingerprint;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn algorithms() -> WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

/// Решение, пускать ли клиента с данным отпечатком.
/// Реализует демон: «сопряжён» или «сейчас открыто окно сопряжения».
pub trait ClientPolicy: Send + Sync + std::fmt::Debug {
    fn allow(&self, fp: &Fingerprint) -> bool;
}

#[derive(Debug)]
pub struct PinnedClientVerifier {
    policy: Arc<dyn ClientPolicy>,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedClientVerifier {
    pub fn new(policy: Arc<dyn ClientPolicy>) -> Self {
        Self { policy, algs: algorithms() }
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        if self.policy.allow(&Fingerprint::of_cert(end_entity)) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::General("неизвестное устройство".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 не поддерживается".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Клиент принимает сервер, только если его отпечаток совпадает с ожидаемым
/// (из QR-кода при сопряжении или из сохранённой записи).
#[derive(Debug)]
pub struct PinnedServerVerifier {
    expected: Fingerprint,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedServerVerifier {
    pub fn new(expected: Fingerprint) -> Self {
        Self { expected, algs: algorithms() }
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if Fingerprint::of_cert(end_entity) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General("отпечаток сервера не совпадает".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 не поддерживается".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// DER-сертификат пира из установленного QUIC-соединения.
pub fn peer_cert(conn: &quinn::Connection) -> Option<CertificateDer<'static>> {
    let certs = conn.peer_identity()?.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().cloned()
}

/// Отпечаток пира из установленного QUIC-соединения.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    peer_cert(conn).map(|c| Fingerprint::of_cert(&c))
}
