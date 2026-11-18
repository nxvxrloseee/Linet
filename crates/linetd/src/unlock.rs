//! Ryoku/qylock lock adapter. No unauthenticated unlock path.
use std::sync::Arc;
use std::time::Duration;
use anyhow::{Result, bail};
use tokio::process::Command;
use crate::daemon::Daemon;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState { Locked, Unlocked, Unknown }

impl LockState {
    pub fn as_str(self) -> &'static str {
        match self { Self::Locked => "locked", Self::Unlocked => "unlocked", Self::Unknown => "unknown" }
    }
}

fn parse_status(success: bool, stdout: &[u8]) -> LockState {
    if !success { return LockState::Unknown; }
    match std::str::from_utf8(stdout).unwrap_or("").trim() {
        "locked" => LockState::Locked,
        "unlocked" => LockState::Unlocked,
        _ => LockState::Unknown,
    }
}

pub async fn state() -> LockState {
    let result = tokio::time::timeout(Duration::from_secs(3), Command::new("ryoku-shell")
        .args(["lock", "status"]).kill_on_drop(true).output()).await;
    match result { Ok(Ok(out)) => parse_status(out.status.success(), &out.stdout), _ => LockState::Unknown }
}

pub async fn status_json() -> String {
    let state = state().await;
    serde_json::json!({"schema_version":1,"backend":"ryoku", "state":state.as_str(),
        "can_lock":state != LockState::Unknown,"authenticated_unlock":false}).to_string()
}

pub async fn lock() -> Result<()> {
    match state().await {
        LockState::Locked => return Ok(()),
        LockState::Unknown => bail!("Ryoku: состояние блокировки недоступно; запрос не отправлен"),
        LockState::Unlocked => {},
    }
    let output = tokio::time::timeout(Duration::from_secs(5), Command::new("ryoku-shell")
        .arg("lock").kill_on_drop(true).output()).await??;
    if !output.status.success() { bail!("Ryoku не принял запрос блокировки"); }
    Ok(())
}

pub struct Unlocker;
impl Unlocker {
    pub fn new(_daemon: Arc<Daemon>) -> Arc<Self> { Arc::new(Self) }
    pub async fn attempt(&self, _why: &str) -> Result<bool> {
        bail!("Ryoku/qylock: authenticated unlock телефоном пока не поддерживается; используйте пароль или настроенную PAM-аутентификацию")
    }
    // No wake/lid requests: a phone approval is not a qylock/PAM authentication.
    pub fn watch(self: Arc<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errors_and_unknown_output_are_not_unlocked() {
        assert_eq!(parse_status(false, b"unlocked"), LockState::Unknown);
        for output in [b"".as_slice(), b"error", b"false", b"locked?", b"\xff"] {
            assert_eq!(parse_status(true, output), LockState::Unknown);
        }
        assert_eq!(parse_status(true, b"locked\n"), LockState::Locked);
        assert_eq!(parse_status(true, b"unlocked\n"), LockState::Unlocked);
    }
    #[tokio::test]
    async fn unsupported_unlock_always_fails() {
        assert!(Unlocker.attempt("test").await.unwrap_err().to_string().contains("пока не поддерживается"));
    }
}
