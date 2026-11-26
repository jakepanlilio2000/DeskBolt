use std::ffi::OsString;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{error, info};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::config::{ServerConfig, default_data_dir, load_or_generate_identity};
use crate::ipc::{AgentHub, ViewerActivity, run_service_agent_supervisor};
use crate::net::run_tcp_server;
use crate::win32_util::enable_software_sas_policy;

pub const SERVICE_NAME: &str = "DeskBoltSvc";
pub const SERVICE_DISPLAY_NAME: &str = "DeskBolt Remote Host";

define_windows_service!(ffi_service_main, service_main);

/// Dispatches `DeskBoltSvc` under the Windows Service Control Manager (SCM).
pub fn run_service_dispatcher() -> Result<(), String> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| format!("SCM service dispatcher failed: {e}"))
}

fn service_main(_arguments: Vec<OsString>) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (activity_tx, activity_rx) = mpsc::unbounded_channel::<ViewerActivity>();
    let event_activity_tx = activity_tx.clone();

    let event_handler = move |control_event: ServiceControl| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(param) => {
                let _ = event_activity_tx.send(ViewerActivity::SessionChanged(param.notification.session_id));
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(h) => h,
        Err(e) => {
            error!("service_control_handler::register failed: {e}");
            return;
        }
    };

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime init");

    rt.block_on(async move {
        let data_dir = default_data_dir();
        let cfg = match ServerConfig::load_or_default(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                error!("config load failed: {e}");
                return;
            }
        };
        let identity = match load_or_generate_identity(&cfg, false) {
            Ok(id) => id,
            Err(e) => {
                error!("identity load failed: {e}");
                return;
            }
        };

        let (to_agent_tx, to_agent_rx) = mpsc::channel(256);
        let (from_agent_tx, _) = broadcast::channel(32);
        let (monitors_tx, monitors_rx) = watch::channel((Vec::new(), 0));

        let hub = AgentHub {
            to_agent_tx,
            from_agent_tx: from_agent_tx.clone(),
            monitors_rx,
            viewer_wake_tx: activity_tx.clone(),
        };

        let sup_shutdown = shutdown_rx.clone();
        let sup_task = tokio::spawn(run_service_agent_supervisor(
            to_agent_rx,
            from_agent_tx,
            monitors_tx,
            activity_rx,
            sup_shutdown,
        ));

        if let Err(e) = run_tcp_server(
            cfg,
            identity.cert_der,
            identity.key_der,
            hub,
            true,
            shutdown_rx,
        )
        .await
        {
            error!("TCP server exited with error: {e}");
        }

        let _ = sup_task.await;
    });

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}

const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_EXISTS: i32 = 1073;

/// Maps `windows-service` errors to standard Win32 exit codes for installer automation.
pub fn win32_error_code(err: &windows_service::Error) -> i32 {
    match err {
        windows_service::Error::Winapi(io_err) => io_err.raw_os_error().unwrap_or(1),
        _ => 1,
    }
}

/// Registers or updates `DeskBoltSvc` with Windows SCM (`LocalSystem`, Automatic startup, `--service`),
/// returning standard Win32 exit codes on failure.
pub fn install_service_with_code() -> Result<(), (i32, String)> {
    let exe_path = std::env::current_exe().map_err(|e| (1, format!("current_exe: {e}")))?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| {
        (
            win32_error_code(&e),
            format!("open SCM failed (run as Administrator): {e}"),
        )
    })?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let (service, updated) = match manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
    {
        Ok(svc) => (svc, false),
        Err(err) if win32_error_code(&err) == ERROR_SERVICE_EXISTS => {
            let existing = manager
                .open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)
                .map_err(|e| (win32_error_code(&e), format!("open_service failed: {e}")))?;
            existing
                .change_config(&info)
                .map_err(|e| (win32_error_code(&e), format!("change_config failed: {e}")))?;
            (existing, true)
        }
        Err(err) => {
            return Err((
                win32_error_code(&err),
                format!("create_service failed: {err}"),
            ));
        }
    };

    let _ = service.set_description("DeskBolt self-hosted remote desktop host service.");
    if let Err(e) = enable_software_sas_policy() {
        info!("Note: could not configure SoftwareSASGeneration policy ({e})");
    }
    if updated {
        println!("Service '{SERVICE_NAME}' updated successfully.");
    } else {
        println!("Service '{SERVICE_NAME}' installed successfully.");
    }
    Ok(())
}

/// Stops (waiting up to 10s for binary lock release) and removes `DeskBoltSvc` from Windows SCM,
/// returning standard Win32 exit codes on failure (`1060` when absent, `5` when access denied).
pub fn uninstall_service_with_code() -> Result<(), (i32, String)> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| {
            (
                win32_error_code(&e),
                format!("open SCM failed (run as Administrator): {e}"),
            )
        })?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| {
            let code = win32_error_code(&e);
            if code == ERROR_SERVICE_DOES_NOT_EXIST {
                (code, format!("service '{SERVICE_NAME}' is not installed"))
            } else {
                (code, format!("open_service({SERVICE_NAME}): {e}"))
            }
        })?;

    if let Ok(status) = service.query_status()
        && status.current_state != ServiceState::Stopped
    {
        if status.current_state != ServiceState::StopPending {
            let _ = service.stop();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match service.query_status() {
                Ok(s) if s.current_state == ServiceState::Stopped => break,
                Err(_) => break,
                _ => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }

    service
        .delete()
        .map_err(|e| (win32_error_code(&e), format!("delete service failed: {e}")))?;
    println!("Service '{SERVICE_NAME}' uninstalled.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_winapi_error_codes_for_automation() {
        let access_denied =
            windows_service::Error::Winapi(std::io::Error::from_raw_os_error(5));
        assert_eq!(win32_error_code(&access_denied), 5);

        let absent =
            windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1060));
        assert_eq!(win32_error_code(&absent), 1060);

        let exists =
            windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1073));
        assert_eq!(win32_error_code(&exists), 1073);
    }
}
