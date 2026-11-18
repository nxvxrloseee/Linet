//! Фрейминг управляющего потока: `u32 big-endian длина ‖ CBOR(Envelope)`.

use anyhow::{Result, bail};
use quinn::{ReadExactError, RecvStream, SendStream};

use crate::proto::{self, Envelope};

/// Защита от мусора и злонамеренных пиров. Файлы идут отдельными потоками,
/// но картинка буфера обмена — одним сообщением, отсюда запас.
pub const MAX_FRAME: usize = crate::proto::MAX_CLIPBOARD_IMAGE + (64 << 10);
pub const MAX_CONTROL_FRAME: usize = 1 << 20;

fn validate(env: &Envelope, encoded_len: usize) -> Result<()> {
    match &env.body {
        proto::Message::ClipboardImage { data, .. } => {
            if data.len() > proto::MAX_CLIPBOARD_IMAGE { bail!("картинка больше лимита"); }
        }
        _ if encoded_len > MAX_CONTROL_FRAME => bail!("управляющее сообщение больше лимита"),
        _ => {}
    }
    Ok(())
}

pub async fn write(stream: &mut SendStream, env: &Envelope) -> Result<()> {
    let body = proto::encode(env)?;
    validate(env, body.len())?;
    if body.len() > MAX_FRAME {
        bail!("сообщение слишком большое: {} байт", body.len());
    }
    // Две записи вместо склейки: не копируем крупное тело (картинку) ещё раз.
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// `Ok(None)` — пир аккуратно закрыл поток.
pub async fn read(stream: &mut RecvStream) -> Result<Option<Envelope>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("фрейм слишком большой: {len} байт");
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    let env = proto::decode(&body)?;
    validate(&env, len)?;
    Ok(Some(env))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_image_and_control_limits() {
        let mut env = Envelope { id: 1, reply_to: None, body: proto::Message::Ping };
        assert!(validate(&env, MAX_CONTROL_FRAME + 1).is_err());
        env.body = proto::Message::ClipboardImage { mime: "image/png".into(), data: vec![0; proto::MAX_CLIPBOARD_IMAGE].into() };
        assert!(validate(&env, MAX_FRAME).is_ok());
        if let proto::Message::ClipboardImage { data, .. } = &mut env.body { data.push(0); }
        assert!(validate(&env, MAX_FRAME).is_err());
    }
}
