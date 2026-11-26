#[cfg(target_os = "windows")]
mod native {
    use windows::Win32::UI::Input::KeyboardAndMouse::BlockInput;

    pub(crate) struct LocalInputLock(std::marker::PhantomData<*const ()>);

    impl LocalInputLock {
        pub fn acquire() -> Result<Self, String> {
            unsafe { BlockInput(true) }
                .map_err(|error| format!("Could not lock local input: {error}"))?;
            Ok(Self(std::marker::PhantomData))
        }

        pub fn maintain(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    impl Drop for LocalInputLock {
        fn drop(&mut self) {
            // The agent acquires and releases the lock on its input injection thread.
            let _ = unsafe { BlockInput(false) };
        }
    }
}

#[cfg(target_os = "linux")]
mod native {
    use std::collections::HashMap;
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

    pub(crate) struct LocalInputLock {
        devices: HashMap<PathBuf, File>,
        next_scan: Instant,
    }

    impl LocalInputLock {
        pub fn acquire() -> Result<Self, String> {
            let mut lock = Self {
                devices: HashMap::new(),
                next_scan: Instant::now(),
            };
            lock.maintain()?;
            if lock.devices.is_empty() {
                return Err("No physical keyboard or mouse can be locked.".into());
            }
            Ok(lock)
        }

        pub fn maintain(&mut self) -> Result<(), String> {
            if Instant::now() < self.next_scan {
                return Ok(());
            }
            self.next_scan = Instant::now() + Duration::from_secs(1);
            let entries = fs::read_dir("/dev/input")
                .map_err(|error| format!("Could not access local input: {error}"))?;
            self.devices.retain(|path, handle| {
                handle
                    .metadata()
                    .ok()
                    .zip(path.metadata().ok())
                    .is_some_and(|(opened, current)| opened.ino() == current.ino())
            });
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if self.devices.contains_key(&path) || !physical_device(&path) {
                    continue;
                }
                let file = File::open(&path).map_err(|error| {
                    format!("Could not access a local keyboard or mouse: {error}")
                })?;
                if unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB, 1 as libc::c_ulong) } != 0 {
                    return Err(format!(
                        "Could not lock a local keyboard or mouse: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                self.devices.insert(path, file);
            }
            // Closing the file descriptors releases the grabs, including after a crash.
            Ok(())
        }
    }

    fn physical_device(path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if !name.starts_with("event") {
            return false;
        }
        let device = PathBuf::from("/sys/class/input").join(name).join("device");
        if !device
            .canonicalize()
            .is_ok_and(|target| !target.starts_with("/sys/devices/virtual"))
        {
            return false;
        }
        let Ok(keys) = fs::read_to_string(device.join("capabilities/key")) else {
            return false;
        };
        (has_key(&keys, 28) && has_key(&keys, 30)) || has_key(&keys, 272)
    }

    fn has_key(keys: &str, code: usize) -> bool {
        let bits = usize::BITS as usize;
        keys.split_whitespace()
            .rev()
            .nth(code / bits)
            .and_then(|word| usize::from_str_radix(word, 16).ok())
            .is_some_and(|word| word & (1 << (code % bits)) != 0)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn kernel_capability_words_are_read_in_reverse_order() {
            assert!(super::has_key("1 50000000", 28));
            assert!(super::has_key("1 50000000", 30));
            assert!(super::has_key("1 50000000", usize::BITS as usize));
            assert!(!super::has_key("1 50000000", 272));
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    type Ref = *const c_void;
    type EventCallback = unsafe extern "C" fn(Ref, u32, Ref, *mut c_void) -> Ref;

    // Left/right/other mouse press, release and drag, movement, keyboard, modifiers and wheel.
    const EVENT_MASK: u64 = (1 << 1)
        | (1 << 2)
        | (1 << 3)
        | (1 << 4)
        | (1 << 5)
        | (1 << 6)
        | (1 << 7)
        | (1 << 10)
        | (1 << 11)
        | (1 << 12)
        | (1 << 22)
        | (1 << 25)
        | (1 << 26)
        | (1 << 27);

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            location: u32,
            placement: u32,
            options: u32,
            mask: u64,
            callback: EventCallback,
            user_data: *mut c_void,
        ) -> Ref;
        fn CGEventTapEnable(tap: Ref, enabled: bool);
        fn CGEventTapIsEnabled(tap: Ref) -> bool;
        fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: Ref;
        fn CFRelease(value: Ref);
        fn CFMachPortInvalidate(port: Ref);
        fn CFMachPortCreateRunLoopSource(allocator: Ref, port: Ref, order: isize) -> Ref;
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
    }

    unsafe extern "C" fn filter(_: Ref, kind: u32, event: Ref, _: *mut c_void) -> Ref {
        if kind >= u32::MAX - 1 || event.is_null() {
            return event;
        }
        let sender = unsafe { CGEventGetIntegerValueField(event, 41) };
        if sender == i64::from(std::process::id()) {
            event
        } else {
            null()
        }
    }

    pub(crate) struct LocalInputLock {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    impl LocalInputLock {
        pub fn acquire() -> Result<Self, String> {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let (ready, initialized) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || unsafe {
                let tap = CGEventTapCreate(0, 0, 0, EVENT_MASK, filter, null_mut());
                if tap.is_null() {
                    let _ = ready.send(false);
                    return;
                }
                let source = CFMachPortCreateRunLoopSource(null(), tap, 0);
                if source.is_null() {
                    CFRelease(tap);
                    let _ = ready.send(false);
                    return;
                }
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
                CGEventTapEnable(tap, true);
                if ready.send(true).is_ok() {
                    while !worker_stop.load(Ordering::Relaxed) {
                        if !CGEventTapIsEnabled(tap) {
                            CGEventTapEnable(tap, true);
                        }
                        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, false);
                    }
                }
                CFMachPortInvalidate(tap);
                CFRelease(source);
                CFRelease(tap);
            });
            if initialized.recv_timeout(Duration::from_secs(2)) != Ok(true) {
                stop.store(true, Ordering::Relaxed);
                return Err("Could not lock local input. Check Accessibility permission.".into());
            }
            Ok(Self {
                stop,
                worker: Some(worker),
            })
        }

        pub fn maintain(&mut self) -> Result<(), String> {
            if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
                Err("The local input lock was interrupted.".into())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for LocalInputLock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(target_os = "android")]
mod native {
    pub(crate) struct LocalInputLock;

    impl LocalInputLock {
        pub fn acquire() -> Result<Self, String> {
            Err("Locking local input is unavailable on this host.".into())
        }

        pub fn maintain(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
}

pub(crate) use native::LocalInputLock;
