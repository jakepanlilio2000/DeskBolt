use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use deskbolt_proto::MonitorInfo;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DisplayMode {
    pub output: String,
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

pub(crate) trait Backend {
    fn modes(&self, monitor: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String>;
    fn current(&self, output: &str) -> Result<DisplayMode, String>;
    fn apply(&self, mode: &DisplayMode) -> Result<(), String>;
}

pub(crate) struct Native;

impl Backend for Native {
    fn modes(&self, monitor: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String> {
        crate::platform::display_mode::modes(monitor)
    }

    fn current(&self, output: &str) -> Result<DisplayMode, String> {
        crate::platform::display_mode::current(output)
    }

    fn apply(&self, mode: &DisplayMode) -> Result<(), String> {
        crate::platform::display_mode::apply(mode)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Rollback {
    original: DisplayMode,
    applied: DisplayMode,
    before_apply: DisplayMode,
}

impl Rollback {
    fn should_restore(&self, current: &DisplayMode) -> bool {
        current != &self.original && (current == &self.applied || current == &self.before_apply)
    }
}

pub(crate) struct DisplayController {
    path: PathBuf,
    owner: Option<u64>,
    rollback: Option<Rollback>,
}

impl DisplayController {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            rollback: None,
            owner: None,
        }
    }

    pub fn recover(&mut self, backend: &impl Backend) -> Result<(), String> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let mut data = Vec::new();
        file.take(8193)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() > 8192 {
            return Err("The saved display configuration is invalid.".into());
        }
        self.rollback = Some(deskbolt_proto::decode_bincode(&data).map_err(|e| e.to_string())?);
        self.restore_pending(backend)
    }

    pub fn set(
        &mut self,
        backend: &impl Backend,
        viewer: u64,
        monitor: &MonitorInfo,
        desired: (u16, u16, u16),
    ) -> Result<DisplayMode, String> {
        if self.owner.is_some_and(|owner| owner != viewer) {
            return Err("Another viewer is matching this host's display.".into());
        }
        if self.rollback.is_some() && self.owner.is_none() {
            self.restore_pending(backend)?;
        } else if self.rollback.is_none() && self.path.is_file() {
            self.recover(backend)?;
        }

        let (mut current, mut modes) = backend.modes(monitor)?;
        if self
            .rollback
            .as_ref()
            .is_some_and(|lease| lease.original.output != current.output)
        {
            self.restore_pending(backend)?;
            (current, modes) = backend.modes(monitor)?;
        }
        let target = best_mode(&modes, desired, current.hz)
            .ok_or("No supported display mode matches this window.")?;
        if target == &current {
            return Ok(current);
        }
        let original = self
            .rollback
            .as_ref()
            .map_or_else(|| current.clone(), |lease| lease.original.clone());
        let candidate = Rollback {
            original,
            applied: target.clone(),
            before_apply: current,
        };
        write_journal(&self.path, &candidate)?;
        self.owner = Some(viewer);
        self.rollback = Some(candidate);
        if let Err(error) = backend.apply(target) {
            let _ = self.restore_pending(backend);
            return Err(error);
        }
        Ok(target.clone())
    }

    pub fn release(&mut self, backend: &impl Backend, viewer: u64) -> Result<(), String> {
        if self.owner.is_some_and(|owner| owner != viewer) {
            return Err("Another viewer is matching this host's display.".into());
        }
        self.restore_pending(backend)
    }

    pub fn disconnected(&mut self, backend: &impl Backend, viewer: u64) -> Result<bool, String> {
        if self.owner != Some(viewer) {
            return Ok(false);
        }
        self.owner = None;
        self.restore_pending(backend)?;
        Ok(true)
    }

    pub fn shutdown(&mut self, backend: &impl Backend) -> Result<(), String> {
        self.restore_pending(backend)
    }

    fn restore_pending(&mut self, backend: &impl Backend) -> Result<(), String> {
        if let Some(lease) = self.rollback.as_ref() {
            let current = backend.current(&lease.original.output)?;
            if lease.should_restore(&current) {
                backend.apply(&lease.original)?;
            }
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            self.rollback = None;
        }
        self.owner = None;
        Ok(())
    }
}

pub(crate) fn recovery_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let session =
            unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
        if let Some(home) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(home)
                .join("DeskBolt")
                .join(format!("display-{session}.bin"));
        }
        return crate::config::default_data_dir().join(format!("display-{session}.bin"));
    }
    #[cfg(target_os = "linux")]
    {
        let display: String = std::env::var("DISPLAY")
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
            .take(80)
            .collect();
        return crate::config::default_data_dir().join(format!("display-{display}.bin"));
    }
    #[cfg(any(target_os = "macos", target_os = "android"))]
    crate::config::default_data_dir().join("display-mode.bin")
}

fn write_journal(path: &Path, rollback: &Rollback) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Display recovery storage is unavailable.")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    let encoded = deskbolt_proto::encode_bincode(rollback).map_err(|e| e.to_string())?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(temporary, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn best_mode(
    modes: &[DisplayMode],
    (width, height, hz): (u16, u16, u16),
    current_hz: u32,
) -> Option<&DisplayMode> {
    if !(320..=16_384).contains(&width) || !(200..=16_384).contains(&height) || hz > 1000 {
        return None;
    }
    let w = u64::from(width);
    let h = u64::from(height);
    let hz = if hz == 0 { current_hz } else { u32::from(hz) };
    modes
        .iter()
        .filter(|mode| mode.width > 0 && mode.height > 0)
        .min_by_key(|mode| {
            let mw = u64::from(mode.width);
            let mh = u64::from(mode.height);
            let mismatch = (mw * h).abs_diff(mh * w) * 1000 / (mh * w);
            let dimensions = mw.abs_diff(w) + mh.abs_diff(h);
            (mismatch, dimensions, mode.hz.abs_diff(hz), mode.id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Display(RefCell<DisplayMode>, Vec<DisplayMode>);

    impl Backend for Display {
        fn modes(&self, _: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String> {
            Ok((self.0.borrow().clone(), self.1.clone()))
        }
        fn current(&self, _: &str) -> Result<DisplayMode, String> {
            Ok(self.0.borrow().clone())
        }
        fn apply(&self, mode: &DisplayMode) -> Result<(), String> {
            self.0.replace(mode.clone());
            Ok(())
        }
    }

    fn test_mode(width: u32, height: u32, hz: u32) -> DisplayMode {
        DisplayMode {
            output: "Display".into(),
            id: u64::from(width),
            width,
            height,
            hz,
        }
    }

    #[test]
    fn restores_original_after_repeated_changes_and_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("restore.bin");
        let original = test_mode(1920, 1080, 60);
        let matched = test_mode(1280, 720, 60);
        let second = test_mode(1600, 900, 60);
        let unrelated = test_mode(1024, 768, 60);
        let display = Display(
            RefCell::new(original.clone()),
            vec![
                original.clone(),
                matched.clone(),
                second.clone(),
                unrelated.clone(),
            ],
        );
        let monitor = MonitorInfo {
            id: 0,
            name: "Display".into(),
            is_primary: true,
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            dpi_scale: 1.0,
        };
        let mut controller = DisplayController::new(path.clone());
        assert_eq!(
            controller
                .set(&display, 1, &monitor, (1200, 675, 0))
                .unwrap(),
            matched
        );
        assert!(
            controller
                .set(&display, 2, &monitor, (1600, 900, 60))
                .is_err()
        );
        assert_eq!(
            controller
                .set(&display, 1, &monitor, (1600, 900, 0))
                .unwrap(),
            second
        );
        assert!(path.exists());
        // A crash after the journal update can leave either the old temporary or the new resolution active.
        display.apply(&matched).unwrap();
        drop(controller);
        let mut recovered = DisplayController::new(path.clone());
        recovered.recover(&display).unwrap();
        assert_eq!(display.current("").unwrap(), original);
        assert!(!path.exists());

        recovered
            .set(&display, 4, &monitor, (1280, 720, 60))
            .unwrap();
        display.apply(&unrelated).unwrap();
        recovered.disconnected(&display, 4).unwrap();
        assert_eq!(display.current("").unwrap(), unrelated);
        assert!(!path.exists());
    }
}
