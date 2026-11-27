#[cfg(target_os = "windows")]
mod service;
#[cfg(target_os = "windows")]
mod win32_util;

pub mod agent;
pub mod config;
mod display_modes;
mod privacy;
pub mod files;
pub mod ipc;
#[cfg(all(feature = "dev-mcp", not(target_os = "android")))]
pub mod loopback;
pub mod net;
mod collaboration;
pub mod platform;
#[cfg(not(target_os = "android"))]
pub mod settings_ui;
pub mod sysadmin;
pub mod terminal;
pub mod tunnel;

use deskbolt_proto::format_fingerprint;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::error;
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "windows")]
use crate::agent::run_agent_process;
use crate::agent::run_inprocess_agent;
use crate::config::{
    ServerConfig, default_data_dir, load_or_generate_identity, parse_set_password_args,
    run_set_password_with,
};
use crate::ipc::AgentHub;
#[cfg(target_os = "windows")]
use crate::ipc::DEFAULT_IPC_PIPE;
use crate::net::run_tcp_server;
#[cfg(not(target_os = "macos"))]
use crate::platform::start_tray_icon;
use crate::platform::{
    install_service_with_code, run_service_dispatcher, startup_permission_check,
    uninstall_service_with_code,
};

pub fn cli_main() -> i32 {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--service") {
        if let Err(e) = run_service_dispatcher() {
            if args.is_empty() {
                print_usage();
                return 2;
            }
            error!("{e}");
            return 1;
        }
        return 0;
    }

    match args[0].as_str() {
        "--version" | "-V" => {
            println!("DeskBolt Host {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "--install" => report_code_result(install_service_with_code()),
        "--uninstall" => report_code_result(uninstall_service_with_code()),
        "--set-password" => match parse_set_password_args(&args[1..]) {
            Ok(opts) => report_result(run_set_password_with(
                &default_data_dir(),
                opts.source,
                opts.port,
            )),
            Err(e) => {
                eprintln!("Error: {e}");
                2
            }
        },
        "--regen-cert" => report_result(regen_cert()),
        "--settings" => run_settings_cli(),
        "--agent" => run_agent_cli(&args),
        "--console" => report_result(run_inprocess_server_mode()),
        "--help" | "-h" => {
            print_usage();
            0
        }
        _ => {
            eprintln!("Unknown option.");
            print_usage();
            2
        }
    }
}

#[cfg(not(target_os = "android"))]
fn run_settings_cli() -> i32 {
    report_result(settings_ui::run_settings_window(&default_data_dir()))
}

#[cfg(target_os = "android")]
fn run_settings_cli() -> i32 {
    eprintln!("--settings GUI is only available on desktop targets.");
    2
}

#[cfg(target_os = "windows")]
fn run_agent_cli(args: &[String]) -> i32 {
    let pipe = args
        .windows(2)
        .find(|w| w[0] == "--pipe")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| DEFAULT_IPC_PIPE.to_string());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    report_result(rt.block_on(run_agent_process(&pipe)))
}

#[cfg(not(target_os = "windows"))]
fn run_agent_cli(_args: &[String]) -> i32 {
    report_result(run_inprocess_server_mode())
}

fn report_result(res: Result<(), String>) -> i32 {
    report_code_result(res.map_err(|e| (1, e)))
}

fn report_code_result(res: Result<(), (i32, String)>) -> i32 {
    match res {
        Ok(()) => 0,
        Err((code, e)) => {
            eprintln!("Error: {e}");
            if code == 0 { 1 } else { code }
        }
    }
}

fn regen_cert() -> Result<(), String> {
    let data_dir = default_data_dir();
    let cfg = ServerConfig::load_or_default(&data_dir)?;
    let id = load_or_generate_identity(&cfg, true)?;
    println!(
        "Generated new TLS certificate at {}\nSHA-256 Fingerprint: {}",
        cfg.cert_path.display(),
        format_fingerprint(&id.fingerprint)
    );
    Ok(())
}

/// Runs the TLS 1.3 TCP server alongside an in-process capture & input agent.
pub fn run_inprocess_server_mode() -> Result<(), String> {
    startup_permission_check();
    let data_dir = default_data_dir();
    let cfg = ServerConfig::load_or_default(&data_dir)?;
    let identity = load_or_generate_identity(&cfg, false)?;
    println!(
        "DeskBolt Host v{} starting on {}:{}\nTLS SHA-256 Fingerprint: {}",
        env!("CARGO_PKG_VERSION"),
        cfg.bind_addr,
        cfg.port,
        format_fingerprint(&identity.fingerprint)
    );

    let show_tray = cfg.show_tray_icon;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio init: {e}"))?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    #[cfg(target_os = "macos")]
    let ui_channels = (shutdown_rx.clone(), shutdown_tx.clone());
    let run = async move {
        #[cfg(not(target_os = "macos"))]
        let tray_handle = show_tray
            .then(|| start_tray_icon(shutdown_rx.clone(), shutdown_tx.clone()))
            .flatten();
        let (to_agent_tx, to_agent_rx) = mpsc::channel(256);
        let (from_agent_tx, _) = broadcast::channel(32);
        let (monitors_tx, monitors_rx) = watch::channel((Vec::new(), 0));
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();

        let hub = AgentHub {
            to_agent_tx,
            from_agent_tx: from_agent_tx.clone(),
            monitors_rx,
            viewer_wake_tx: activity_tx,
        };

        let agent_task = tokio::spawn(run_inprocess_agent(
            to_agent_rx,
            from_agent_tx,
            monitors_tx,
            activity_rx,
            shutdown_rx.clone(),
        ));

        let ctrl_c_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = ctrl_c_shutdown.send(true);
        });

        let res = run_tcp_server(
            cfg,
            identity.cert_der,
            identity.key_der,
            hub,
            false,
            shutdown_rx,
        )
        .await;
        let _ = shutdown_tx.send(true);
        let _ = agent_task.await;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Err(error) = platform::set_curtain_mode(false) {
            tracing::warn!("Could not restore the screen: {error}");
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(h) = tray_handle {
            let _ = h.join();
        }
        res
    };
    #[cfg(target_os = "macos")]
    if show_tray && platform::macos::interactive_session() {
        let server = std::thread::spawn(move || rt.block_on(run));
        if let Err(error) = platform::macos::run_tray_icon(ui_channels.0, ui_channels.1) {
            tracing::warn!("Host menu unavailable: {error}");
        }
        return server.join().map_err(|_| "The host stopped unexpectedly.")?;
    }
    rt.block_on(run)
}

fn print_usage() {
    println!(
        "DeskBolt Host {}\n\
         Usage:\n\
         \tdeskbolt-server --version                  Print server version\n\
         \tdeskbolt-server --settings                 Open interactive Host Settings dialog\n\
         \tdeskbolt-server --install                  Install and register system service\n\
         \tdeskbolt-server --uninstall                Stop and remove system service\n\
         \tdeskbolt-server --set-password [options]   Configure password (--password-stdin | --password-env <var>, --port <port>)\n\
         \tdeskbolt-server --regen-cert               Regenerate host TLS identity certificate\n\
         \tdeskbolt-server --console                  Run interactively in foreground console\n\
         \tdeskbolt-server --service                  Run under OS service manager",
        env!("CARGO_PKG_VERSION")
    );
}
