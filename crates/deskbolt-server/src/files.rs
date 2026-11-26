use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use deskbolt_proto::{DESKTOP_UPLOAD_PREFIX, DriveEntry, FileEntry, FileTransferMsg, TransferDir, valid_drop_filename};
use ring::digest::{Context, SHA256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW};
#[cfg(target_os = "windows")]
use windows::core::PCWSTR;

#[cfg(target_os = "windows")]
use crate::win32_util::to_wide_null;

pub const FILE_CHUNK_SIZE: usize = 64 * 1024;
pub const MAX_UNACKED_CHUNKS: u32 = 8;

/// Rejects path traversal (`..`), UNC/device prefixes (`\\.\`, `\\`, `//`), and null bytes.
pub fn validate_remote_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() || raw.contains('\0') {
        return Err("empty or invalid path".into());
    }
    let trimmed = raw.trim();
    if trimmed.starts_with(r"\\") || trimmed.starts_with("//") {
        return Err("UNC and device paths are not permitted".into());
    }
    let p = Path::new(trimmed);
    #[cfg(target_os = "windows")]
    {
        let mut has_drive = false;
        for comp in p.components() {
            match comp {
                Component::Prefix(prefix) => match prefix.kind() {
                    std::path::Prefix::Disk(_) => has_drive = true,
                    _ => return Err("unsupported path prefix".into()),
                },
                Component::ParentDir => return Err("parent traversal '..' is not permitted".into()),
                Component::RootDir | Component::CurDir | Component::Normal(_) => {}
            }
        }
        if !has_drive || !p.is_absolute() {
            return Err("path must include an absolute drive letter (e.g. C:\\...)".into());
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        for comp in p.components() {
            if matches!(comp, Component::ParentDir) {
                return Err("parent traversal '..' is not permitted".into());
            }
        }
        if !p.is_absolute() {
            return Err("path must be an absolute path (e.g. /home/...)".into());
        }
    }
    Ok(p.to_path_buf())
}

struct ActiveUpload {
    part_path: PathBuf,
    final_path: PathBuf,
    file: File,
    hasher: Context,
    bytes_written: u64,
    expected_bytes: u64,
}

struct ActiveDownload {
    path: PathBuf,
    transfer_id: u64,
    file: File,
    hasher: Context,
    bytes_sent: u64,
    total_bytes: u64,
    next_seq: u32,
    acked_seq: u32,
    completed_sent: bool,
}

/// Stateful per-session handler for Channel `0x05` FileTransfer operations.
pub struct FileSessionHandler {
    uploads: HashMap<u64, ActiveUpload>,
    downloads: HashMap<u64, ActiveDownload>,
}

impl Default for FileSessionHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSessionHandler {
    pub fn new() -> Self {
        Self {
            uploads: HashMap::new(),
            downloads: HashMap::new(),
        }
    }

    pub fn pack_reply(&self, message: &FileTransferMsg) -> Result<deskbolt_proto::Frame, deskbolt_proto::ProtoError> {
        let source = match message {
            FileTransferMsg::FileChunk { transfer_id, .. } =>
                self.downloads.get(transfer_id).map(|download| download.path.as_path()),
            _ => None,
        };
        deskbolt_proto::frame::pack_file_message(message, source)
    }

    /// Processes a single incoming `FileTransferMsg`, returning zero or more immediate response messages.
    pub async fn handle_message(&mut self, msg: FileTransferMsg) -> Vec<FileTransferMsg> {
        match msg {
            FileTransferMsg::ListRootsReq => {
                vec![FileTransferMsg::ListRootsResp {
                    roots: list_remote_roots(),
                }]
            }
            FileTransferMsg::ListDirReq { path } => vec![list_directory(&path).await],
            FileTransferMsg::MkdirReq { path } => vec![create_directory(&path).await],
            FileTransferMsg::DeleteReq { path, recursive } => {
                vec![delete_path(&path, recursive).await]
            }
            FileTransferMsg::RenameReq { from_path, to_path } => {
                vec![rename_path(&from_path, &to_path).await]
            }
            FileTransferMsg::TransferStart {
                transfer_id,
                direction,
                remote_path,
                total_bytes,
                modified_unix: _,
                overwrite,
            } => self.handle_transfer_start(transfer_id, direction, &remote_path, total_bytes, overwrite).await,
            FileTransferMsg::FileChunk {
                transfer_id,
                offset,
                chunk_seq,
                data,
            } => self.handle_upload_chunk(transfer_id, offset, chunk_seq, &data).await,
            FileTransferMsg::FileAck {
                transfer_id,
                acked_seq,
            } => self.handle_download_ack(transfer_id, acked_seq).await,
            FileTransferMsg::FileComplete {
                transfer_id,
                total_bytes,
                sha256,
            } => vec![self.handle_upload_complete(transfer_id, total_bytes, sha256).await],
            FileTransferMsg::FileVerifyResult { transfer_id, .. } => {
                self.downloads.remove(&transfer_id);
                Vec::new()
            }
            FileTransferMsg::FileAbort { transfer_id, .. } => {
                self.abort_transfer(transfer_id).await;
                Vec::new()
            }
            FileTransferMsg::HashTreeReq { root, cursor, page_size, exclude_globs } => {
                let result = match validate_remote_path(&root) {
                    Ok(path) => deskbolt_proto::hash_tree::scan_hash_page(path, cursor, page_size, exclude_globs).await,
                    Err(error) => Err(error),
                };
                let (entries, next_cursor, error) = match result {
                    Ok((entries, cursor)) => (entries, cursor, None),
                    Err(error) => (Vec::new(), None, Some(error)),
                };
                vec![FileTransferMsg::HashTreeResp { root, entries, next_cursor, error }]
            }
            FileTransferMsg::TailChunkReq {
                path,
                offset,
                max_bytes,
            } => vec![read_tail_chunk(&path, offset, max_bytes).await],
            _ => Vec::new(),
        }
    }

    /// Pumps the next burst of download chunks (up to `MAX_UNACKED_CHUNKS`) when window allows.
    pub async fn poll_downloads(&mut self) -> Vec<FileTransferMsg> {
        let mut out = Vec::new();
        let ids: Vec<u64> = self.downloads.keys().copied().collect();
        for id in ids {
            let Some(dl) = self.downloads.get_mut(&id) else {
                continue;
            };
            while !dl.completed_sent && dl.next_seq.saturating_sub(dl.acked_seq) < MAX_UNACKED_CHUNKS {
                let mut buf = vec![0u8; FILE_CHUNK_SIZE];
                match dl.file.read(&mut buf).await {
                    Ok(0) => {
                        let sha_digest = dl.hasher.clone().finish();
                        let mut sha256 = [0u8; 32];
                        sha256.copy_from_slice(sha_digest.as_ref());
                        out.push(FileTransferMsg::FileComplete {
                            transfer_id: dl.transfer_id,
                            total_bytes: dl.bytes_sent,
                            sha256,
                        });
                        dl.completed_sent = true;
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        dl.hasher.update(&buf);
                        let offset = dl.bytes_sent;
                        dl.bytes_sent += n as u64;
                        dl.next_seq = dl.next_seq.saturating_add(1);
                        out.push(FileTransferMsg::FileChunk {
                            transfer_id: dl.transfer_id,
                            offset,
                            chunk_seq: dl.next_seq,
                            data: buf,
                        });
                    }
                    Err(e) => {
                        out.push(FileTransferMsg::FileAbort {
                            transfer_id: dl.transfer_id,
                            reason: format!("read error: {e}"),
                        });
                        self.downloads.remove(&id);
                        break;
                    }
                }
            }
        }
        out
    }

    /// Cleans up partial `.part` files when a session disconnects mid-transfer.
    pub async fn cleanup_all(&mut self) {
        let upload_ids: Vec<u64> = self.uploads.keys().copied().collect();
        for id in upload_ids {
            self.abort_transfer(id).await;
        }
        self.downloads.clear();
    }

    async fn handle_transfer_start(
        &mut self,
        transfer_id: u64,
        direction: TransferDir,
        remote_path: &str,
        total_bytes: u64,
        overwrite: bool,
    ) -> Vec<FileTransferMsg> {
        let validated = match transfer_path(direction, remote_path) {
            Ok(p) => p,
            Err(e) => {
                return vec![FileTransferMsg::TransferStartAck {
                    transfer_id,
                    accepted: false,
                    exists: false,
                    existing_size: 0,
                    existing_modified_unix: 0,
                    error: Some(e),
                }];
            }
        };

        match direction {
            TransferDir::Upload => {
                if let Ok(meta) = fs::metadata(&validated).await
                    && !overwrite
                {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    return vec![FileTransferMsg::TransferStartAck {
                        transfer_id,
                        accepted: false,
                        exists: true,
                        existing_size: meta.len(),
                        existing_modified_unix: mtime,
                        error: None,
                    }];
                }
                if let Some(parent) = validated.parent()
                    && let Err(e) = fs::create_dir_all(parent).await
                {
                    return vec![FileTransferMsg::TransferStartAck {
                        transfer_id,
                        accepted: false,
                        exists: false,
                        existing_size: 0,
                        existing_modified_unix: 0,
                        error: Some(format!("create parent directory failed: {e}")),
                    }];
                }
                let part_path = part_path_for(&validated, transfer_id);
                match OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&part_path)
                    .await
                {
                    Ok(file) => {
                        self.uploads.insert(
                            transfer_id,
                            ActiveUpload {
                                part_path,
                                final_path: validated,
                                file,
                                hasher: Context::new(&SHA256),
                                bytes_written: 0,
                                expected_bytes: total_bytes,
                            },
                        );
                        vec![FileTransferMsg::TransferStartAck {
                            transfer_id,
                            accepted: true,
                            exists: false,
                            existing_size: 0,
                            existing_modified_unix: 0,
                            error: None,
                        }]
                    }
                    Err(e) => vec![FileTransferMsg::TransferStartAck {
                        transfer_id,
                        accepted: false,
                        exists: false,
                        existing_size: 0,
                        existing_modified_unix: 0,
                        error: Some(format!("open target failed: {e}")),
                    }],
                }
            }
            TransferDir::Download => {
                let meta = match fs::metadata(&validated).await {
                    Ok(m) if m.is_file() => m,
                    Ok(_) => {
                        return vec![FileTransferMsg::TransferStartAck {
                            transfer_id,
                            accepted: false,
                            exists: true,
                            existing_size: 0,
                            existing_modified_unix: 0,
                            error: Some("requested path is not a file".into()),
                        }];
                    }
                    Err(e) => {
                        return vec![FileTransferMsg::TransferStartAck {
                            transfer_id,
                            accepted: false,
                            exists: false,
                            existing_size: 0,
                            existing_modified_unix: 0,
                            error: Some(format!("stat failed: {e}")),
                        }];
                    }
                };
                match File::open(&validated).await {
                    Ok(file) => {
                        let size = meta.len();
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        self.downloads.insert(
                            transfer_id,
                            ActiveDownload {
                                path: validated,
                                transfer_id,
                                file,
                                hasher: Context::new(&SHA256),
                                bytes_sent: 0,
                                total_bytes: size,
                                next_seq: 0,
                                acked_seq: 0,
                                completed_sent: false,
                            },
                        );
                        let mut out = vec![FileTransferMsg::TransferStartAck {
                            transfer_id,
                            accepted: true,
                            exists: true,
                            existing_size: size,
                            existing_modified_unix: mtime,
                            error: None,
                        }];
                        out.extend(self.poll_downloads().await);
                        out
                    }
                    Err(e) => vec![FileTransferMsg::TransferStartAck {
                        transfer_id,
                        accepted: false,
                        exists: true,
                        existing_size: 0,
                        existing_modified_unix: 0,
                        error: Some(format!("open failed: {e}")),
                    }],
                }
            }
        }
    }

    async fn handle_upload_chunk(
        &mut self,
        transfer_id: u64,
        offset: u64,
        chunk_seq: u32,
        data: &[u8],
    ) -> Vec<FileTransferMsg> {
        let Some(up) = self.uploads.get_mut(&transfer_id) else {
            return vec![FileTransferMsg::FileAbort {
                transfer_id,
                reason: "unknown upload transfer id".into(),
            }];
        };
        if offset != up.bytes_written || data.len() > FILE_CHUNK_SIZE {
            self.abort_transfer(transfer_id).await;
            return vec![FileTransferMsg::FileAbort {
                transfer_id,
                reason: "out-of-order or oversized chunk".into(),
            }];
        }
        if let Err(e) = up.file.write_all(data).await {
            self.abort_transfer(transfer_id).await;
            return vec![FileTransferMsg::FileAbort {
                transfer_id,
                reason: format!("write failed: {e}"),
            }];
        }
        up.hasher.update(data);
        up.bytes_written += data.len() as u64;
        vec![FileTransferMsg::FileAck {
            transfer_id,
            acked_seq: chunk_seq,
        }]
    }

    async fn handle_download_ack(&mut self, transfer_id: u64, acked_seq: u32) -> Vec<FileTransferMsg> {
        if let Some(dl) = self.downloads.get_mut(&transfer_id) {
            dl.acked_seq = dl.acked_seq.max(acked_seq);
            let _ = dl.total_bytes;
        }
        self.poll_downloads().await
    }

    async fn handle_upload_complete(
        &mut self,
        transfer_id: u64,
        total_bytes: u64,
        expected_sha256: [u8; 32],
    ) -> FileTransferMsg {
        let Some(mut up) = self.uploads.remove(&transfer_id) else {
            return FileTransferMsg::FileVerifyResult {
                transfer_id,
                verified: false,
                error: Some("unknown transfer id".into()),
            };
        };

        if let Err(e) = up.file.flush().await {
            drop(up.file);
            let _ = fs::remove_file(&up.part_path).await;
            return FileTransferMsg::FileVerifyResult {
                transfer_id,
                verified: false,
                error: Some(format!("flush failed: {e}")),
            };
        }
        drop(up.file);

        let actual = up.hasher.finish();
        if up.bytes_written != total_bytes
            || up.bytes_written != up.expected_bytes
            || actual.as_ref() != expected_sha256
        {
            let _ = fs::remove_file(&up.part_path).await;
            return FileTransferMsg::FileVerifyResult {
                transfer_id,
                verified: false,
                error: Some("SHA-256 checksum or byte count mismatch".into()),
            };
        }

        if let Err(e) = fs::rename(&up.part_path, &up.final_path).await {
            let _ = fs::remove_file(&up.part_path).await;
            return FileTransferMsg::FileVerifyResult {
                transfer_id,
                verified: false,
                error: Some(format!("finalize file rename failed: {e}")),
            };
        }

        FileTransferMsg::FileVerifyResult {
            transfer_id,
            verified: true,
            error: None,
        }
    }

    async fn abort_transfer(&mut self, transfer_id: u64) {
        if let Some(up) = self.uploads.remove(&transfer_id) {
            drop(up.file);
            let _ = fs::remove_file(&up.part_path).await;
        }
        self.downloads.remove(&transfer_id);
    }
}

fn transfer_path(direction: TransferDir, raw: &str) -> Result<PathBuf, String> {
    if direction == TransferDir::Upload
        && let Some(name) = raw.strip_prefix(DESKTOP_UPLOAD_PREFIX)
    {
        if !valid_drop_filename(name) {
            return Err("The dropped file name is invalid.".into());
        }
        let desktop = crate::platform::desktop_dir()?;
        return validate_remote_path(&desktop.join(name).to_string_lossy());
    }
    validate_remote_path(raw)
}

fn part_path_for(target: &Path, transfer_id: u64) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    target.with_file_name(format!(".{name}.{transfer_id}.part"))
}

#[cfg(target_os = "windows")]
fn list_remote_roots() -> Vec<DriveEntry> {
    let mut roots = Vec::new();
    // SAFETY: GetLogicalDrives returns a bitmask of mounted drive letters A..Z.
    let mask = unsafe { GetLogicalDrives() };
    for idx in 0..26u8 {
        if (mask & (1 << idx)) == 0 {
            continue;
        }
        let letter = format!("{}:\\", (b'A' + idx) as char);
        let wide = to_wide_null(&letter);
        let mut free_avail = 0u64;
        let mut total = 0u64;
        // SAFETY: wide is null-terminated; output pointers are valid u64 locals.
        unsafe {
            let _ = GetDiskFreeSpaceExW(
                PCWSTR(wide.as_ptr()),
                Some(&mut free_avail),
                Some(&mut total),
                None,
            );
        }
        let mut vol_buf = [0u16; 128];
        // SAFETY: wide is null-terminated; vol_buf length is passed cleanly.
        let label = unsafe {
            if GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                Some(&mut vol_buf),
                None,
                None,
                None,
                None,
            )
            .is_ok()
            {
                let end = vol_buf.iter().position(|&c| c == 0).unwrap_or(vol_buf.len());
                String::from_utf16_lossy(&vol_buf[..end])
            } else {
                String::new()
            }
        };
        roots.push(DriveEntry {
            letter,
            label,
            free_bytes: free_avail,
            total_bytes: total,
        });
    }
    roots
}

#[cfg(not(target_os = "windows"))]
fn list_remote_roots() -> Vec<DriveEntry> {
    let mut roots = vec![DriveEntry {
        letter: "/".into(),
        label: "Root".into(),
        free_bytes: 0,
        total_bytes: 0,
    }];
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if p.exists() {
            roots.push(DriveEntry {
                letter: p.to_string_lossy().into_owned(),
                label: "Home".into(),
                free_bytes: 0,
                total_bytes: 0,
            });
        }
    }
    roots
}

async fn list_directory(raw_path: &str) -> FileTransferMsg {
    let path = match validate_remote_path(raw_path) {
        Ok(p) => p,
        Err(e) => {
            return FileTransferMsg::ListDirResp {
                path: raw_path.to_string(),
                entries: Vec::new(),
                error: Some(e),
            };
        }
    };
    let mut entries = Vec::new();
    let mut rd = match fs::read_dir(&path).await {
        Ok(r) => r,
        Err(e) => {
            return FileTransferMsg::ListDirResp {
                path: path.to_string_lossy().into_owned(),
                entries,
                error: Some(format!("cannot read directory: {e}")),
            };
        }
    };
    while let Ok(Some(item)) = rd.next_entry().await {
        let name = item.file_name().to_string_lossy().into_owned();
        if let Ok(meta) = item.metadata().await {
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            entries.push(FileEntry {
                name,
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                modified_unix,
                readonly: meta.permissions().readonly(),
            });
        }
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    FileTransferMsg::ListDirResp {
        path: path.to_string_lossy().into_owned(),
        entries,
        error: None,
    }
}

async fn create_directory(raw_path: &str) -> FileTransferMsg {
    let path = match validate_remote_path(raw_path) {
        Ok(p) => p,
        Err(e) => {
            return FileTransferMsg::MkdirResp {
                ok: false,
                error: Some(e),
            };
        }
    };
    match fs::create_dir_all(&path).await {
        Ok(()) => FileTransferMsg::MkdirResp {
            ok: true,
            error: None,
        },
        Err(e) => FileTransferMsg::MkdirResp {
            ok: false,
            error: Some(format!("create folder failed: {e}")),
        },
    }
}

async fn delete_path(raw_path: &str, recursive: bool) -> FileTransferMsg {
    let path = match validate_remote_path(raw_path) {
        Ok(p) => p,
        Err(e) => {
            return FileTransferMsg::DeleteResp {
                ok: false,
                error: Some(e),
            };
        }
    };
    let res = match fs::metadata(&path).await {
        Ok(m) if m.is_dir() => {
            if recursive {
                fs::remove_dir_all(&path).await
            } else {
                fs::remove_dir(&path).await
            }
        }
        Ok(_) => fs::remove_file(&path).await,
        Err(e) => Err(e),
    };
    match res {
        Ok(()) => FileTransferMsg::DeleteResp {
            ok: true,
            error: None,
        },
        Err(e) => FileTransferMsg::DeleteResp {
            ok: false,
            error: Some(format!("delete failed: {e}")),
        },
    }
}

async fn rename_path(raw_from: &str, raw_to: &str) -> FileTransferMsg {
    let from = match validate_remote_path(raw_from) {
        Ok(p) => p,
        Err(e) => {
            return FileTransferMsg::RenameResp {
                ok: false,
                error: Some(e),
            };
        }
    };
    let to = match validate_remote_path(raw_to) {
        Ok(p) => p,
        Err(e) => {
            return FileTransferMsg::RenameResp {
                ok: false,
                error: Some(e),
            };
        }
    };
    match fs::rename(&from, &to).await {
        Ok(()) => FileTransferMsg::RenameResp {
            ok: true,
            error: None,
        },
        Err(e) => FileTransferMsg::RenameResp {
            ok: false,
            error: Some(format!("rename failed: {e}")),
        },
    }
}

const MAX_TAIL_CHUNK_BYTES: usize = 256 * 1024;

async fn read_tail_chunk(raw_path: &str, offset: u64, max_bytes: u32) -> FileTransferMsg {
    let path = match validate_remote_path(raw_path) {
        Ok(p) => p,
        Err(e) => {
            return FileTransferMsg::TailChunkResp {
                path: raw_path.to_string(),
                next_offset: offset,
                data: Vec::new(),
                truncated: false,
                error: Some(e),
            };
        }
    };
    let canonical = path.to_string_lossy().into_owned();
    let mut file = match File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            return FileTransferMsg::TailChunkResp {
                path: canonical,
                next_offset: offset,
                data: Vec::new(),
                truncated: false,
                error: Some(format!("open log failed: {e}")),
            };
        }
    };
    let file_len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            return FileTransferMsg::TailChunkResp {
                path: canonical,
                next_offset: offset,
                data: Vec::new(),
                truncated: false,
                error: Some(format!("stat log failed: {e}")),
            };
        }
    };

    let cap = (max_bytes as usize).clamp(1, MAX_TAIL_CHUNK_BYTES);
    let (read_from, truncated) = if offset > file_len {
        (file_len.saturating_sub(cap as u64), true)
    } else {
        (offset, false)
    };

    if let Err(e) = file.seek(SeekFrom::Start(read_from)).await {
        return FileTransferMsg::TailChunkResp {
            path: canonical,
            next_offset: read_from,
            data: Vec::new(),
            truncated,
            error: Some(format!("seek log failed: {e}")),
        };
    }

    let mut data = Vec::new();
    let mut remaining = cap;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let to_read = remaining.min(buf.len());
        match file.read(&mut buf[..to_read]).await {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                remaining -= n;
            }
            Err(e) => {
                return FileTransferMsg::TailChunkResp {
                    path: canonical,
                    next_offset: read_from + data.len() as u64,
                    data,
                    truncated,
                    error: Some(format!("read log failed: {e}")),
                };
            }
        }
    }

    let next_offset = read_from + data.len() as u64;
    FileTransferMsg::TailChunkResp {
        path: canonical,
        next_offset,
        data,
        truncated,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use ring::digest::{SHA256, digest};

    use super::*;

    #[test]
    fn path_traversal_and_unc_rejected() {
        #[cfg(target_os = "windows")]
        {
            assert!(validate_remote_path(r"C:\Users\Admin\file.txt").is_ok());
            assert!(validate_remote_path(r"C:\Users\..\Windows\System32").is_err());
            assert!(validate_remote_path(r"\\.\PhysicalDrive0").is_err());
            assert!(validate_remote_path("relative\\path.txt").is_err());
            assert!(validate_remote_path("C:\\bad\0name").is_err());
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert!(validate_remote_path("/home/admin/file.txt").is_ok());
            assert!(validate_remote_path("/home/../etc/passwd").is_err());
            assert!(validate_remote_path("relative/path.txt").is_err());
            assert!(validate_remote_path("/bad\0name").is_err());
        }
        assert!(validate_remote_path(r"\\server\share\file").is_err());
        assert!(validate_remote_path("//server/share/file").is_err());
    }

    #[tokio::test]
    async fn upload_sha256_verify_and_corrupt_cleanup() {
        let root = std::env::temp_dir().join(format!("deskbolt_ft_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root).await;
        fs::create_dir_all(&root).await.unwrap();

        let target = root.join("payload.bin");
        let data = b"DeskBolt chunked SHA-256 verified transfer payload!".to_vec();
        let good_sha: [u8; 32] = digest(&SHA256, &data).as_ref().try_into().unwrap();

        let mut handler = FileSessionHandler::new();

        // 1. Valid upload succeeds and atomically promotes .part -> target
        let r1 = handler
            .handle_message(FileTransferMsg::TransferStart {
                transfer_id: 1,
                direction: TransferDir::Upload,
                remote_path: target.to_string_lossy().into_owned(),
                total_bytes: data.len() as u64,
                modified_unix: 0,
                overwrite: true,
            })
            .await;
        assert!(matches!(r1[0], FileTransferMsg::TransferStartAck { accepted: true, .. }));

        let r2 = handler
            .handle_message(FileTransferMsg::FileChunk {
                transfer_id: 1,
                offset: 0,
                chunk_seq: 1,
                data: data.clone(),
            })
            .await;
        assert!(matches!(r2[0], FileTransferMsg::FileAck { acked_seq: 1, .. }));

        let r3 = handler
            .handle_message(FileTransferMsg::FileComplete {
                transfer_id: 1,
                total_bytes: data.len() as u64,
                sha256: good_sha,
            })
            .await;
        assert!(matches!(r3[0], FileTransferMsg::FileVerifyResult { verified: true, .. }));
        assert_eq!(fs::read(&target).await.unwrap(), data);

        // 2. Corrupt hash deletes .part and fails verification
        let bad_target = root.join("bad.bin");
        let _ = handler
            .handle_message(FileTransferMsg::TransferStart {
                transfer_id: 2,
                direction: TransferDir::Upload,
                remote_path: bad_target.to_string_lossy().into_owned(),
                total_bytes: data.len() as u64,
                modified_unix: 0,
                overwrite: true,
            })
            .await;
        let _ = handler
            .handle_message(FileTransferMsg::FileChunk {
                transfer_id: 2,
                offset: 0,
                chunk_seq: 1,
                data,
            })
            .await;
        let r_bad = handler
            .handle_message(FileTransferMsg::FileComplete {
                transfer_id: 2,
                total_bytes: 51,
                sha256: [0xFF; 32],
            })
            .await;
        assert!(matches!(r_bad[0], FileTransferMsg::FileVerifyResult { verified: false, .. }));
        assert!(!bad_target.exists());
        assert!(!part_path_for(&bad_target, 2).exists());

        let _ = fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn hash_tree_and_tail_chunk_handle_diffs_and_truncation() {
        let root = std::env::temp_dir().join(format!("deskbolt_sync_tail_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root).await;
        fs::create_dir_all(root.join("nested")).await.unwrap();

        let alpha = b"alpha-content".to_vec();
        let beta = b"beta-nested".to_vec();
        fs::write(root.join("alpha.txt"), &alpha).await.unwrap();
        fs::write(root.join("nested").join("beta.bin"), &beta).await.unwrap();

        let mut handler = FileSessionHandler::new();
        let tree_resp = handler
            .handle_message(FileTransferMsg::HashTreeReq {
                root: root.to_string_lossy().into_owned(),
                cursor: None,
                page_size: 10,
                exclude_globs: Vec::new(),
            })
            .await;
        match &tree_resp[0] {
            FileTransferMsg::HashTreeResp { entries, error, .. } => {
                assert!(error.is_none());
                let alpha_entry = entries.iter().find(|e| e.rel_path == "alpha.txt").unwrap();
                let expected: [u8; 32] = digest(&SHA256, &alpha).as_ref().try_into().unwrap();
                assert_eq!(alpha_entry.sha256, Some(expected));
                assert!(entries.iter().any(|e| e.rel_path == "nested/beta.bin"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        // Log tail incremental append & rotation/truncation detection
        let log_path = root.join("service.log");
        fs::write(&log_path, b"line1\n").await.unwrap();
        let t1 = handler
            .handle_message(FileTransferMsg::TailChunkReq {
                path: log_path.to_string_lossy().into_owned(),
                offset: 0,
                max_bytes: 1024,
            })
            .await;
        let next_off = match &t1[0] {
            FileTransferMsg::TailChunkResp {
                next_offset,
                data,
                truncated,
                error,
                ..
            } => {
                assert!(error.is_none());
                assert!(!truncated);
                assert_eq!(data, b"line1\n");
                *next_offset
            }
            other => panic!("unexpected tail resp: {other:?}"),
        };

        let mut f = OpenOptions::new().append(true).open(&log_path).await.unwrap();
        f.write_all(b"line2\n").await.unwrap();
        drop(f);

        let t2 = handler
            .handle_message(FileTransferMsg::TailChunkReq {
                path: log_path.to_string_lossy().into_owned(),
                offset: next_off,
                max_bytes: 1024,
            })
            .await;
        assert!(matches!(
            &t2[0],
            FileTransferMsg::TailChunkResp { next_offset: 12, data, truncated: false, error: None, .. }
            if data == b"line2\n"
        ));

        // Simulate log truncation shorter than previous offset
        fs::write(&log_path, b"new\n").await.unwrap();
        let t3 = handler
            .handle_message(FileTransferMsg::TailChunkReq {
                path: log_path.to_string_lossy().into_owned(),
                offset: 12,
                max_bytes: 1024,
            })
            .await;
        assert!(matches!(
            &t3[0],
            FileTransferMsg::TailChunkResp { next_offset: 4, data, truncated: true, error: None, .. }
            if data == b"new\n"
        ));

        let _ = fs::remove_dir_all(&root).await;
    }
}
