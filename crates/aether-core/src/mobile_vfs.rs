//! Remote file browsing without a native mount (mobile hosts cannot mount NFS).
//!
//! The mobile app browses a peer's shares through the same pipelined VFS RPC the desktop
//! mount uses, and downloads single files into its sandbox.

use crate::client::ClientSession;
use crate::{CoreError, Result};
use aether_protocol::FileKind;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// One directory entry, flattened for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    pub read_only: bool,
}

fn vfs_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(e.to_string())
}

fn join(dir: &str, name: &str) -> String {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() { format!("/{name}") } else { format!("{dir}/{name}") }
}

impl ClientSession {
    /// List `path` (share-relative, `/` = root) of remote share `share`; directories first.
    pub async fn vfs_read_dir(&self, share: u32, path: &str) -> Result<Vec<RemoteEntry>> {
        let client = self.vfs_client().await?;
        let mut out: Vec<RemoteEntry> = client
            .read_dir(share, path)
            .await
            .map_err(vfs_err)?
            .into_iter()
            .map(|e| RemoteEntry {
                path: join(path, &e.name),
                is_dir: e.attr.kind == FileKind::Directory,
                size: e.attr.size,
                modified_ms: (e.attr.mtime_ns / 1_000_000) as i64,
                read_only: e.attr.read_only,
                name: e.name,
            })
            .collect();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    /// Download one remote file into `dest_dir` (BLAKE3-verified chunk cache underneath).
    pub async fn vfs_download(&self, share: u32, path: &str, dest_dir: &Path) -> Result<PathBuf> {
        let client = self.vfs_client().await?;
        let name = path.rsplit('/').find(|s| !s.is_empty()).ok_or_else(|| CoreError::Invalid("errors.invalid".into()))?;
        if name == "." || name == ".." {
            return Err(CoreError::Invalid("errors.invalid".into()));
        }
        let data = client.read_all(share, path).await.map_err(vfs_err)?;
        std::fs::create_dir_all(dest_dir)?;
        let dest = dest_dir.join(name);
        tokio::fs::write(&dest, data).await?;
        Ok(dest)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn joins_paths() {
        assert_eq!(super::join("/", "a"), "/a");
        assert_eq!(super::join("", "a"), "/a");
        assert_eq!(super::join("/x/", "a"), "/x/a");
    }
}
