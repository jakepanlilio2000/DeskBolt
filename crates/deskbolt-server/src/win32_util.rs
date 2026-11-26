use std::ffi::c_void;
use std::io::{self, BufRead};
use std::mem;
use std::path::Path;
use std::ptr;

use deskbolt_proto::MonitorInfo;
use windows::Win32::Foundation::{GENERIC_ALL, HLOCAL, LPARAM, LocalFree, RECT};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{HDC, HMONITOR, MONITORINFOEXW, GetMonitorInfoW, MonitorFromRect, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PSECURITY_DESCRIPTOR, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_ELEVATION, TOKEN_QUERY,
    TokenElevation,
};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};
use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW, GetProcAddress};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, RegCreateKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEMENU,
    DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, GetThreadDesktop, GetUserObjectInformationW,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThreadId, OpenProcessToken};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::core::{BOOL, PCWSTR, PWSTR, s, w};

pub fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Seals bytes with machine-scoped Windows DPAPI (`CRYPTPROTECT_LOCAL_MACHINE`).
pub fn dpapi_protect_machine(plain: &[u8]) -> Result<Vec<u8>, String> {
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: valid input slice & output blob pointer; freed with LocalFree below.
    unsafe {
        CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE,
            &mut out_blob,
        )
        .map_err(|e| format!("CryptProtectData: {e}"))?;
        let slice = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let result = slice.to_vec();
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData.cast())));
        Ok(result)
    }
}

/// Unseals machine-scoped DPAPI ciphertext.
pub fn dpapi_unprotect_machine(cipher: &[u8]) -> Result<Vec<u8>, String> {
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: cipher.len() as u32,
        pbData: cipher.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: valid input slice & output blob pointer; freed with LocalFree below.
    unsafe {
        CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            0,
            &mut out_blob,
        )
        .map_err(|e| format!("CryptUnprotectData: {e}"))?;
        let slice = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let result = slice.to_vec();
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData.cast())));
        Ok(result)
    }
}

/// Returns `true` when running elevated (Administrator / SYSTEM), where restricting DACLs to `SY`+`BA` keeps read access.
pub fn is_process_elevated() -> bool {
    // SAFETY: Queries TokenElevation on current process token.
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elev as *mut TOKEN_ELEVATION).cast::<c_void>()),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// Sets protected NTFS DACL granting Full Control strictly to `SYSTEM` and `Administrators`
/// when running elevated/SYSTEM. Skips stripping user access in non-elevated dev/test tokens.
pub fn set_restrictive_acl(path: &Path) -> Result<(), String> {
    if !is_process_elevated() {
        return Ok(());
    }
    let sddl = w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)");
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL string is valid static UTF-16; freed via LocalFree after SetNamedSecurityInfoW.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .map_err(|e| format!("ConvertStringSecurityDescriptor: {e}"))?;

        let mut dacl_present = BOOL::default();
        let mut dacl_defaulted = BOOL::default();
        let mut dacl: *mut ACL = ptr::null_mut();
        let dacl_res = GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted);

        let path_w = to_wide_null(&path.to_string_lossy());
        let apply_res = if dacl_res.is_ok() && dacl_present.as_bool() {
            SetNamedSecurityInfoW(
                PWSTR(path_w.as_ptr() as *mut u16),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            )
        } else {
            windows::Win32::Foundation::ERROR_INVALID_SECURITY_DESCR
        };

        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if apply_res.0 != 0 {
            return Err(format!("SetNamedSecurityInfoW failed: code {}", apply_res.0));
        }
    }
    Ok(())
}

/// Reads a line from stdin with console echo temporarily disabled (RAII restored).
pub fn read_password_masked() -> Result<String, String> {
    struct EchoGuard {
        handle: windows::Win32::Foundation::HANDLE,
        prev_mode: CONSOLE_MODE,
    }
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            // SAFETY: restores original console mode on scope exit.
            unsafe {
                let _ = SetConsoleMode(self.handle, self.prev_mode);
            }
        }
    }

    // SAFETY: queries standard input console handle and clears ENABLE_ECHO_INPUT if attached to a console.
    let _guard = unsafe {
        match GetStdHandle(STD_INPUT_HANDLE) {
            Ok(h) => {
                let mut mode = CONSOLE_MODE::default();
                if GetConsoleMode(h, &mut mode).is_ok() {
                    let masked = CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0);
                    let _ = SetConsoleMode(h, masked);
                    Some(EchoGuard {
                        handle: h,
                        prev_mode: mode,
                    })
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    };

    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("failed to read password: {e}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Ensures `SoftwareSASGeneration` policy allows Windows Services to invoke `SendSAS(FALSE)`.
pub fn enable_software_sas_policy() -> Result<(), String> {
    let subkey = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
    let value_name = w!("SoftwareSASGeneration");
    // SAFETY: Registry API calls against HKLM\...\Policies\System with bounded DWORD buffers.
    unsafe {
        let mut hkey = Default::default();
        let status = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if status.0 != 0 {
            return Err(format!("RegCreateKeyExW failed: {}", status.0));
        }
        let mut cur_val: u32 = 0;
        let mut size = mem::size_of::<u32>() as u32;
        let q = RegQueryValueExW(
            hkey,
            value_name,
            None,
            None,
            Some((&mut cur_val as *mut u32).cast()),
            Some(&mut size),
        );
        if q.0 != 0 || cur_val == 0 {
            let val: u32 = 1; // 1 = Services allowed to generate SAS
            let bytes = val.to_le_bytes();
            let s = RegSetValueExW(hkey, value_name, None, REG_DWORD, Some(&bytes));
            let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
            if s.0 != 0 {
                return Err(format!("RegSetValueExW SoftwareSASGeneration failed: {}", s.0));
            }
        } else {
            let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
        }
    }
    Ok(())
}

/// Invokes `SendSAS(FALSE)` from `System32\sas.dll` (must run in `NT AUTHORITY\SYSTEM` service).
pub fn trigger_send_sas() -> Result<(), String> {
    type SendSasFn = unsafe extern "system" fn(BOOL);
    // SAFETY: Loads system sas.dll with LOAD_LIBRARY_SEARCH_SYSTEM32 and calls SendSAS(FALSE).
    unsafe {
        let module = LoadLibraryExW(w!("sas.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("LoadLibraryExW(sas.dll): {e}"))?;
        let proc = GetProcAddress(module, s!("SendSAS"))
            .ok_or_else(|| "SendSAS symbol not found in sas.dll".to_string())?;
        let send_sas: SendSasFn = mem::transmute(proc);
        send_sas(BOOL(0));
    }
    Ok(())
}

/// Binds the calling OS thread to whichever interactive desktop currently holds user input
/// (`WinSta0\Default` or `WinSta0\Winlogon`) and returns its desktop name.
pub fn bind_thread_to_active_input_desktop() -> Result<String, String> {
    let desired = DESKTOP_ACCESS_FLAGS(
        DESKTOP_READOBJECTS.0
            | DESKTOP_WRITEOBJECTS.0
            | DESKTOP_CREATEWINDOW.0
            | DESKTOP_CREATEMENU.0
            | DESKTOP_HOOKCONTROL.0
            | DESKTOP_ENUMERATE.0
            | DESKTOP_SWITCHDESKTOP.0
            | GENERIC_ALL.0,
    );
    // SAFETY: Opens active input desktop, inspects its name, switches current thread desktop, and closes previous handle.
    unsafe {
        let new_desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, desired)
            .map_err(|e| format!("OpenInputDesktop failed: {e}"))?;
        let name = read_desktop_name(new_desk).unwrap_or_else(|_| "Unknown".to_string());
        let old_desk = GetThreadDesktop(GetCurrentThreadId()).ok();
        if let Err(e) = SetThreadDesktop(new_desk) {
            let _ = CloseDesktop(new_desk);
            return Err(format!("SetThreadDesktop({name}) failed: {e}"));
        }
        if let Some(prev) = old_desk
            && prev != new_desk
        {
            let _ = CloseDesktop(prev);
        }
        Ok(name)
    }
}

/// Reads the name of an `HDESK` handle (`Default`, `Winlogon`, `Disconnect`, etc.).
unsafe fn read_desktop_name(desk: windows::Win32::System::StationsAndDesktops::HDESK) -> Result<String, String> {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    // SAFETY: caller guarantees `desk` is valid.
    unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .map_err(|e| format!("GetUserObjectInformationW: {e}"))?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// Enumerates attached displays in exact DXGI output duplication index order (`0, 1, ...`).
pub fn enumerate_dxgi_monitors() -> Vec<MonitorInfo> {
    let mut monitors = Vec::new();
    // SAFETY: Enumerates DXGI adapters/outputs and inspects Win32 monitor coordinates/DPI.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return monitors;
        };
        let mut idx: u8 = 0;
        let mut adapter_idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
            let mut out_idx = 0u32;
            while let Ok(output) = adapter.EnumOutputs(out_idx) {
                if let Ok(desc) = output.GetDesc()
                    && desc.AttachedToDesktop.as_bool()
                {
                    let r = desc.DesktopCoordinates;
                    let width = (r.right - r.left).max(0) as u32;
                    let height = (r.bottom - r.top).max(0) as u32;
                    let hmon: HMONITOR = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
                    let is_primary = is_primary_monitor(hmon, r);
                    let dpi_scale = monitor_dpi_scale(hmon);
                    let raw_name = &desc.DeviceName;
                    let end = raw_name.iter().position(|&c| c == 0).unwrap_or(raw_name.len());
                    let name = String::from_utf16_lossy(&raw_name[..end]);
                    monitors.push(MonitorInfo {
                        id: idx,
                        name,
                        is_primary,
                        left: r.left,
                        top: r.top,
                        width,
                        height,
                        dpi_scale,
                    });
                    idx = idx.saturating_add(1);
                }
                out_idx += 1;
            }
            adapter_idx += 1;
        }
    }
    monitors
}

unsafe fn is_primary_monitor(hmon: HMONITOR, rect: RECT) -> bool {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: valid MONITORINFOEXW pointer.
    if unsafe { GetMonitorInfoW(hmon, (&mut info.monitorInfo) as *mut _).as_bool() } {
        return (info.monitorInfo.dwFlags & 1) != 0; // MONITORINFOF_PRIMARY == 1
    }
    rect.left == 0 && rect.top == 0
}

unsafe fn monitor_dpi_scale(hmon: HMONITOR) -> f32 {
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // SAFETY: valid HMONITOR and output pointers.
    if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
        && dpi_x > 0
    {
        return (dpi_x as f32) / 96.0;
    }
    1.0
}

#[allow(dead_code)]
fn _unused_lparam_check(_p: LPARAM, _h: HDC) {}
