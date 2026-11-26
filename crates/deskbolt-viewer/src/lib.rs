pub mod admin_ui;
pub mod comms_ui;
pub mod db;
pub mod hook;
pub mod net_tools;
pub mod oob;
pub mod session;
pub mod sftp_sync;
pub mod shortcuts;
pub mod terminal;
pub mod ui;

use deskbolt_proto::SessionMode;
use eframe::egui;
use tracing_subscriber::EnvFilter;

use crate::db::{PhonebookDb, default_phonebook_path};
use crate::shortcuts::parse_mode_arg;
use crate::ui::DeskBoltViewerApp;

#[derive(Debug, Clone)]
pub struct StartupConnectArgs {
    pub connect_id: String,
    pub mode_override: Option<SessionMode>,
}

pub fn parse_viewer_cli_args(args: &[String]) -> Option<StartupConnectArgs> {
    let mut connect_id: Option<String> = None;
    let mut mode_override: Option<SessionMode> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--connect-id" && i + 1 < args.len() {
            connect_id = Some(args[i + 1].clone());
            i += 2;
        } else if let Some(val) = args[i].strip_prefix("--connect-id=") {
            connect_id = Some(val.to_string());
            i += 1;
        } else if args[i] == "--mode" && i + 1 < args.len() {
            mode_override = Some(parse_mode_arg(&args[i + 1]));
            i += 2;
        } else if let Some(val) = args[i].strip_prefix("--mode=") {
            mode_override = Some(parse_mode_arg(val));
            i += 1;
        } else {
            i += 1;
        }
    }
    connect_id.map(|id| StartupConnectArgs {
        connect_id: id,
        mode_override,
    })
}

#[cfg(target_os = "android")]
static ANDROID_APP: std::sync::OnceLock<android_activity::AndroidApp> =
    std::sync::OnceLock::new();

#[cfg(target_os = "android")]
pub fn set_android_soft_keyboard(show: bool) {
    if let Some(app) = ANDROID_APP.get() {
        if show {
            app.show_soft_input(true);
        } else {
            app.hide_soft_input(false);
        }
    }
}

#[cfg(not(target_os = "android"))]
pub fn set_android_soft_keyboard(_show: bool) {}

fn build_viewer_app(
    db: PhonebookDb,
    rt_handle: tokio::runtime::Handle,
    startup: Option<StartupConnectArgs>,
) -> eframe::AppCreator<'static> {
    Box::new(move |_cc| Ok(Box::new(DeskBoltViewerApp::new_with_startup(db, rt_handle, startup))))
}

#[cfg(not(target_os = "android"))]
pub fn run_desktop() -> eframe::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let startup = parse_viewer_cli_args(&raw_args);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to initialize async runtime");

    let db_path = default_phonebook_path();
    let db = PhonebookDb::open(&db_path).expect("failed to open phonebook database");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DeskBolt Viewer")
            .with_inner_size([1020.0, 660.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "DeskBolt Viewer",
        options,
        build_viewer_app(db, rt.handle().clone(), startup),
    )
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init();

    if let Some(dir) = app.internal_data_path() {
        db::init_android_phonebook_dir(dir);
    }
    let _ = ANDROID_APP.set(app.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to initialize async runtime");

    let db_path = default_phonebook_path();
    let db = PhonebookDb::open(&db_path).expect("failed to open phonebook database");

    let options = eframe::NativeOptions {
        android_app: Some(app),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "DeskBolt Viewer",
        options,
        build_viewer_app(db, rt.handle().clone(), None),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_id_and_mode_cli_flags() {
        let a = parse_viewer_cli_args(&[
            "--connect-id".into(),
            "abc-123".into(),
            "--mode".into(),
            "view".into(),
        ])
        .unwrap();
        assert_eq!(a.connect_id, "abc-123");
        assert_eq!(a.mode_override, Some(SessionMode::ViewOnly));

        let b = parse_viewer_cli_args(&["--connect-id=xyz".into(), "--mode=files".into()]).unwrap();
        assert_eq!(b.connect_id, "xyz");
        assert_eq!(b.mode_override, Some(SessionMode::FileTransfer));

        assert!(parse_viewer_cli_args(&["--mode".into(), "full".into()]).is_none());
    }
}
