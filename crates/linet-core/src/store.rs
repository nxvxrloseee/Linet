//! Список сопряжённых устройств в JSON-файле.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity::Fingerprint;
use crate::proto::DeviceKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub kind: DeviceKind,
    /// Последние известные адреса (для клиента — адреса сервера).
    #[serde(default)]
    pub addrs: Vec<String>,
    pub paired_at: u64,
}

impl PeerRecord {
    pub fn id(&self) -> String {
        self.fingerprint.device_id()
    }
}

pub struct PeerStore {
    path: PathBuf,
    peers: Vec<PeerRecord>,
}

impl PeerStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let peers = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("разбор {path:?}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("чтение {path:?}")),
        };
        Ok(Self { path, peers })
    }

    pub fn peers(&self) -> &[PeerRecord] {
        &self.peers
    }

    pub fn get(&self, fp: &Fingerprint) -> Option<&PeerRecord> {
        self.peers.iter().find(|p| &p.fingerprint == fp)
    }

    /// Поиск по короткому id или по его префиксу.
    pub fn find(&self, id: &str) -> Option<&PeerRecord> {
        let mut it = self.peers.iter().filter(|p| p.id().starts_with(id));
        match (it.next(), it.next()) {
            (Some(p), None) => Some(p),
            _ => None,
        }
    }

    pub fn contains(&self, fp: &Fingerprint) -> bool {
        self.get(fp).is_some()
    }

    pub fn upsert(&mut self, rec: PeerRecord) -> Result<()> {
        self.peers.retain(|p| p.fingerprint != rec.fingerprint);
        self.peers.push(rec);
        self.save()
    }

    pub fn update(&mut self, fp: &Fingerprint, f: impl FnOnce(&mut PeerRecord)) -> Result<()> {
        if let Some(p) = self.peers.iter_mut().find(|p| &p.fingerprint == fp) {
            f(p);
            self.save()?;
        }
        Ok(())
    }

    pub fn remove(&mut self, fp: &Fingerprint) -> Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| &p.fingerprint != fp);
        let removed = self.peers.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(&self.path, &serde_json::to_vec_pretty(&self.peers)?)
    }
}

/// Атомарная запись файла, читаемого только владельцем.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("запись {tmp:?}"))?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
