use std::path::{Path, PathBuf};

use deskbolt_proto::SessionMode;

use crate::db::HostRow;

pub fn mode_cli_name(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::FullControl => "full",
        SessionMode::ViewOnly => "view",
        SessionMode::FileTransfer => "files",
    }
}

pub fn parse_mode_arg(raw: &str) -> SessionMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "view" | "viewonly" | "view-only" | "view_only" | "1" => SessionMode::ViewOnly,
        "files" | "filetransfer" | "file-transfer" | "file_transfer" | "2" => {
            SessionMode::FileTransfer
        }
        _ => SessionMode::FullControl,
    }
}

pub fn sanitize_shortcut_filename(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim_matches(['.', ' ']);
    if t.is_empty() {
        "DeskBoltConnection".into()
    } else {
        t.to_string()
    }
}

pub fn desktop_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        if let Some(up) = std::env::var_os("USERPROFILE") {
            let p = PathBuf::from(up).join("Desktop");
            if p.exists() {
                return Ok(p);
            }
            let od = PathBuf::from(std::env::var_os("USERPROFILE").unwrap())
                .join("OneDrive")
                .join("Desktop");
            if od.exists() {
                return Ok(od);
            }
            return Ok(p);
        }
        Err("Unable to locate user Desktop directory (%USERPROFILE%).".into())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DESKTOP_DIR") {
            let p = PathBuf::from(xdg);
            if p.exists() {
                return Ok(p);
            }
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| "Unable to locate Desktop directory ($HOME unset).".to_string())?;
        Ok(PathBuf::from(home).join("Desktop"))
    }
    #[cfg(target_os = "android")]
    {
        Err("Desktop shortcuts are not supported on Android.".into())
    }
}

/// Generates a native OS desktop shortcut launching `deskbolt-viewer --connect-id <uuid> --mode <mode>`.
pub fn create_desktop_shortcut(host: &HostRow, mode: SessionMode) -> Result<PathBuf, String> {
    let desk = desktop_dir()?;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let stem = sanitize_shortcut_filename(&format!("{} ({})", host.name, mode_cli_name(mode)));
    let args = format!("--connect-id {} --mode {}", host.uuid, mode_cli_name(mode));
    create_desktop_shortcut_in_dir(&desk, &exe, &stem, &host.name, &args)
}

pub fn build_linux_desktop_entry(display_name: &str, exe: &Path, args: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=DeskBolt - {display_name}\n\
         Exec=\"{}\" {args}\n\
         Icon=deskbolt\n\
         Terminal=false\n\
         Categories=Network;RemoteAccess;\n",
        exe.display()
    )
}

pub fn build_macos_command_script(exe: &Path, args: &str) -> String {
    format!("#!/bin/sh\nexec \"{}\" {args} \"$@\"\n", exe.display())
}

fn create_desktop_shortcut_in_dir(
    dir: &Path,
    exe: &Path,
    stem: &str,
    display_name: &str,
    args: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("create shortcut directory {}: {e}", dir.display()))?;

    #[cfg(target_os = "windows")]
    {
        let _ = display_name;
        let out = dir.join(format!("{stem}.lnk"));
        create_windows_lnk(&out, exe, args, &format!("Connect to {}", display_name))?;
        Ok(out)
    }

    #[cfg(target_os = "linux")]
    {
        let out = dir.join(format!("{stem}.desktop"));
        let content = build_linux_desktop_entry(display_name, exe, args);
        std::fs::write(&out, content)
            .map_err(|e| format!("write {}: {e}", out.display()))?;
        Ok(out)
    }

    #[cfg(target_os = "macos")]
    {
        let _ = display_name;
        let out = dir.join(format!("{stem}.command"));
        let content = build_macos_command_script(exe, args);
        std::fs::write(&out, content)
            .map_err(|e| format!("write {}: {e}", out.display()))?;
        let c = std::ffi::CString::new(out.to_string_lossy().as_bytes())
            .map_err(|_| "invalid path".to_string())?;
        // SAFETY: marks generated shell command shortcut executable.
        unsafe { libc::chmod(c.as_ptr(), 0o755) };
        Ok(out)
    }

    #[cfg(target_os = "android")]
    {
        let _ = (exe, stem, display_name, args);
        Err("Desktop shortcuts are not supported on Android.".into())
    }
}

#[cfg(target_os = "windows")]
fn create_windows_lnk(
    lnk_path: &Path,
    target_exe: &Path,
    args: &str,
    description: &str,
) -> Result<(), String> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{Interface, PCWSTR};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // SAFETY: Standard Win32 ShellLink COM shortcut creation & IPersistFile save.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("CoCreateInstance(ShellLink): {e}"))?;

        let exe_w = wide(&target_exe.to_string_lossy());
        link.SetPath(PCWSTR(exe_w.as_ptr()))
            .map_err(|e| format!("IShellLinkW::SetPath: {e}"))?;

        let args_w = wide(args);
        link.SetArguments(PCWSTR(args_w.as_ptr()))
            .map_err(|e| format!("IShellLinkW::SetArguments: {e}"))?;

        let desc_w = wide(description);
        link.SetDescription(PCWSTR(desc_w.as_ptr()))
            .map_err(|e| format!("IShellLinkW::SetDescription: {e}"))?;

        if let Some(work_dir) = target_exe.parent() {
            let wd_w = wide(&work_dir.to_string_lossy());
            let _ = link.SetWorkingDirectory(PCWSTR(wd_w.as_ptr()));
        }

        let pf: IPersistFile = link
            .cast()
            .map_err(|e| format!("QueryInterface(IPersistFile): {e}"))?;
        let out_w = wide(&lnk_path.to_string_lossy());
        pf.Save(PCWSTR(out_w.as_ptr()), true)
            .map_err(|e| format!("IPersistFile::Save({}): {e}", lnk_path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_helpers_sanitize_parse_mode_and_emit_files() {
        assert_eq!(sanitize_shortcut_filename("Prod:Web/01*"), "Prod_Web_01_");
        assert_eq!(sanitize_shortcut_filename("   "), "DeskBoltConnection");
        assert_eq!(parse_mode_arg("full"), SessionMode::FullControl);
        assert_eq!(parse_mode_arg("view"), SessionMode::ViewOnly);
        assert_eq!(parse_mode_arg("files"), SessionMode::FileTransfer);

        let desktop = build_linux_desktop_entry(
            "App01",
            Path::new("/usr/bin/deskbolt-viewer"),
            "--connect-id abc-123 --mode full",
        );
        assert!(desktop.contains("Exec=\"/usr/bin/deskbolt-viewer\" --connect-id abc-123 --mode full"));

        let cmd = build_macos_command_script(
            Path::new("/Applications/DeskBolt Viewer.app/Contents/MacOS/deskbolt-viewer"),
            "--connect-id abc-123 --mode view",
        );
        assert!(cmd.contains("--connect-id abc-123 --mode view"));

        #[cfg(target_os = "windows")]
        {
            let tmp = std::env::temp_dir().join(format!("deskbolt_lnk_test_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let lnk = create_desktop_shortcut_in_dir(
                &tmp,
                &std::env::current_exe().unwrap(),
                "TestHost",
                "TestHost",
                "--connect-id 11111111-2222-4333-8444-555555555555 --mode full",
            )
            .unwrap();
            assert!(lnk.exists());
            assert!(std::fs::metadata(&lnk).unwrap().len() > 64);
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
}
