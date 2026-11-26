use deskbolt_proto::{
    DiskStat, ProcessStat, ServiceOp, ServiceStat, SysAdminMsg, SystemSnapshot,
};
#[cfg(not(target_os = "android"))]
use deskbolt_proto::ServiceStateKind;
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, Pid, ProcessesToUpdate, RefreshKind, System};

#[cfg(not(target_os = "android"))]
use std::{io::Read, time::Duration};

const MAX_SNAPSHOT_PROCESSES: usize = 150;
const MAX_SNAPSHOT_SERVICES: usize = 300;
#[cfg(not(target_os = "android"))]
const BROADCAST_TIMEOUT: Duration = Duration::from_secs(15);

/// Headless host diagnostics, process/service actions, and broadcast command runner (`0x08`).
pub struct SysAdminHandler {
    sys: System,
    disks: Disks,
}

impl Default for SysAdminHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl SysAdminHandler {
    pub fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        sys.refresh_cpu_usage();
        let disks = Disks::new_with_refreshed_list();
        Self { sys, disks }
    }

    pub fn snapshot(&mut self) -> SysAdminMsg {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        self.disks.refresh(true);

        let disks = self
            .disks
            .list()
            .iter()
            .map(|d| DiskStat {
                name: d.name().to_string_lossy().into_owned(),
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                fs_type: d.file_system().to_string_lossy().into_owned(),
                total_bytes: d.total_space(),
                available_bytes: d.available_space(),
            })
            .collect();

        let mut processes: Vec<ProcessStat> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcessStat {
                pid: pid.as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_pct: p.cpu_usage(),
                memory_bytes: p.memory(),
            })
            .collect();
        processes.sort_by(|a, b| {
            b.memory_bytes
                .cmp(&a.memory_bytes)
                .then_with(|| b.cpu_pct.total_cmp(&a.cpu_pct))
        });
        // shortcut: limit snapshot to top 150 processes to bound single-frame payload size cleanly.
        processes.truncate(MAX_SNAPSHOT_PROCESSES);

        let mut services = list_os_services();
        services.truncate(MAX_SNAPSHOT_SERVICES);

        SysAdminMsg::SystemSnapshotResp {
            snapshot: SystemSnapshot {
                hostname: System::host_name().unwrap_or_else(|| "localhost".into()),
                os_version: System::long_os_version().unwrap_or_else(|| std::env::consts::OS.into()),
                uptime_secs: System::uptime(),
                cpu_usage_pct: self.sys.global_cpu_usage(),
                cpu_cores: self.sys.cpus().len() as u16,
                total_mem_bytes: self.sys.total_memory(),
                used_mem_bytes: self.sys.used_memory(),
                disks,
                processes,
                services,
            },
            error: None,
        }
    }

    pub fn kill_process(&mut self, pid: u32) -> SysAdminMsg {
        if pid <= 4 {
            return SysAdminMsg::ActionResult {
                ok: false,
                message: format!("Refusing to terminate system PID {pid}."),
            };
        }
        let target = Pid::from_u32(pid);
        self.sys.refresh_processes(ProcessesToUpdate::Some(&[target]), true);
        let Some(proc) = self.sys.process(target) else {
            return SysAdminMsg::ActionResult {
                ok: false,
                message: format!("Process {pid} was not found."),
            };
        };
        if proc.kill() {
            SysAdminMsg::ActionResult {
                ok: true,
                message: format!("Terminated process {pid}."),
            }
        } else {
            SysAdminMsg::ActionResult {
                ok: false,
                message: format!("Failed to terminate process {pid} (check permissions)."),
            }
        }
    }

    pub fn control_service(&self, name: &str, action: ServiceOp) -> SysAdminMsg {
        let trimmed = name.trim();
        if trimmed.is_empty()
            || trimmed.contains(['/', '\\', '\0'])
            || trimmed.len() > 256
        {
            return SysAdminMsg::ActionResult {
                ok: false,
                message: "Invalid service name.".into(),
            };
        }
        match execute_service_op(trimmed, action) {
            Ok(msg) => SysAdminMsg::ActionResult {
                ok: true,
                message: msg,
            },
            Err(e) => SysAdminMsg::ActionResult {
                ok: false,
                message: e,
            },
        }
    }
}

/// Runs a non-interactive broadcast command via the native PTY subsystem with a strict timeout.
#[cfg(not(target_os = "android"))]
pub fn execute_broadcast_command(command_id: u64, command: &str) -> SysAdminMsg {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return SysAdminMsg::BroadcastExecResp {
            command_id,
            exit_code: 2,
            output: String::new(),
            error: Some("Command cannot be empty.".into()),
        };
    }

    let pty_system = portable_pty::native_pty_system();
    let pair = match pty_system.openpty(portable_pty::PtySize {
        rows: 40,
        cols: 160,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            return SysAdminMsg::BroadcastExecResp {
                command_id,
                exit_code: -1,
                output: String::new(),
                error: Some(format!("open execution PTY failed: {e}")),
            };
        }
    };

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = portable_pty::CommandBuilder::new("cmd.exe");
        c.args(["/C", trimmed]);
        c
    } else {
        let mut c = portable_pty::CommandBuilder::new("/bin/sh");
        c.args(["-c", trimmed]);
        c
    };
    cmd.env("TERM", "dumb");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            return SysAdminMsg::BroadcastExecResp {
                command_id,
                exit_code: -1,
                output: String::new(),
                error: Some(format!("spawn broadcast command failed: {e}")),
            };
        }
    };
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = child.kill();
            return SysAdminMsg::BroadcastExecResp {
                command_id,
                exit_code: -1,
                output: String::new(),
                error: Some(format!("capture output failed: {e}")),
            };
        }
    };
    let writer = pair.master.take_writer().ok();
    let writer_shared = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let writer_for_reader = writer_shared.clone();
    let mut killer = child.clone_killer();

    let reader_handle = std::thread::spawn(move || {
        use std::io::Write as _;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let mut answered_dsr = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let slice = &buf[..n];
                    if !answered_dsr
                        && slice.windows(4).any(|w| w == b"\x1b[6n")
                        && let Ok(mut guard) = writer_for_reader.lock()
                        && let Some(w) = guard.as_mut()
                    {
                        let _ = w.write_all(b"\x1b[1;1R");
                        let _ = w.flush();
                        answered_dsr = true;
                    }
                    if out.len() + n <= 256 * 1024 {
                        out.extend_from_slice(slice);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        out
    });

    let deadline = std::time::Instant::now() + BROADCAST_TIMEOUT;
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.exit_code() as i32,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    let _ = killer.kill();
                    break -1;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break -1,
        }
    };

    // Close stdin handle BEFORE dropping master (ClosePseudoConsole) and drain reader cleanly.
    if let Ok(mut guard) = writer_shared.lock() {
        let _ = guard.take();
    }
    std::thread::sleep(Duration::from_millis(80));
    drop(pair.master);
    let raw_bytes = reader_handle.join().unwrap_or_default();
    let output = strip_basic_ansi(&String::from_utf8_lossy(&raw_bytes)).replace("\r\n", "\n");
    SysAdminMsg::BroadcastExecResp {
        command_id,
        exit_code,
        output,
        error: timed_out.then(|| "Command exceeded 15s timeout.".to_string()),
    }
}

#[cfg(target_os = "android")]
pub fn execute_broadcast_command(command_id: u64, _command: &str) -> SysAdminMsg {
    SysAdminMsg::BroadcastExecResp {
        command_id,
        exit_code: -1,
        output: String::new(),
        error: Some("Broadcast command execution is not supported on Android hosts.".into()),
    }
}

#[cfg(not(target_os = "android"))]
fn strip_basic_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if matches!(chars.peek(), Some('[' | ']')) {
                let is_osc = matches!(chars.next(), Some(']'));
                for nc in chars.by_ref() {
                    if is_osc && (nc == '\x07' || nc == '\\') {
                        break;
                    }
                    if !is_osc && ('@'..='~').contains(&nc) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(target_os = "windows")]
fn list_os_services() -> Vec<ServiceStat> {
    use windows::Win32::System::Services::{
        CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW,
        OpenSCManagerW, SC_ENUM_PROCESS_INFO, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_PAUSED,
        SERVICE_RUNNING, SERVICE_STATE_ALL, SERVICE_STOPPED, SERVICE_WIN32,
    };
    use windows::core::PCWSTR;

    let mut out = Vec::new();
    // SAFETY: Standard Win32 two-pass EnumServicesStatusExW over local SCM handle closed before return.
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ENUMERATE_SERVICE)
        else {
            return out;
        };

        let mut needed = 0u32;
        let mut count = 0u32;
        let _ = EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            None,
            &mut needed,
            &mut count,
            None,
            PCWSTR::null(),
        );

        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            if EnumServicesStatusExW(
                scm,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                Some(&mut buf),
                &mut needed,
                &mut count,
                None,
                PCWSTR::null(),
            )
            .is_ok()
            {
                let entries = std::slice::from_raw_parts(
                    buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                    count as usize,
                );
                for entry in entries {
                    let name = entry.lpServiceName.to_string().unwrap_or_default();
                    let display_name = entry.lpDisplayName.to_string().unwrap_or_else(|_| name.clone());
                    let state = match entry.ServiceStatusProcess.dwCurrentState {
                        SERVICE_RUNNING => ServiceStateKind::Running,
                        SERVICE_STOPPED => ServiceStateKind::Stopped,
                        SERVICE_PAUSED => ServiceStateKind::Paused,
                        _ => ServiceStateKind::Pending,
                    };
                    out.push(ServiceStat {
                        name,
                        display_name,
                        state,
                    });
                }
            }
        }
        let _ = CloseServiceHandle(scm);
    }

    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

#[cfg(target_os = "windows")]
fn execute_service_op(name: &str, action: ServiceOp) -> Result<String, String> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("open SCM failed: {e}"))?;
    let svc = manager
        .open_service(
            name,
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| format!("open service '{name}' failed: {e}"))?;

    match action {
        ServiceOp::Start => {
            svc.start(&[std::ffi::OsStr::new("")][0..0])
                .map_err(|e| format!("start '{name}' failed: {e}"))?;
            Ok(format!("Started service '{name}'."))
        }
        ServiceOp::Stop => {
            svc.stop()
                .map_err(|e| format!("stop '{name}' failed: {e}"))?;
            Ok(format!("Stopped service '{name}'."))
        }
        ServiceOp::Restart => {
            if let Ok(st) = svc.query_status()
                && st.current_state != ServiceState::Stopped
            {
                let _ = svc.stop();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if let Ok(cur) = svc.query_status()
                        && cur.current_state == ServiceState::Stopped
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
            svc.start(&[std::ffi::OsStr::new("")][0..0])
                .map_err(|e| format!("restart '{name}' failed: {e}"))?;
            Ok(format!("Restarted service '{name}'."))
        }
    }
}

#[cfg(target_os = "linux")]
fn list_os_services() -> Vec<ServiceStat> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in ["/etc/systemd/system", "/lib/systemd/system", "/etc/init.d"] {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for item in rd.flatten() {
            let fname = item.file_name().to_string_lossy().into_owned();
            let is_unit = fname.ends_with(".service");
            let is_init = dir == "/etc/init.d" && item.file_type().map(|t| t.is_file()).unwrap_or(false);
            if (!is_unit && !is_init) || !seen.insert(fname.clone()) {
                continue;
            }
            let stem = fname.trim_end_matches(".service");
            let active_cgroup = std::path::Path::new("/sys/fs/cgroup/system.slice")
                .join(&fname)
                .join("cgroup.procs");
            let active_pid = std::path::Path::new("/run").join(format!("{stem}.pid"));
            let running = std::fs::read_to_string(&active_cgroup)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
                || active_pid.exists();
            out.push(ServiceStat {
                name: fname.clone(),
                display_name: stem.to_string(),
                state: if running {
                    ServiceStateKind::Running
                } else {
                    ServiceStateKind::Stopped
                },
            });
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

#[cfg(target_os = "linux")]
fn execute_service_op(name: &str, action: ServiceOp) -> Result<String, String> {
    let unit = if name.ends_with(".service") {
        name.to_string()
    } else {
        format!("{name}.service")
    };
    let method = match action {
        ServiceOp::Start => "StartUnit",
        ServiceOp::Stop => "StopUnit",
        ServiceOp::Restart => "RestartUnit",
    };
    let mut body = Vec::new();
    push_dbus_str(&mut body, &unit);
    push_dbus_str(&mut body, "replace");
    send_system_dbus_call(
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        method,
        "org.freedesktop.systemd1",
        "ss",
        &body,
    )?;
    Ok(format!("Dispatched {method} for '{unit}'."))
}

#[cfg(target_os = "linux")]
pub(crate) fn call_systemd_login_power(method: &str) -> Result<String, String> {
    let mut body = Vec::new();
    while body.len() % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(&0u32.to_le_bytes()); // interactive = false
    send_system_dbus_call(
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        method,
        "org.freedesktop.login1",
        "b",
        &body,
    )?;
    Ok(format!("System power command '{method}' dispatched."))
}

#[cfg(target_os = "linux")]
fn send_system_dbus_call(
    path: &str,
    iface: &str,
    member: &str,
    dest: &str,
    sig: &str,
    body: &[u8],
) -> Result<(), String> {
    // Pure-Rust D-Bus Unix domain socket call without spawning external CLI tools.
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect("/run/dbus/system_bus_socket")
        .map_err(|e| format!("system D-Bus unavailable: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let uid_hex: String = unsafe { libc::geteuid() }
        .to_string()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    stream
        .write_all(format!("\0AUTH EXTERNAL {uid_hex}\r\n").as_bytes())
        .map_err(|e| format!("dbus auth write: {e}"))?;
    let mut ack = [0u8; 256];
    let n = stream.read(&mut ack).map_err(|e| format!("dbus auth read: {e}"))?;
    if !String::from_utf8_lossy(&ack[..n]).starts_with("OK ") {
        return Err("system D-Bus authentication rejected (insufficient privileges)".into());
    }
    stream.write_all(b"BEGIN\r\n").map_err(|e| format!("dbus begin: {e}"))?;

    let hello = build_dbus_call(
        1,
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "Hello",
        "org.freedesktop.DBus",
        &[],
    );
    stream.write_all(&hello).map_err(|e| format!("dbus hello: {e}"))?;
    let _ = stream.read(&mut ack);

    let msg = build_dbus_call_sig(2, path, iface, member, dest, sig, body);
    stream
        .write_all(&msg)
        .map_err(|e| format!("dbus call {member}: {e}"))?;
    let mut reply = [0u8; 512];
    let rn = stream.read(&mut reply).unwrap_or(0);
    if rn > 0 && reply[1] == 3 {
        return Err(format!("system D-Bus rejected call '{member}'"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn push_dbus_str(buf: &mut Vec<u8>, s: &str) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

#[cfg(target_os = "linux")]
fn build_dbus_call(serial: u32, path: &str, iface: &str, member: &str, dest: &str, body: &[u8]) -> Vec<u8> {
    build_dbus_call_sig(serial, path, iface, member, dest, "", body)
}

#[cfg(target_os = "linux")]
fn build_dbus_call_sig(
    serial: u32,
    path: &str,
    iface: &str,
    member: &str,
    dest: &str,
    sig: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut fields = Vec::new();
    let push_field = |f: &mut Vec<u8>, code: u8, val_type: u8, s: &str| {
        while f.len() % 8 != 0 {
            f.push(0);
        }
        f.push(code);
        f.push(1);
        f.push(val_type);
        f.push(0);
        push_dbus_str(f, s);
    };
    push_field(&mut fields, 1, b'o', path);
    push_field(&mut fields, 2, b's', iface);
    push_field(&mut fields, 3, b's', member);
    push_field(&mut fields, 6, b's', dest);
    if !sig.is_empty() {
        while fields.len() % 8 != 0 {
            fields.push(0);
        }
        fields.push(8);
        fields.push(1);
        fields.push(b'g');
        fields.push(0);
        fields.push(sig.len() as u8);
        fields.extend_from_slice(sig.as_bytes());
        fields.push(0);
    }

    let mut pkt = Vec::with_capacity(16 + fields.len() + 8 + body.len());
    pkt.extend_from_slice(&[b'l', 1, 0, 1]);
    pkt.extend_from_slice(&(body.len() as u32).to_le_bytes());
    pkt.extend_from_slice(&serial.to_le_bytes());
    pkt.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    pkt.extend_from_slice(&fields);
    while pkt.len() % 8 != 0 {
        pkt.push(0);
    }
    pkt.extend_from_slice(body);
    pkt
}

#[cfg(target_os = "macos")]
fn list_os_services() -> Vec<ServiceStat> {
    let mut out = Vec::new();
    for dir in ["/Library/LaunchDaemons", "/System/Library/LaunchDaemons"] {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for item in rd.flatten() {
            let fname = item.file_name().to_string_lossy().into_owned();
            if !fname.ends_with(".plist") {
                continue;
            }
            let stem = fname.trim_end_matches(".plist").to_string();
            out.push(ServiceStat {
                name: stem.clone(),
                display_name: stem,
                state: ServiceStateKind::Unknown,
            });
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

#[cfg(target_os = "macos")]
fn execute_service_op(name: &str, _action: ServiceOp) -> Result<String, String> {
    Err(format!(
        "Direct launchd runtime service control for '{name}' requires macOS Mach bootstrap privileges."
    ))
}

#[cfg(target_os = "android")]
fn list_os_services() -> Vec<ServiceStat> {
    Vec::new()
}

#[cfg(target_os = "android")]
fn execute_service_op(name: &str, _action: ServiceOp) -> Result<String, String> {
    Err(format!("Service action '{name}' is not supported on Android hosts."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysadmin_snapshot_and_pid_guard_work() {
        let mut handler = SysAdminHandler::new();
        let resp = handler.snapshot();
        match resp {
            SysAdminMsg::SystemSnapshotResp { snapshot, error } => {
                assert!(error.is_none());
                assert!(snapshot.total_mem_bytes > 0);
                assert!(!snapshot.processes.is_empty());
            }
            other => panic!("unexpected sysadmin resp: {other:?}"),
        }

        let kill_guard = handler.kill_process(0);
        assert!(matches!(kill_guard, SysAdminMsg::ActionResult { ok: false, .. }));
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn broadcast_command_executes_and_captures_output() {
        let cmd = "echo DESKBOLT_BCAST_OK";
        match execute_broadcast_command(99, cmd) {
            SysAdminMsg::BroadcastExecResp {
                command_id,
                exit_code,
                output,
                error,
            } => {
                assert_eq!(command_id, 99);
                assert_eq!(exit_code, 0);
                assert!(error.is_none());
                assert!(output.contains("DESKBOLT_BCAST_OK"), "output: {output:?}");
            }
            other => panic!("unexpected broadcast resp: {other:?}"),
        }
    }
}
