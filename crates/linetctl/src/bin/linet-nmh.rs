//! Native messaging host для браузерного расширения Linet.
//!
//! Браузер запускает процесс на каждое сообщение: на stdin приходит
//! `u32 (native-endian) длина ‖ JSON`, ответ пишется в том же формате.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use linetctl::DaemonProxy;
use serde_json::{Value, json};

const MAX_MESSAGE: usize = 64 * 1024;

fn read_message(input: &mut impl Read) -> Result<Option<Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_MESSAGE {
        bail!("сообщение слишком большое: {len}");
    }
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

fn write_message(output: &mut impl Write, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    output.write_all(&(body.len() as u32).to_ne_bytes())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

async fn handle(daemon: &DaemonProxy<'_>, msg: &Value) -> Result<()> {
    match msg["type"].as_str() {
        Some("open") => {
            let url = msg["url"].as_str().context("нет url")?;
            let title = msg["title"].as_str().unwrap_or("");
            daemon.send_url("", url, title).await?;
            Ok(())
        }
        other => bail!("неизвестный тип сообщения: {other:?}"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let daemon = DaemonProxy::new(&conn).await?;
    let (mut stdin, mut stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    while let Some(msg) = read_message(&mut stdin)? {
        let reply = match handle(&daemon, &msg).await {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
        };
        write_message(&mut stdout, &reply)?;
    }
    Ok(())
}
