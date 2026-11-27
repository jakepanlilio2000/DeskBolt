use std::collections::HashMap;
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use deskbolt_proto::MonitorInfo;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

static WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

pub(super) fn start() -> Result<(), String> {
    let mut worker = WORKER.lock().unwrap();
    if worker.as_ref().is_some_and(|thread| !thread.is_finished()) {
        return Ok(());
    }
    let (ready, recv) = mpsc::sync_channel(1);
    let handle = thread::Builder::new().name("desktop-annotation".into()).spawn(move || {
        if let Err(error) = run(&ready) {
            let _ = ready.try_send(Err(error));
        }
    }).map_err(|error| error.to_string())?;
    recv.recv_timeout(Duration::from_secs(1)).map_err(|_| "The desktop did not respond.")??;
    *worker = Some(handle);
    Ok(())
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        unsafe { let _ = DestroyWindow(self.0); }
    }
}

fn run(ready: &mpsc::SyncSender<Result<(), String>>) -> Result<(), String> {
    crate::win32_util::bind_thread_to_active_input_desktop()?;
    let instance = unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        GetModuleHandleW(None)
    }.map_err(|error| error.to_string())?;
    let class_name = w!("DeskBoltAnnotationWindow");
    let class = WNDCLASSW {
        hInstance: instance.into(), lpfnWndProc: Some(window_proc), lpszClassName: class_name,
        ..WNDCLASSW::default()
    };
    unsafe { RegisterClassW(&class); }
    ready.send(Ok(())).map_err(|error| error.to_string())?;
    let mut last = 0;
    let mut windows = HashMap::<u8, Window>::new();
    let mut idle = 0;
    let mut message = MSG::default();
    loop {
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if let Some(scene) = super::snapshot(&mut last) {
            let displays = scene.displays();
            windows.retain(|id, _| displays.iter().any(|monitor| monitor.id == *id));
            for monitor in displays {
                let Some(image) = scene.render(monitor.id, monitor.width, monitor.height) else { continue };
                if let std::collections::hash_map::Entry::Vacant(entry) = windows.entry(monitor.id) {
                    let hwnd = unsafe {
                        CreateWindowExW(
                            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
                            class_name, w!("Session Annotation"), WS_POPUP, monitor.left, monitor.top,
                            monitor.width as i32, monitor.height as i32, None, None, Some(instance.into()), None,
                        )
                    }.map_err(|error| error.to_string())?;
                    unsafe { let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
                    entry.insert(Window(hwnd));
                }
                present(windows[&monitor.id].0, monitor, image).map_err(|error| error.to_string())?;
            }
        }
        idle = if windows.is_empty() { idle + 1 } else { 0 };
        if idle > 200 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn present(hwnd: HWND, monitor: &MonitorInfo, mut image: tiny_skia::Pixmap) -> windows::core::Result<()> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: image.width() as i32, biHeight: -(image.height() as i32), biPlanes: 1,
            biBitCount: 32, biCompression: BI_RGB.0, ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    for pixel in image.data_mut().as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    unsafe {
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(bitmap.into());
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "Could not open the display drawing context."));
        }
        std::ptr::copy_nonoverlapping(image.data().as_ptr(), bits.cast::<u8>(), image.data().len());
        let prev = SelectObject(dc, bitmap.into());
        let size = SIZE { cx: info.bmiHeader.biWidth, cy: -info.bmiHeader.biHeight };
        let position = POINT { x: monitor.left, y: monitor.top };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let result = UpdateLayeredWindow(hwnd, None, Some(&position), Some(&size), Some(dc), Some(&POINT::default()),
            COLORREF(0), Some(&blend), ULW_ALPHA);
        let _ = SelectObject(dc, prev);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(bitmap.into());
        result
    }
}
