use anyhow::Result;
use futures_util::StreamExt;
use linetctl::DaemonProxy;
use std::io::{self, Write};
use std::time::Duration;

pub async fn once(daemon: &DaemonProxy<'_>) -> Result<()> {
    let json = tokio::time::timeout(Duration::from_secs(2), daemon.snapshot_json()).await
        .ok().and_then(Result::ok).unwrap_or_else(|| {
        serde_json::json!({"schema_version":1,"available":false,"devices":[],"transfers":[]}).to_string()
    });
    println!("{json}");
    io::stdout().flush()?;
    Ok(())
}

pub async fn watch(daemon: &DaemonProxy<'_>) -> Result<()> {
    // These well-known-name subscriptions follow NameOwnerChanged in zbus.
    let mut owners = daemon.inner().receive_owner_changed().await?;
    let mut devices = daemon.receive_devices_changed().await?;
    let mut progress = daemon.receive_transfer_progress().await?;
    let mut finished = daemon.receive_transfer_finished().await?;
    once(daemon).await?;
    let mut fallback = tokio::time::interval(Duration::from_secs(30));
    fallback.tick().await;
    loop {
        tokio::select! {
            owner = owners.next() => if owner.is_none() { break; },
            event = devices.next() => if event.is_none() { break; },
            event = progress.next() => if event.is_none() { break; },
            event = finished.next() => if event.is_none() { break; },
            _ = fallback.tick() => {},
        }
        once(daemon).await?;
    }
    Ok(())
}
