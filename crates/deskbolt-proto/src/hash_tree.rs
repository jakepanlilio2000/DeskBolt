use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

use ignore::gitignore::GitignoreBuilder;
use ring::digest::{Context, SHA256};

use crate::SyncEntry;

pub const HASH_PAGE_SIZE: u32 = 512;
pub const DEFAULT_EXCLUDE_GLOBS: &str = "node_modules/\n.git/\ntarget/\n*.tmp";

pub fn safe_relative_path(path: &str) -> bool {
    path.split('/').all(crate::valid_drop_filename)
}

pub async fn scan_hash_page(
    root: std::path::PathBuf,
    cursor: Option<String>,
    page_size: u32,
    globs: Vec<String>,
) -> Result<(Vec<SyncEntry>, Option<String>), String> {
    tokio::task::spawn_blocking(move || scan(&root, cursor.as_deref(), page_size, &globs))
        .await
        .map_err(|error| error.to_string())?
}

fn scan(
    root: &Path,
    cursor: Option<&str>,
    page_size: u32,
    globs: &[String],
) -> Result<(Vec<SyncEntry>, Option<String>), String> {
    if globs.len() > 128 || globs.iter().any(|glob| glob.len() > 1024) {
        return Err("Too many or oversized exclusion rules.".into());
    }
    let mut builder = GitignoreBuilder::new(".");
    for glob in globs {
        builder.add_line(None, glob).map_err(|e| e.to_string())?;
    }
    let exclusions = builder.build().map_err(|e| e.to_string())?;
    let cursor = cursor.map(|path| path.split('/').collect::<Vec<_>>());
    let mut stack = vec![children(root, "")?.into_iter()];
    let mut page = Vec::<SyncEntry>::new();
    let mut path_bytes = 0;

    while let Some(directory) = stack.last_mut() {
        let Some((relative, item)) = directory.next() else {
            stack.pop();
            continue;
        };
        let parts = relative.split('/').collect::<Vec<_>>();
        let before_cursor = cursor.as_ref().is_some_and(|last| parts <= *last);
        if let Some(last) = &cursor
            && before_cursor && !last.starts_with(&parts)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&item).map_err(|e| e.to_string())?;
        let kind = metadata.file_type();
        if !(kind.is_dir() || kind.is_file())
            || exclusions.matched(&relative, kind.is_dir()).is_ignore()
        {
            continue;
        }
        if !before_cursor {
            if page.len() >= page_size.clamp(1, HASH_PAGE_SIZE) as usize
                || (!page.is_empty() && path_bytes + relative.len() > 8 * 1024 * 1024)
            {
                let next = page.last().map(|entry| entry.rel_path.clone());
                return Ok((page, next));
            }
            let sha256 = if kind.is_file() {
                Some(hash_file(&item).map_err(|e| e.to_string())?)
            } else {
                None
            };
            path_bytes += relative.len();
            page.push(SyncEntry {
                rel_path: relative.clone(),
                is_dir: kind.is_dir(),
                size: if kind.is_file() { metadata.len() } else { 0 },
                modified_unix: metadata.modified().ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs() as i64),
                sha256,
            });
        }
        if kind.is_dir() {
            stack.push(children(&item, &relative)?.into_iter());
        }
    }
    Ok((page, None))
}

fn children(dir: &Path, prefix: &str) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    let mut items = fs::read_dir(dir).map_err(|e| e.to_string())?
        .map(|entry| {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().into_string()
                .map_err(|_| "A file name cannot be represented on the wire.")?;
            let relative = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            Ok((relative, entry.path()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    items.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(items)
}

fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Context::new(&SHA256);
    let mut data = [0; 64 * 1024];
    loop {
        let count = file.read(&mut data)?;
        if count == 0 {
            return Ok(hasher.finish().as_ref().try_into().expect("SHA-256 length"));
        }
        hasher.update(&data[..count]);
    }
}
