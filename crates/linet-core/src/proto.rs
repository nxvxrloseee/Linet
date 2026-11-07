use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// Обёртка любого сообщения управляющего потока.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
    pub body: Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Laptop,
    Desktop,
    Phone,
    Tablet,
    #[serde(other)]
    Unknown,
}

impl DeviceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Laptop => "laptop",
            Self::Desktop => "desktop",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub kind: DeviceKind,
    pub proto: u16,
    /// Поддерживаемые фичи: "ping", "files", "notifications", ...
    #[serde(default)]
    pub caps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub key: String,
    /// Имя пакета приложения: по нему ноутбук кеширует иконку.
    pub package: String,
    pub app: String,
    pub title: String,
    pub text: String,
    /// PNG иконки приложения. Телефон шлёт его один раз за сеанс на пакет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ByteBuf>,
    #[serde(default)]
    pub actions: Vec<NotificationButton>,
    /// Можно ли смахнуть (dismiss) с ноутбука.
    #[serde(default)]
    pub clearable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationButton {
    pub index: u32,
    pub title: String,
    /// Кнопка ожидает текст (ответ в мессенджере).
    #[serde(default)]
    pub reply: bool,
}

/// Новые варианты добавляются в конец. Неизвестный тег у старой версии
/// превращается в `Unknown`, и соединение не рвётся.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    PairRequest { hello: Hello, proof: ByteBuf },
    PairAccept(Hello),
    PairReject { reason: String },
    Ping,
    Pong,
    /// Предложение файла. Данные идут отдельным однонаправленным потоком:
    /// `u64 big-endian id ‖ содержимое`.
    FileOffer {
        id: u64,
        name: String,
        size: u64,
        mime: String,
        /// SHA-256 содержимого; без него докачка отключена.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        /// Назначение: `None` — обычный файл, `"app-update"` — обновление приложения.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<String>,
    },
    /// `offset` > 0 — у получателя уже есть начало файла, слать с этого байта.
    FileAccept {
        id: u64,
        #[serde(default)]
        offset: u64,
        /// Подтверждает поддержку проверки содержимого при докачке.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    FileReject { id: u64, reason: String },
    /// Уведомление телефона (новое или обновлённое — по тому же `key`).
    Notification(Notification),
    /// Уведомление убрано на телефоне.
    NotificationRemoved { key: String },
    /// Ноутбук → телефон: нажата кнопка (`reply` — текст ответа).
    NotificationAction { key: String, action: u32, reply: Option<String> },
    /// Ноутбук → телефон: пользователь закрыл уведомление на ноутбуке.
    NotificationDismiss { key: String },
    /// Новое содержимое буфера обмена (только текст).
    Clipboard { text: String },
    /// «Продолжить на другом устройстве»: открыть страницу.
    Handoff { url: String, title: String },
    /// Ноутбук → телефон: можно ли разблокировать экран? Ответ — `UnlockResponse`.
    UnlockRequest { host: String },
    UnlockResponse { approved: bool, reason: String },
    /// Телефон → ноутбук: заряд батареи (при подключении и при изменении).
    Battery { level: u8, charging: bool },
    /// Картинка в буфере обмена (PNG/JPEG, до `MAX_CLIPBOARD_IMAGE`).
    ClipboardImage { mime: String, data: ByteBuf },
    /// Ноутбук → телефон: включить/выключить беспроводную отладку (для scrcpy).
    /// Ответ — `ScreenShareResult`.
    ScreenShare { enable: bool },
    ScreenShareResult { ok: bool, error: String },
    /// Телефон → ноутбук: показать экран ноутбука по VNC. `extend` — второй монитор
    /// размером `width`×`height` (пиксели экрана телефона в альбомной ориентации).
    VncRequest { extend: bool, width: u32, height: u32 },
    /// Ответ: ссылка `vnc://…` с одноразовым паролем или ошибка.
    VncResult { url: String, error: String },
    /// Телефон → ноутбук: закончить показ экрана.
    VncStop,
    /// Получатель проверил SHA-256 и сохранил файл (None), либо сообщает ошибку.
    FileResult { id: u64, error: Option<String> },
    #[serde(other)]
    Unknown,
}

/// Самая большая картинка буфера обмена: идёт одним сообщением управляющего потока.
pub const MAX_CLIPBOARD_IMAGE: usize = 12 << 20;

pub fn encode(env: &Envelope) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    ciborium::into_writer(env, &mut buf)?;
    Ok(buf)
}

/// Тело разбирается отдельно: сообщение неизвестного типа (или с
/// несовместимым содержимым) становится `Message::Unknown`, а не ошибкой.
pub fn decode(bytes: &[u8]) -> anyhow::Result<Envelope> {
    #[derive(Deserialize)]
    struct Raw {
        id: u64,
        #[serde(default)]
        reply_to: Option<u64>,
        body: ciborium::Value,
    }
    let raw: Raw = ciborium::from_reader(bytes)?;
    let body = raw.body.deserialized().unwrap_or(Message::Unknown);
    Ok(Envelope { id: raw.id, reply_to: raw.reply_to, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let env = Envelope {
            id: 7,
            reply_to: Some(3),
            body: Message::Hello(Hello {
                name: "pc".into(),
                kind: DeviceKind::Laptop,
                proto: 1,
                caps: vec!["ping".into()],
            }),
        };
        let back = decode(&encode(&env).unwrap()).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.reply_to, Some(3));
        assert!(matches!(back.body, Message::Hello(h) if h.name == "pc"));
    }

    #[test]
    fn unknown_message_is_tolerated() {
        #[derive(Serialize)]
        #[serde(tag = "t", content = "c")]
        enum Future {
            #[serde(rename = "teleport")]
            Teleport { to: String },
        }
        #[derive(Serialize)]
        struct Env {
            id: u64,
            body: Future,
        }
        let mut buf = Vec::new();
        ciborium::into_writer(&Env { id: 1, body: Future::Teleport { to: "mars".into() } }, &mut buf)
            .unwrap();
        let env = decode(&buf).unwrap();
        assert!(matches!(env.body, Message::Unknown));
    }

    #[test]
    fn legacy_file_messages_decode_without_hash() {
        for (tag, content) in [
            ("file_offer", serde_json::json!({"id": 3, "name": "a", "size": 4, "mime": "text/plain"})),
            ("file_accept", serde_json::json!({"id": 3, "offset": 2})),
        ] {
            let legacy = serde_json::json!({"id": 1, "body": {"t": tag, "c": content}});
            let mut bytes = Vec::new();
            ciborium::into_writer(&legacy, &mut bytes).unwrap();
            let body = decode(&bytes).unwrap().body;
            assert!(matches!(body, Message::FileOffer { sha256: None, .. } | Message::FileAccept { sha256: None, .. }));
        }
    }
}
