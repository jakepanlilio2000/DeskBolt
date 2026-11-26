use std::path::PathBuf;

const UNAVAILABLE: &str = "The host desktop folder is unavailable. Open Files and choose a destination.";

#[cfg(target_os = "windows")]
pub fn desktop_dir() -> Result<PathBuf, String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: The shell returns an owned path. The session token and path are released here.
    unsafe {
        let console = WTSGetActiveConsoleSessionId();
        let mut token = HANDLE::default();
        let token = if WTSQueryUserToken(console, &mut token).is_ok() {
            Some(token)
        } else {
            let mut current = u32::MAX;
            ProcessIdToSessionId(std::process::id(), &mut current).map_err(|_| UNAVAILABLE)?;
            if current == 0 {
                return Err(UNAVAILABLE.into());
            }
            None
        };
        let folder = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, token);
        if let Some(token) = token {
            let _ = CloseHandle(token);
        }
        let folder = folder.map_err(|_| UNAVAILABLE)?;
        let path = PathBuf::from(String::from_utf16_lossy(folder.as_wide()));
        CoTaskMemFree(Some(folder.0.cast()));
        path.is_dir().then_some(path).ok_or_else(|| UNAVAILABLE.into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn desktop_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: Reads the effective user ID of the current process.
    let effective = unsafe { libc::geteuid() };
    let uid = if effective != 0 {
        effective
    } else {
        #[cfg(target_os = "linux")]
        let console_uid = std::env::var_os("XDG_RUNTIME_DIR")
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.uid())
            .or_else(linux_console_uid);
        #[cfg(target_os = "macos")]
        let console_uid = std::fs::metadata("/dev/console").ok().map(|metadata| metadata.uid());
        console_uid.filter(|&id| id != 0).ok_or(UNAVAILABLE)?
    };
    let home = unix_home(uid).ok_or(UNAVAILABLE)?;
    #[cfg(target_os = "linux")]
    let desktop = linux_desktop_path(&home, effective == uid);
    #[cfg(target_os = "macos")]
    let desktop = home.join("Desktop");
    let desktop = desktop.canonicalize().map_err(|_| UNAVAILABLE)?;
    let metadata = desktop.metadata().map_err(|_| UNAVAILABLE)?;
    if metadata.is_dir() && metadata.uid() == uid {
        Ok(desktop)
    } else {
        Err(UNAVAILABLE.into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unix_home(uid: libc::uid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let mut raw = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut storage = [0u8; 16_384];
    let mut found = std::ptr::null_mut();
    // SAFETY: The reentrant account lookup writes only to the supplied buffer and outputs.
    unsafe {
        if libc::getpwuid_r(uid, raw.as_mut_ptr(), storage.as_mut_ptr().cast(), storage.len(), &mut found) != 0
            || found.is_null() || (*found).pw_dir.is_null()
        {
            return None;
        }
        let bytes = std::ffi::CStr::from_ptr((*found).pw_dir).to_bytes();
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        home.is_absolute().then_some(home)
    }
}

#[cfg(target_os = "linux")]
fn linux_console_uid() -> Option<libc::uid_t> {
    let seat = std::fs::read_to_string("/run/systemd/seats/seat0").ok()?;
    let session = seat.lines().find_map(|line| line.strip_prefix("ACTIVE="))?;
    if session.is_empty() || !session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    let session = std::fs::read_to_string(PathBuf::from("/run/systemd/sessions").join(session)).ok()?;
    session.lines().find_map(|line| line.strip_prefix("UID="))?.parse().ok()
}

#[cfg(target_os = "linux")]
fn linux_desktop_path(home: &std::path::Path, own_session: bool) -> PathBuf {
    let config_dir = own_session.then(|| std::env::var_os("XDG_CONFIG_HOME")).flatten()
        .map(PathBuf::from).filter(|path| path.is_absolute()).unwrap_or_else(|| home.join(".config"));
    if let Ok(config) = std::fs::read_to_string(config_dir.join("user-dirs.dirs"))
        && let Some(path) = config.lines().find_map(|line| {
            line.trim().strip_prefix("XDG_DESKTOP_DIR=\"")?.strip_suffix('"')
        })
    {
        if let Some(relative) = path.strip_prefix("$HOME/") {
            return home.join(relative);
        }
        if std::path::Path::new(path).is_absolute() {
            return PathBuf::from(path);
        }
    }
    home.join("Desktop")
}

#[cfg(target_os = "android")]
pub fn desktop_dir() -> Result<PathBuf, String> {
    Err(UNAVAILABLE.into())
}
