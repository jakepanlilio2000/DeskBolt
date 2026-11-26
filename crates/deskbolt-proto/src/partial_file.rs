use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::digest::{Context, SHA256};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub const DEFAULT_PART_TTL_HOURS: u64 = 24;
pub const FILE_CHUNK_BYTES: usize = 64 * 1024;
const MAGIC: &[u8; 8] = b"DBPART01";

struct Checkpoint {
    total: u64,
    created: u64,
    offset: u64,
    hash: [u8; 32],
}

type Expirations = Mutex<HashMap<PathBuf, (u64, u64)>>;
static EXPIRATIONS: OnceLock<Expirations> = OnceLock::new();

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn suffix(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(extension);
    name.into()
}

fn checkpoint(path: &Path) -> io::Result<Checkpoint> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other("Invalid partial file metadata."));
    }
    let mut file = std::fs::File::open(path)?;
    let mut data = [0; 64];
    if file.metadata()?.len() != data.len() as u64 {
        return Err(io::Error::other("Invalid partial file metadata."));
    }
    file.read_exact(&mut data)?;
    if &data[..8] != MAGIC {
        return Err(io::Error::other("Invalid partial file metadata."));
    }
    Ok(Checkpoint {
        total: u64::from_le_bytes(data[8..16].try_into().unwrap()),
        created: u64::from_le_bytes(data[16..24].try_into().unwrap()),
        offset: u64::from_le_bytes(data[24..32].try_into().unwrap()),
        hash: data[32..64].try_into().unwrap(),
    })
}

fn track(part: PathBuf, created: u64, ttl: u64) {
    let entries = EXPIRATIONS.get_or_init(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(60));
                if let Some(entries) = EXPIRATIONS.get() {
                    entries.lock().unwrap().retain(|part, (created, ttl)| {
                        now().saturating_sub(*created) < *ttl || !purge_expired(part, *created)
                    });
                }
            }
        });
        Expirations::default()
    });
    entries.lock().unwrap().insert(part, (created, ttl));
}

// shortcut: Sweep checkpoints observed by this process. Discover expired checkpoints
// after a restart when their destination is reused, without walking unrelated folders.
fn purge_expired(part: &Path, created: u64) -> bool {
    let meta = suffix(part, ".meta");
    if !checkpoint(&meta).is_ok_and(|saved| saved.created == created) {
        return true;
    }
    match open_existing(part) {
        Ok(file) => {
            if std::fs::remove_file(part).is_err() {
                return false;
            }
            let _ = std::fs::remove_file(meta);
            drop(file);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(meta);
        }
        Err(_) => return false,
    }
    true
}

fn forget(path: &Path) {
    if let Some(entries) = EXPIRATIONS.get() {
        entries.lock().unwrap().remove(path);
    }
}

fn open_existing(path: &Path) -> io::Result<std::fs::File> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other("Invalid partial file."));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock().map_err(io::Error::from)?;
    Ok(file)
}

pub async fn offer(target: &Path, ttl_hours: u64) -> Option<(u64, [u8; 32], u64)> {
    let part = suffix(target, ".part");
    let saved = checkpoint(&suffix(&part, ".meta")).ok()?;
    if saved.offset == 0
        || saved.offset > saved.total
        || now().saturating_sub(saved.created) >= ttl_hours.saturating_mul(3600)
    {
        return None;
    }
    let mut file = File::from_std(open_existing(&part).ok()?);
    if file.metadata().await.ok()?.len() != saved.offset {
        return None;
    }
    let hash = hash_prefix(&mut file, saved.offset).await.ok()?;
    (digest(&hash) == saved.hash).then_some((saved.offset, saved.hash, saved.total))
}

pub async fn hash_prefix(file: &mut File, offset: u64) -> io::Result<Context> {
    file.rewind().await?;
    let mut hash = Context::new(&SHA256);
    let mut remaining = offset;
    let mut buffer = vec![0; FILE_CHUNK_BYTES];
    while remaining != 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..take]).await?;
        hash.update(&buffer[..take]);
        remaining -= take as u64;
    }
    Ok(hash)
}

pub fn digest(hash: &Context) -> [u8; 32] {
    hash.clone().finish().as_ref().try_into().unwrap()
}

pub async fn resume_source(
    file: &mut File,
    (offset, prefix): (u64, Option<[u8; 32]>),
    size: u64,
) -> io::Result<(u64, Option<[u8; 32]>, Context)> {
    if let Some(prefix) = prefix
        && offset > 0
        && offset <= size
        && offset <= file.metadata().await?.len()
        && let Ok(hash) = hash_prefix(file, offset).await
        && digest(&hash) == prefix
    {
        return Ok((offset, Some(prefix), hash));
    }
    Ok((0, None, hash_prefix(file, 0).await?))
}

pub struct PartialFile {
    path: PathBuf,
    target: PathBuf,
    file: File,
    hash: Context,
    offset: u64,
    total: u64,
    created: u64,
    ttl: u64,
}

impl PartialFile {
    pub async fn open(target: &Path, total: u64, ttl_hours: u64) -> io::Result<Self> {
        let path = suffix(target, ".part");
        let meta = suffix(&path, ".meta");
        let (file, saved) = match fs::symlink_metadata(&path).await {
            Ok(_) => {
                let file = open_existing(&path)?;
                (file, Some(checkpoint(&meta)?))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.try_lock().map_err(io::Error::from)?;
                if let Err(error) = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&meta)
                {
                    drop(file);
                    let _ = fs::remove_file(&path).await;
                    return Err(error);
                }
                (file, None)
            }
            Err(error) => return Err(error),
        };
        let mut part = Self {
            path,
            target: target.to_owned(),
            file: File::from_std(file),
            hash: Context::new(&SHA256),
            offset: 0,
            total,
            created: now(),
            ttl: ttl_hours.saturating_mul(3600),
        };
        if let Some(saved) = saved
            && saved.total == total
            && saved.offset <= total
            && now().saturating_sub(saved.created) < part.ttl
            && part.file.metadata().await?.len() == saved.offset
            && let Ok(hash) = hash_prefix(&mut part.file, saved.offset).await
            && digest(&hash) == saved.hash
        {
            part.hash = hash;
            part.offset = saved.offset;
            part.created = saved.created;
        } else {
            part.restart().await?;
        }
        track(part.path.clone(), part.created, part.ttl);
        Ok(part)
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn prefix(&self) -> Option<[u8; 32]> {
        (self.offset != 0).then(|| digest(&self.hash))
    }

    pub async fn restart(&mut self) -> io::Result<()> {
        self.file.set_len(0).await?;
        self.file.rewind().await?;
        self.hash = Context::new(&SHA256);
        self.offset = 0;
        self.save().await
    }

    pub async fn append(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if offset != self.offset
            || data.is_empty()
            || data.len() > FILE_CHUNK_BYTES
            || data.len() as u64 > self.total.saturating_sub(self.offset)
        {
            return Err(io::Error::other("Invalid file chunk offset or length."));
        }
        self.file.write_all(data).await?;
        self.file.flush().await?;
        self.offset += data.len() as u64;
        self.hash.update(data);
        self.save().await
    }

    async fn save(&self) -> io::Result<()> {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..16].copy_from_slice(&self.total.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.created.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.offset.to_le_bytes());
        bytes[32..].copy_from_slice(&digest(&self.hash));
        let meta = suffix(&self.path, ".meta");
        let temp = suffix(
            &meta,
            &format!(
                ".{:016x}.tmp",
                u64::from_le_bytes(crate::random_bytes::<8>().map_err(io::Error::other)?)
            ),
        );
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        let result = async {
            file.write_all(&bytes).await?;
            file.flush().await?;
            drop(file);
            fs::rename(&temp, &meta).await
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(temp).await;
        }
        result
    }

    pub async fn finish(self, total: u64, sha256: [u8; 32]) -> io::Result<()> {
        if self.offset != total || total != self.total || digest(&self.hash) != sha256 {
            self.discard().await;
            return Err(io::Error::other("SHA-256 checksum or byte count mismatch."));
        }
        fs::rename(&self.path, &self.target).await?;
        let _ = fs::remove_file(suffix(&self.path, ".meta")).await;
        forget(&self.path);
        Ok(())
    }

    pub async fn discard(self) {
        let _ = fs::remove_file(&self.path).await;
        let _ = fs::remove_file(suffix(&self.path, ".meta")).await;
        forget(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_recovers_from_tampering_and_cleanup_respects_active_lock() {
        let dir = std::env::temp_dir().join(format!(
            "deskbolt-part-{:?}",
            crate::random_bytes::<8>().unwrap()
        ));
        fs::create_dir(&dir).await.unwrap();
        let target = dir.join("sample.txt");
        let mut partial = PartialFile::open(&target, 10, DEFAULT_PART_TTL_HOURS)
            .await
            .unwrap();
        partial.append(0, b"first").await.unwrap();
        let path = partial.path.clone();
        assert!(!purge_expired(&path, partial.created));
        assert!(
            PartialFile::open(&target, 10, DEFAULT_PART_TTL_HOURS)
                .await
                .is_err()
        );
        drop(partial);

        fs::write(&path, b"other").await.unwrap();
        assert_eq!(offer(&target, DEFAULT_PART_TTL_HOURS).await, None);
        let partial = PartialFile::open(&target, 10, DEFAULT_PART_TTL_HOURS)
            .await
            .unwrap();
        assert_eq!((partial.offset(), partial.prefix()), (0, None));
        let created = partial.created;
        drop(partial);
        assert!(purge_expired(&path, created));
        assert!(!path.exists());
        assert!(!suffix(&path, ".meta").exists());
        forget(&path);
        fs::remove_dir(dir).await.unwrap();
    }
}
