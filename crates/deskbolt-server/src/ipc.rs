use std::sync::Arc;

use deskbolt_proto::MonitorInfo;
use tokio::sync::{broadcast, mpsc, watch};

#[cfg(target_os = "windows")]
use std::{ffi::c_void, os::windows::io::RawHandle, path::Path, time::Duration};
#[cfg(target_os = "windows")]
use deskbolt_proto::{
    ChannelId, FLAG_NONE, IpcMsg, pack_message, read_frame_async, unpack_message, write_frame_async,
};
#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
#[cfg(target_os = "windows")]
use tracing::{error, info, warn};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0};
#[cfg(target_os = "windows")]
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
#[cfg(target_os = "windows")]
use windows::Win32::Security::{
    DuplicateTokenEx, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SECURITY_IMPERSONATION_LEVEL,
    SetTokenInformation, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
#[cfg(target_os = "windows")]
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
#[cfg(target_os = "windows")]
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
    OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
#[cfg(target_os = "windows")]
use windows::core::{BOOL, PCWSTR, PWSTR};

#[cfg(target_os = "windows")]
use crate::win32_util::to_wide_null;

#[cfg(target_os = "windows")]
pub const DEFAULT_IPC_PIPE: &str = r"\\.\pipe\DeskBolt_IPC";

/// Shared bridge between TLS client sessions (`net.rs`) and either the Session-0 `--agent` supervisor
/// or the in-process capture engine.
#[derive(Clone)]
pub struct AgentHub {
    pub to_agent_tx: mpsc::Sender<deskbolt_proto::IpcMsg>,
    pub from_agent_tx: broadcast::Sender<Arc<deskbolt_proto::IpcMsg>>,
    pub monitors_rx: watch::Receiver<(Vec<MonitorInfo>, u8)>,
    pub viewer_wake_tx: mpsc::UnboundedSender<ViewerActivity>,
}

#[derive(Debug, Clone, Copy)]
pub enum ViewerActivity {
    Connected,
    Disconnected,
    SessionChanged(u32),
}

#[cfg(target_os = "windows")]
mod win_ipc {
    use super::*;

/// Builds a `SECURITY_DESCRIPTOR` from SDDL (`D:(A;;GA;;;SY)` for `SYSTEM` service mode).
pub fn create_pipe_server_with_sddl(
    pipe_name: &str,
    sddl: &str,
) -> Result<NamedPipeServer, String> {
    let pipe_w = to_wide_null(pipe_name);
    let sddl_w = to_wide_null(sddl);
    let mut sd = PSECURITY_DESCRIPTOR::default();

    // SAFETY: Converts valid SDDL string, creates overlapped named pipe with reject-remote-clients, frees SD, wraps handle in Tokio NamedPipeServer.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .map_err(|e| format!("ConvertStringSecurityDescriptor({sddl}): {e}"))?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: BOOL(0),
        };

        let open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        let handle = CreateNamedPipeW(
            PCWSTR(pipe_w.as_ptr()),
            open_mode,
            pipe_mode,
            1,
            1024 * 1024,
            1024 * 1024,
            0,
            Some(&sa),
        );
        let _ = LocalFree(Some(HLOCAL(sd.0)));

        if handle.is_invalid() {
            return Err(format!(
                "CreateNamedPipeW failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        NamedPipeServer::from_raw_handle(handle.0 as RawHandle).map_err(|e| {
            let _ = CloseHandle(handle);
            format!("NamedPipeServer::from_raw_handle: {e}")
        })
    }
}

/// Verifies that the connected client process ID on a named pipe matches `expected_pid`.
pub fn verify_pipe_client_pid(pipe: &NamedPipeServer, expected_pid: u32) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    let raw = pipe.as_raw_handle();
    let mut client_pid = 0u32;
    // SAFETY: `raw` is an open named pipe handle owned by `pipe`.
    unsafe {
        GetNamedPipeClientProcessId(HANDLE(raw), &mut client_pid)
            .map_err(|e| format!("GetNamedPipeClientProcessId: {e}"))?;
    }
    if client_pid != expected_pid {
        return Err(format!(
            "IPC client PID mismatch: expected {expected_pid}, got {client_pid}"
        ));
    }
    Ok(())
}

/// Connects to an IPC named pipe with bounded retry while `--agent` starts up.
pub async fn connect_ipc_client(pipe_name: &str) -> Result<NamedPipeClient, String> {
    for _ in 0..40 {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return Ok(client),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    Err(format!("timed out connecting to IPC pipe {pipe_name}"))
}

struct SendHandle(HANDLE);
// SAFETY: Win32 process kernel handles can be safely signaled/terminated/closed from supervisor tasks.
unsafe impl Send for SendHandle {}
// SAFETY: Win32 process kernel handles can be safely referenced across async executor threads.
unsafe impl Sync for SendHandle {}

struct SpawnedAgent {
    pid: u32,
    process_handle: SendHandle,
    session_id: u32,
}

impl SpawnedAgent {
    fn terminate_and_close(self) {
        // SAFETY: terminates and closes child process handle owned by supervisor.
        unsafe {
            let wait = WaitForSingleObject(self.process_handle.0, 800);
            if wait != WAIT_OBJECT_0 {
                let _ = TerminateProcess(self.process_handle.0, 1);
            }
            let _ = CloseHandle(self.process_handle.0);
        }
    }
}

/// Duplicates the Session-0 `SYSTEM` token into `target_session_id` and spawns `exe --agent`.
fn spawn_agent_in_console_session(
    exe_path: &Path,
    target_session_id: u32,
    pipe_name: &str,
) -> Result<SpawnedAgent, String> {
    // SAFETY: Standard Win32 Session-0 -> Console-Session SYSTEM token duplication sequence.
    unsafe {
        let mut self_token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY | TOKEN_ADJUST_SESSIONID,
            &mut self_token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;

        let mut dup_token = HANDLE::default();
        let dup_res = DuplicateTokenEx(
            self_token,
            windows::Win32::Security::TOKEN_ACCESS_MASK(0x0200_0000), // MAXIMUM_ALLOWED
            None,
            SECURITY_IMPERSONATION_LEVEL(2), // SecurityImpersonation
            TokenPrimary,
            &mut dup_token,
        );
        let _ = CloseHandle(self_token);
        dup_res.map_err(|e| format!("DuplicateTokenEx: {e}"))?;

        let session_bytes = target_session_id;
        if let Err(e) = SetTokenInformation(
            dup_token,
            TokenSessionId,
            (&session_bytes as *const u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        ) {
            let _ = CloseHandle(dup_token);
            return Err(format!("SetTokenInformation(TokenSessionId={target_session_id}): {e}"));
        }

        let mut env_block: *mut c_void = std::ptr::null_mut();
        let has_env = CreateEnvironmentBlock(&mut env_block, Some(dup_token), false).is_ok();

        let mut desktop_w = to_wide_null(r"WinSta0\Default");
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_w.as_mut_ptr()),
            ..Default::default()
        };
        let mut proc_info = PROCESS_INFORMATION::default();

        let cmd = format!("\"{}\" --agent --pipe \"{}\"", exe_path.display(), pipe_name);
        let mut cmd_w = to_wide_null(&cmd);
        let exe_w = to_wide_null(&exe_path.to_string_lossy());

        let mut flags = CREATE_NO_WINDOW;
        if has_env {
            flags |= CREATE_UNICODE_ENVIRONMENT;
        }

        let create_res = CreateProcessAsUserW(
            Some(dup_token),
            PCWSTR(exe_w.as_ptr()),
            Some(PWSTR(cmd_w.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            if has_env { Some(env_block) } else { None },
            PCWSTR::null(),
            &startup,
            &mut proc_info,
        );

        if has_env {
            let _ = DestroyEnvironmentBlock(env_block);
        }
        let _ = CloseHandle(dup_token);

        create_res.map_err(|e| format!("CreateProcessAsUserW(--agent): {e}"))?;
        let _ = CloseHandle(proc_info.hThread);

        Ok(SpawnedAgent {
            pid: proc_info.dwProcessId,
            process_handle: SendHandle(proc_info.hProcess),
            session_id: target_session_id,
        })
    }
}

/// Runs the Session-0 agent supervisor loop (lazy-spawning `--agent` on active console session).
pub async fn run_service_agent_supervisor(
    mut to_agent_rx: mpsc::Receiver<IpcMsg>,
    from_agent_tx: broadcast::Sender<Arc<IpcMsg>>,
    monitors_tx: watch::Sender<(Vec<MonitorInfo>, u8)>,
    mut activity_rx: mpsc::UnboundedReceiver<ViewerActivity>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            error!("cannot resolve current_exe for agent spawning: {e}");
            return;
        }
    };

    let mut active_viewers: usize = 0;
    let mut idle_deadline: Option<tokio::time::Instant> = None;
    let mut current_agent: Option<(SpawnedAgent, NamedPipeServer)> = None;
    let mut poll_tick = tokio::time::interval(Duration::from_millis(500));

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        // Handle pending viewer connects/disconnects/session-changes
        while let Ok(ev) = activity_rx.try_recv() {
            match ev {
                ViewerActivity::Connected => {
                    active_viewers += 1;
                    idle_deadline = None;
                }
                ViewerActivity::Disconnected => {
                    active_viewers = active_viewers.saturating_sub(1);
                    if active_viewers == 0 {
                        idle_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                    }
                }
                ViewerActivity::SessionChanged(new_sid) => {
                    info!(session_id = new_sid, "SCM session change notification");
                }
            }
        }

        // Check active console session & tear down or respawn if needed
        let console_sid = unsafe { WTSGetActiveConsoleSessionId() };
        let has_console = console_sid != u32::MAX;

        if let Some((ref agent, _)) = current_agent {
            let expired = idle_deadline.is_some_and(|d| tokio::time::Instant::now() >= d);
            if active_viewers == 0 && expired {
                info!("Stopping idle console agent");
                if let Some((old_agent, mut pipe)) = current_agent.take() {
                    if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_NONE, &IpcMsg::Shutdown, None) {
                        let _ = write_frame_async(&mut pipe, frame.channel, frame.flags, &frame.payload).await;
                    }
                    old_agent.terminate_and_close();
                }
            } else if !has_console || agent.session_id != console_sid {
                info!(old_sid = agent.session_id, new_sid = console_sid, "Console session changed; cycling agent");
                if let Some((old_agent, mut pipe)) = current_agent.take() {
                    if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_NONE, &IpcMsg::Shutdown, None) {
                        let _ = write_frame_async(&mut pipe, frame.channel, frame.flags, &frame.payload).await;
                    }
                    old_agent.terminate_and_close();
                }
            }
        }

        if active_viewers > 0 && current_agent.is_none() && has_console {
            match create_pipe_server_with_sddl(DEFAULT_IPC_PIPE, "D:(A;;GA;;;SY)") {
                Ok(pipe) => match spawn_agent_in_console_session(&exe_path, console_sid, DEFAULT_IPC_PIPE) {
                    Ok(spawned) => {
                        let connect_res = tokio::time::timeout(Duration::from_secs(4), pipe.connect()).await;
                        match connect_res {
                            Ok(Ok(())) => {
                                if let Err(e) = verify_pipe_client_pid(&pipe, spawned.pid) {
                                    warn!("{e}");
                                    spawned.terminate_and_close();
                                } else {
                                    info!(pid = spawned.pid, session = console_sid, "Console agent connected over IPC");
                                    current_agent = Some((spawned, pipe));
                                }
                            }
                            _ => {
                                warn!("Timed out waiting for --agent IPC connection");
                                spawned.terminate_and_close();
                            }
                        }
                    }
                    Err(e) => error!("Failed to spawn console agent: {e}"),
                },
                Err(e) => error!("Failed to create hardened IPC pipe: {e}"),
            }
        }

        if let Some((agent, pipe)) = current_agent.as_mut() {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                _ = poll_tick.tick() => {}
                Some(ev) = activity_rx.recv() => {
                    match ev {
                        ViewerActivity::Connected => { active_viewers += 1; idle_deadline = None; }
                        ViewerActivity::Disconnected => {
                            active_viewers = active_viewers.saturating_sub(1);
                            if active_viewers == 0 {
                                idle_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                            }
                        }
                        ViewerActivity::SessionChanged(_) => {}
                    }
                }
                Some(msg) = to_agent_rx.recv() => {
                    if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_NONE, &msg, None)
                        && let Err(e) = write_frame_async(pipe, frame.channel, frame.flags, &frame.payload).await
                    {
                        warn!("IPC write to agent failed: {e}");
                        let (old_agent, _) = current_agent.take().unwrap();
                        old_agent.terminate_and_close();
                    }
                }
                res = read_frame_async(pipe) => {
                    match res {
                        Ok(Some(frame)) => {
                            if let Ok(msg) = unpack_message::<IpcMsg>(&frame) {
                                match &msg {
                                    IpcMsg::AgentReady { monitors, active_monitor_id, .. }
                                    | IpcMsg::MonitorList { monitors, active_monitor_id } => {
                                        let _ = monitors_tx.send((monitors.clone(), *active_monitor_id));
                                    }
                                    _ => {}
                                }
                                let _ = from_agent_tx.send(Arc::new(msg));
                            }
                        }
                        Ok(None) | Err(_) => {
                            warn!(pid = agent.pid, "Console agent IPC disconnected");
                            let (old_agent, _) = current_agent.take().unwrap();
                            old_agent.terminate_and_close();
                        }
                    }
                }
            }
        } else {
            // Drain incoming agent commands while idle so channel never wedges
            while let Ok(msg) = to_agent_rx.try_recv() {
                let _ = msg;
            }
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                _ = poll_tick.tick() => {}
                Some(ev) = activity_rx.recv() => {
                    match ev {
                        ViewerActivity::Connected => { active_viewers += 1; idle_deadline = None; }
                        ViewerActivity::Disconnected => {
                            active_viewers = active_viewers.saturating_sub(1);
                            if active_viewers == 0 {
                                idle_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                            }
                        }
                        ViewerActivity::SessionChanged(_) => {}
                    }
                }
            }
        }
    }

    if let Some((old_agent, mut pipe)) = current_agent.take() {
        if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_NONE, &IpcMsg::Shutdown, None) {
            let _ = write_frame_async(&mut pipe, frame.channel, frame.flags, &frame.payload).await;
        }
        old_agent.terminate_and_close();
    }
}

#[cfg(test)]
mod tests {
    use deskbolt_proto::ControlMsg;
    use tokio::net::windows::named_pipe::ClientOptions;

    use super::*;

    #[tokio::test]
    async fn hardened_named_pipe_sddl_and_pid_verify() {
        let pipe_name = format!(r"\\.\pipe\DeskBolt_Test_{}", std::process::id());
        // Allow Authenticated Users (`AU`) in unit test context while enforcing `PIPE_REJECT_REMOTE_CLIENTS`
        let server = create_pipe_server_with_sddl(&pipe_name, "D:(A;;GA;;;AU)").unwrap();

        let connect_fut = server.connect();
        let client = ClientOptions::new().open(&pipe_name).unwrap();
        connect_fut.await.unwrap();

        verify_pipe_client_pid(&server, std::process::id()).unwrap();
        assert!(verify_pipe_client_pid(&server, std::process::id().wrapping_add(999)).is_err());

        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut sr, mut sw) = tokio::io::split(server);

        let frame = pack_message(
            ChannelId::AuthControl,
            FLAG_NONE,
            &ControlMsg::Ping { timestamp_ms: 42 },
            None,
        )
        .unwrap();
        write_frame_async(&mut cw, frame.channel, frame.flags, &frame.payload)
            .await
            .unwrap();
        let got = read_frame_async(&mut sr).await.unwrap().unwrap();
        let decoded: ControlMsg = unpack_message(&got).unwrap();
        assert_eq!(decoded, ControlMsg::Ping { timestamp_ms: 42 });

        write_frame_async(&mut sw, ChannelId::AuthControl, FLAG_NONE, &got.payload)
            .await
            .unwrap();
        let back = read_frame_async(&mut cr).await.unwrap().unwrap();
        assert_eq!(unpack_message::<ControlMsg>(&back).unwrap(), ControlMsg::Ping { timestamp_ms: 42 });
    }
}
}

#[cfg(target_os = "windows")]
pub use win_ipc::*;
