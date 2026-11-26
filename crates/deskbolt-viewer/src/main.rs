#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    #[cfg(not(target_os = "android"))]
    {
        deskbolt_viewer_lib::run_desktop()
    }
    #[cfg(target_os = "android")]
    {
        Ok(())
    }
}
