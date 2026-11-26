use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

static CURTAIN: Mutex<Option<Curtain>> = Mutex::new(None);

struct Curtain {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Curtain {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn set(enabled: bool) -> Result<(), String> {
    let mut curtain = CURTAIN.lock().unwrap();
    if !enabled {
        curtain.take();
        return Ok(());
    }
    if curtain.as_ref().is_some_and(|state| {
        state
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }) {
        return Ok(());
    }
    curtain.take();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let (ready, initialized) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let result = open_and_run(&worker_stop, &ready);
        if let Err(error) = result {
            let _ = ready.try_send(Err(error));
        }
    });
    match initialized.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => {
            *curtain = Some(Curtain {
                stop,
                thread: Some(worker),
            });
            Ok(())
        }
        Ok(Err(message)) => Err(message),
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err("The host did not open its privacy curtain.".into())
        }
    }
}

fn bounds() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE => {
            let (x, y, w, h) = bounds();
            unsafe {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn open_and_run(
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    crate::win32_util::bind_thread_to_active_input_desktop()?;
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class_name = w!("DeskBoltPrivacyWindow");
        let class = WNDCLASSW {
            hInstance: instance.into(),
            lpfnWndProc: Some(window_proc),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class_name,
            ..WNDCLASSW::default()
        };
        let _ = RegisterClassW(&class);
        let (x, y, width, height) = bounds();
        if width <= 0 || height <= 0 {
            return Err("There is no local display to blank.".into());
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            class_name,
            w!("Privacy Curtain"),
            WS_POPUP,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        if let Err(error) = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) {
            let _ = DestroyWindow(hwnd);
            return Err(format!("Could not configure the privacy curtain: {error}"));
        }
        if let Err(error) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
            let _ = DestroyWindow(hwnd);
            return Err(format!(
                "The desktop cannot exclude the privacy curtain from capture: {error}"
            ));
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if ready.send(Ok(())).is_ok() {
            let mut message = MSG::default();
            while !stop.load(Ordering::Relaxed) {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = DestroyWindow(hwnd);
        Ok(())
    }
}
