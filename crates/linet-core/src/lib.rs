//! Ядро Linet: протокол, идентичность, TLS с закреплёнными сертификатами,
//! транспорт поверх QUIC. Используется и демоном на Linux, и Android-приложением.

pub mod client;
pub mod files;
pub mod frame;
pub mod hint;
pub mod identity;
pub mod pairing;
pub mod proto;
pub mod session;
pub mod store;
pub mod tls;
pub mod transport;

pub use identity::{Fingerprint, Identity};
pub use proto::{DeviceKind, Envelope, Hello, Message, Notification, NotificationButton};
pub use session::{Inbound, Session};
pub use store::{PeerRecord, PeerStore};

/// UDP-порт демона по умолчанию.
pub const DEFAULT_PORT: u16 = 47321;
/// Версия протокола, передаётся в `Hello`.
pub const PROTO_VERSION: u16 = 1;
pub const ALPN: &[u8] = b"linet/1";
/// Имя для SNI. Реальная проверка сервера — по отпечатку сертификата.
pub const SERVER_NAME: &str = "linet";
