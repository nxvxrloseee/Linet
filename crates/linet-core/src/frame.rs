//! Фрейминг управляющего потока: `u32 big-endian длина ‖ CBOR(Envelope)`.

use anyhow::{Result, bail};
use quinn::{ReadExactError, RecvStream, SendStream};

use crate::proto::{self, Envelope};

/// Защита от мусора и злонамеренных пиров. Файлы идут отдельными потоками,
/// но картинка буфера обмена — одним сообщением, отсюда запас.
pub const MAX_FRAME: usize = 16 << 20;

pub async fn write(stream: &mut SendStream, env: &Envelope) -> Result<()> {
    let body = proto::encode(env)?;
    if body.len() > MAX_FRAME {
        bail!("сообщение слишком большое: {} байт", body.len());
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    stream.write_all(&buf).await?;
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
    Ok(Some(proto::decode(&body)?))
}
