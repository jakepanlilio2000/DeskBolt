use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use deskbolt_proto::{FileEntry, SyncEntry};
use ring::digest::{Context, SHA256};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::terminal::{SshConnectError, SshConnectParams, connect_ssh2_session};

const LOCAL_SCAN_MAX_ENTRIES: usize = 10_000;
const LOCAL_SCAN_MAX_DEPTH: usize = 32;
const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    MirrorUpload,
    MirrorDownload,
    TwoWayNewest,
}

impl SyncMode {
    pub const ALL: [Self; 3] = [Self::MirrorUpload, Self::MirrorDownload, Self::TwoWayNewest];

    pub const fn label(self) -> &'static str {
        match self {
            Self::MirrorUpload => "Mirror Local -> Remote",
            Self::MirrorDownload => "Mirror Remote -> Local",
            Self::TwoWayNewest => "Two-Way (Newest Wins)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    OnlyLocal,
    OnlyRemote,
    ChangedLocalNewer,
    ChangedRemoteNewer,
    Identical,
}

impl DiffStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OnlyLocal => "Only Local",
            Self::OnlyRemote => "Only Remote",
            Self::ChangedLocalNewer => "Modified (Local Newer)",
            Self::ChangedRemoteNewer => "Modified (Remote Newer)",
            Self::Identical => "Identical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub status: DiffStatus,
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    EnsureLocalDir { rel_path: String },
    EnsureRemoteDir { rel_path: String },
    Upload { rel_path: String, size: u64 },
    Download { rel_path: String, size: u64 },
}

/// Scans a local directory tree cleanly using `ring::digest::SHA256` for differential sync.
pub async fn scan_local_hash_tree(root: &Path) -> Result<Vec<SyncEntry>, String> {
    if !root.exists() {
        return Err(format!("local path does not exist: {}", root.display()));
    }
    let mut entries = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), String::new(), 0usize)]);

    while let Some((dir, prefix, depth)) = queue.pop_front() {
        let mut rd = fs::read_dir(&dir)
            .await
            .map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        while let Ok(Some(item)) = rd.next_entry().await {
            if entries.len() >= LOCAL_SCAN_MAX_ENTRIES {
                break;
            }
            let item_path = item.path();
            let Ok(meta) = fs::symlink_metadata(&item_path).await else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let rel_path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if meta.is_dir() {
                entries.push(SyncEntry {
                    rel_path: rel_path.clone(),
                    is_dir: true,
                    size: 0,
                    modified_unix,
                    sha256: None,
                });
                if depth < LOCAL_SCAN_MAX_DEPTH {
                    queue.push_back((item_path, rel_path, depth + 1));
                }
            } else if meta.is_file() {
                let hash = sha256_local_file(&item_path).await.ok();
                entries.push(SyncEntry {
                    rel_path,
                    is_dir: false,
                    size: meta.len(),
                    modified_unix,
                    sha256: hash,
                });
            }
        }
    }

    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(entries)
}

async fn sha256_local_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = File::open(path).await.map_err(|e| e.to_string())?;
    let mut ctx = Context::new(&SHA256);
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    let d = ctx.finish();
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    Ok(out)
}

/// Classifies differences between local and remote directory trees by size, mtime, and SHA-256 checksum.
pub fn compute_tree_diff(local: &[SyncEntry], remote: &[SyncEntry]) -> Vec<DiffEntry> {
    let l_map: HashMap<&str, &SyncEntry> =
        local.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let r_map: HashMap<&str, &SyncEntry> =
        remote.iter().map(|e| (e.rel_path.as_str(), e)).collect();

    let mut all_paths: Vec<&str> = l_map
        .keys()
        .chain(r_map.keys())
        .copied()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    all_paths.sort_unstable();

    let mut out = Vec::with_capacity(all_paths.len());
    for rel in all_paths {
        match (l_map.get(rel), r_map.get(rel)) {
            (Some(l), None) => out.push(DiffEntry {
                rel_path: rel.to_string(),
                is_dir: l.is_dir,
                status: DiffStatus::OnlyLocal,
                local_size: (!l.is_dir).then_some(l.size),
                remote_size: None,
            }),
            (None, Some(r)) => out.push(DiffEntry {
                rel_path: rel.to_string(),
                is_dir: r.is_dir,
                status: DiffStatus::OnlyRemote,
                local_size: None,
                remote_size: (!r.is_dir).then_some(r.size),
            }),
            (Some(l), Some(r)) => {
                let identical = if l.is_dir && r.is_dir {
                    true
                } else if l.is_dir != r.is_dir || l.size != r.size {
                    false
                } else if let (Some(lh), Some(rh)) = (l.sha256, r.sha256) {
                    lh == rh
                } else {
                    l.modified_unix == r.modified_unix
                };

                let status = if identical {
                    DiffStatus::Identical
                } else if l.modified_unix >= r.modified_unix {
                    DiffStatus::ChangedLocalNewer
                } else {
                    DiffStatus::ChangedRemoteNewer
                };
                out.push(DiffEntry {
                    rel_path: rel.to_string(),
                    is_dir: l.is_dir,
                    status,
                    local_size: (!l.is_dir).then_some(l.size),
                    remote_size: (!r.is_dir).then_some(r.size),
                });
            }
            (None, None) => {}
        }
    }
    out
}

/// Converts a diff list into ordered transfer actions for `SyncMode`.
pub fn plan_sync_actions(diffs: &[DiffEntry], mode: SyncMode) -> Vec<SyncAction> {
    let mut actions = Vec::new();
    for d in diffs {
        if d.status == DiffStatus::Identical {
            continue;
        }
        let upload = matches!(
            (mode, d.status),
            (
                SyncMode::MirrorUpload,
                DiffStatus::OnlyLocal | DiffStatus::ChangedLocalNewer | DiffStatus::ChangedRemoteNewer
            ) | (SyncMode::TwoWayNewest, DiffStatus::OnlyLocal | DiffStatus::ChangedLocalNewer)
        );
        let download = matches!(
            (mode, d.status),
            (
                SyncMode::MirrorDownload,
                DiffStatus::OnlyRemote | DiffStatus::ChangedLocalNewer | DiffStatus::ChangedRemoteNewer
            ) | (SyncMode::TwoWayNewest, DiffStatus::OnlyRemote | DiffStatus::ChangedRemoteNewer)
        );

        if d.is_dir {
            if upload {
                actions.push(SyncAction::EnsureRemoteDir {
                    rel_path: d.rel_path.clone(),
                });
            } else if download {
                actions.push(SyncAction::EnsureLocalDir {
                    rel_path: d.rel_path.clone(),
                });
            }
        } else if upload {
            actions.push(SyncAction::Upload {
                rel_path: d.rel_path.clone(),
                size: d.local_size.unwrap_or(0),
            });
        } else if download {
            actions.push(SyncAction::Download {
                rel_path: d.rel_path.clone(),
                size: d.remote_size.unwrap_or(0),
            });
        }
    }
    actions
}

/// Pure-Rust SFTP client (`russh` + `russh-sftp`) sharing directory/sync/tail/edit capabilities.
pub struct SftpClient {
    _ssh: russh::client::Handle<crate::terminal::TofuSshHandler>,
    sftp: SftpSession,
}

impl SftpClient {
    pub async fn connect(params: &SshConnectParams) -> Result<(Self, String), SshConnectError> {
        let (handle, fp) = connect_ssh2_session(params).await?;
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshConnectError::Failed(format!("open SFTP SSH channel: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| SshConnectError::Failed(format!("request SFTP subsystem: {e}")))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SshConnectError::Failed(format!("init SFTP session: {e}")))?;
        Ok((
            Self {
                _ssh: handle,
                sftp,
            },
            fp,
        ))
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        let dir = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|e| format!("SFTP read_dir '{path}': {e}"))?;
        let mut out = Vec::new();
        for entry in dir {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let meta = entry.metadata();
            out.push(FileEntry {
                name,
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                modified_unix: meta.mtime.map(|m| m as i64).unwrap_or(0),
                readonly: false,
            });
        }
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    pub async fn mkdir(&self, path: &str) -> Result<(), String> {
        self.sftp
            .create_dir(path)
            .await
            .map_err(|e| format!("SFTP mkdir '{path}': {e}"))
    }

    pub async fn delete(&self, path: &str, is_dir: bool) -> Result<(), String> {
        if is_dir {
            self.sftp
                .remove_dir(path)
                .await
                .map_err(|e| format!("SFTP rmdir '{path}': {e}"))
        } else {
            self.sftp
                .remove_file(path)
                .await
                .map_err(|e| format!("SFTP remove '{path}': {e}"))
        }
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.sftp
            .rename(from, to)
            .await
            .map_err(|e| format!("SFTP rename failed: {e}"))
    }

    pub async fn upload_file(&self, local: &Path, remote: &str) -> Result<u64, String> {
        let mut src = File::open(local)
            .await
            .map_err(|e| format!("open local {}: {e}", local.display()))?;
        let mut dst = self
            .sftp
            .create(remote)
            .await
            .map_err(|e| format!("SFTP create '{remote}': {e}"))?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut total = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .await
                .map_err(|e| format!("read local: {e}"))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .await
                .map_err(|e| format!("SFTP write: {e}"))?;
            total += n as u64;
        }
        dst.shutdown()
            .await
            .map_err(|e| format!("SFTP flush: {e}"))?;
        Ok(total)
    }

    pub async fn download_file(&self, remote: &str, local: &Path) -> Result<u64, String> {
        let mut src = self
            .sftp
            .open(remote)
            .await
            .map_err(|e| format!("SFTP open '{remote}': {e}"))?;
        if let Some(parent) = local.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("create parent dir: {e}"))?;
        }
        let part = PathBuf::from(format!("{}.deskbolt-part", local.display()));
        let mut dst = File::create(&part)
            .await
            .map_err(|e| format!("create {}: {e}", part.display()))?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut total = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .await
                .map_err(|e| format!("SFTP read: {e}"))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .await
                .map_err(|e| format!("write local: {e}"))?;
            total += n as u64;
        }
        dst.flush().await.map_err(|e| format!("flush local: {e}"))?;
        drop(dst);
        fs::rename(&part, local)
            .await
            .map_err(|e| format!("finalize download: {e}"))?;
        Ok(total)
    }

    pub async fn read_text_limited(&self, remote: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
        let mut file = self
            .sftp
            .open(remote)
            .await
            .map_err(|e| format!("SFTP open '{remote}': {e}"))?;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| format!("SFTP read '{remote}': {e}"))?;
            if n == 0 {
                break;
            }
            if out.len() + n > max_bytes {
                return Err(format!("Remote file exceeds quick-edit cap ({max_bytes} bytes)."));
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    pub async fn write_all_bytes(&self, remote: &str, bytes: &[u8]) -> Result<(), String> {
        let mut file = self
            .sftp
            .open_with_flags(
                remote,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .await
            .map_err(|e| format!("SFTP open write '{remote}': {e}"))?;
        file.write_all(bytes)
            .await
            .map_err(|e| format!("SFTP write '{remote}': {e}"))?;
        file.shutdown()
            .await
            .map_err(|e| format!("SFTP close '{remote}': {e}"))?;
        Ok(())
    }

    pub async fn tail_chunk(
        &self,
        remote: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(u64, Vec<u8>, bool), String> {
        let mut file = self
            .sftp
            .open(remote)
            .await
            .map_err(|e| format!("SFTP open '{remote}': {e}"))?;
        let len = file
            .metadata()
            .await
            .map_err(|e| format!("SFTP stat '{remote}': {e}"))?
            .len();
        let cap = max_bytes.clamp(1, 256 * 1024);
        let (read_from, truncated) = if offset > len {
            (len.saturating_sub(cap as u64), true)
        } else {
            (offset, false)
        };
        file.seek(std::io::SeekFrom::Start(read_from))
            .await
            .map_err(|e| format!("SFTP seek '{remote}': {e}"))?;
        let mut data = Vec::new();
        let mut buf = [0u8; 8192];
        while data.len() < cap {
            let want = (cap - data.len()).min(buf.len());
            let n = file
                .read(&mut buf[..want])
                .await
                .map_err(|e| format!("SFTP tail read: {e}"))?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
        }
        Ok((read_from + data.len() as u64, data, truncated))
    }

    pub async fn hash_tree(&self, root: &str) -> Result<Vec<SyncEntry>, String> {
        let mut entries = Vec::new();
        let base = root.trim_end_matches('/');
        let mut queue = VecDeque::from([(base.to_string(), String::new(), 0usize)]);

        while let Some((dir, prefix, depth)) = queue.pop_front() {
            let items = self.list_dir(&dir).await?;
            for item in items {
                if entries.len() >= LOCAL_SCAN_MAX_ENTRIES {
                    break;
                }
                let full = if dir.is_empty() || dir == "/" {
                    format!("/{}", item.name)
                } else {
                    format!("{dir}/{}", item.name)
                };
                let rel_path = if prefix.is_empty() {
                    item.name.clone()
                } else {
                    format!("{prefix}/{}", item.name)
                };
                if item.is_dir {
                    entries.push(SyncEntry {
                        rel_path: rel_path.clone(),
                        is_dir: true,
                        size: 0,
                        modified_unix: item.modified_unix,
                        sha256: None,
                    });
                    if depth < LOCAL_SCAN_MAX_DEPTH {
                        queue.push_back((full, rel_path, depth + 1));
                    }
                } else {
                    let sha256 = self.hash_remote_file(&full).await.ok();
                    entries.push(SyncEntry {
                        rel_path,
                        is_dir: false,
                        size: item.size,
                        modified_unix: item.modified_unix,
                        sha256,
                    });
                }
            }
        }
        entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(entries)
    }

    async fn hash_remote_file(&self, remote: &str) -> Result<[u8; 32], String> {
        let mut f = self.sftp.open(remote).await.map_err(|e| e.to_string())?;
        let mut ctx = Context::new(&SHA256);
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let n = f.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            ctx.update(&buf[..n]);
        }
        let d = ctx.finish();
        let mut out = [0u8; 32];
        out.copy_from_slice(d.as_ref());
        Ok(out)
    }
}

/// Checks whether a filename extension belongs to editable config/text formats.
pub fn is_quick_edit_ext(name: &str) -> bool {
    let low = name.to_ascii_lowercase();
    [".toml", ".ini", ".json", ".conf", ".xml", ".yaml", ".yml", ".txt"]
        .iter()
        .any(|ext| low.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_diff_and_sync_plan_classify_changes_accurately() {
        let h_a = [0x11u8; 32];
        let h_b = [0x22u8; 32];
        let local = vec![
            SyncEntry {
                rel_path: "same.txt".into(),
                is_dir: false,
                size: 10,
                modified_unix: 100,
                sha256: Some(h_a),
            },
            SyncEntry {
                rel_path: "local_only.toml".into(),
                is_dir: false,
                size: 20,
                modified_unix: 150,
                sha256: Some(h_a),
            },
            SyncEntry {
                rel_path: "edited.conf".into(),
                is_dir: false,
                size: 30,
                modified_unix: 200,
                sha256: Some(h_a),
            },
        ];
        let remote = vec![
            SyncEntry {
                rel_path: "same.txt".into(),
                is_dir: false,
                size: 10,
                modified_unix: 100,
                sha256: Some(h_a),
            },
            SyncEntry {
                rel_path: "remote_only.log".into(),
                is_dir: false,
                size: 40,
                modified_unix: 180,
                sha256: Some(h_b),
            },
            SyncEntry {
                rel_path: "edited.conf".into(),
                is_dir: false,
                size: 30,
                modified_unix: 120,
                sha256: Some(h_b), // same size, different SHA-256!
            },
        ];

        let diffs = compute_tree_diff(&local, &remote);
        assert_eq!(
            diffs.iter().find(|d| d.rel_path == "same.txt").unwrap().status,
            DiffStatus::Identical
        );
        assert_eq!(
            diffs.iter().find(|d| d.rel_path == "local_only.toml").unwrap().status,
            DiffStatus::OnlyLocal
        );
        assert_eq!(
            diffs.iter().find(|d| d.rel_path == "remote_only.log").unwrap().status,
            DiffStatus::OnlyRemote
        );
        assert_eq!(
            diffs.iter().find(|d| d.rel_path == "edited.conf").unwrap().status,
            DiffStatus::ChangedLocalNewer
        );

        let two_way = plan_sync_actions(&diffs, SyncMode::TwoWayNewest);
        assert!(two_way.contains(&SyncAction::Upload {
            rel_path: "local_only.toml".into(),
            size: 20
        }));
        assert!(two_way.contains(&SyncAction::Upload {
            rel_path: "edited.conf".into(),
            size: 30
        }));
        assert!(two_way.contains(&SyncAction::Download {
            rel_path: "remote_only.log".into(),
            size: 40
        }));

        let mirror_up = plan_sync_actions(&diffs, SyncMode::MirrorUpload);
        assert!(!mirror_up.iter().any(|a| matches!(a, SyncAction::Download { .. })));
    }
}
