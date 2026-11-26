use deskbolt_proto::{SerialParity, TerminalMsg};
use tokio::sync::mpsc;

#[cfg(not(target_os = "android"))]
use std::{
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(not(target_os = "android"))]
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Maximum terminal or serial payload bytes per frame.
#[cfg(not(target_os = "android"))]
const IO_READ_BUF: usize = 4096;

#[cfg(not(target_os = "android"))]
struct ActivePty {
    writer: Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    stop: Arc<AtomicBool>,
}

#[cfg(not(target_os = "android"))]
struct ActiveSerial {
    writer: Box<dyn serialport::SerialPort>,
    stop: Arc<AtomicBool>,
}

/// Manages per-session Channel `0x06` interactive PTY and serial port consoles.
pub struct TerminalSessionHandler {
    tx: mpsc::Sender<TerminalMsg>,
    #[cfg(not(target_os = "android"))]
    pty: Option<ActivePty>,
    #[cfg(not(target_os = "android"))]
    serial: Option<ActiveSerial>,
}

impl TerminalSessionHandler {
    pub fn new(tx: mpsc::Sender<TerminalMsg>) -> Self {
        Self {
            tx,
            #[cfg(not(target_os = "android"))]
            pty: None,
            #[cfg(not(target_os = "android"))]
            serial: None,
        }
    }

    pub async fn handle_message(&mut self, msg: TerminalMsg) {
        match msg {
            TerminalMsg::OpenPty {
                cols,
                rows,
                shell_hint,
            } => {
                let resp = self.open_pty(cols, rows, shell_hint);
                let _ = self.tx.send(resp).await;
            }
            TerminalMsg::PtyData(bytes) => {
                self.write_pty(&bytes);
            }
            TerminalMsg::PtyResize { cols, rows } => {
                self.resize_pty(cols, rows);
            }
            TerminalMsg::ClosePty => {
                self.close_pty();
            }
            TerminalMsg::ListSerialPortsReq => {
                let resp = list_serial_ports();
                let _ = self.tx.send(resp).await;
            }
            TerminalMsg::SerialOpen {
                port,
                baud_rate,
                data_bits,
                parity,
                stop_bits,
            } => {
                let resp = self.open_serial(&port, baud_rate, data_bits, parity, stop_bits);
                let _ = self.tx.send(resp).await;
            }
            TerminalMsg::SerialData(bytes) => {
                self.write_serial(&bytes);
            }
            TerminalMsg::SerialClose => {
                self.close_serial();
            }
            _ => {}
        }
    }

    pub fn cleanup_all(&mut self) {
        self.close_pty();
        self.close_serial();
    }

    #[cfg(target_os = "android")]
    fn open_pty(&mut self, _cols: u16, _rows: u16, _hint: Option<String>) -> TerminalMsg {
        TerminalMsg::PtyOpened {
            ok: false,
            shell: String::new(),
            error: Some("Interactive PTY shells are not supported on Android hosts.".into()),
        }
    }

    #[cfg(not(target_os = "android"))]
    fn open_pty(&mut self, cols: u16, rows: u16, shell_hint: Option<String>) -> TerminalMsg {
        self.close_pty();

        let pty_system = native_pty_system();
        let size = PtySize {
            rows: rows.clamp(4, 500),
            cols: cols.clamp(20, 500),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = match pty_system.openpty(size) {
            Ok(p) => p,
            Err(e) => {
                return TerminalMsg::PtyOpened {
                    ok: false,
                    shell: String::new(),
                    error: Some(format!("open PTY failed: {e}")),
                };
            }
        };

        let shell_prog = resolve_shell_program(shell_hint.as_deref());
        let mut cmd = CommandBuilder::new(&shell_prog);
        cmd.env("TERM", "xterm-256color");

        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                return TerminalMsg::PtyOpened {
                    ok: false,
                    shell: shell_prog,
                    error: Some(format!("spawn shell failed: {e}")),
                };
            }
        };
        drop(pair.slave);

        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                let _ = child.kill();
                return TerminalMsg::PtyOpened {
                    ok: false,
                    shell: shell_prog,
                    error: Some(format!("clone PTY reader failed: {e}")),
                };
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                let _ = child.kill();
                return TerminalMsg::PtyOpened {
                    ok: false,
                    shell: shell_prog,
                    error: Some(format!("take PTY writer failed: {e}")),
                };
            }
        };

        let killer = child.clone_killer();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let tx = self.tx.clone();
        let tx_exit = self.tx.clone();
        let shared_writer = Arc::new(std::sync::Mutex::new(Some(writer)));
        let writer_for_reader = shared_writer.clone();

        std::thread::spawn(move || {
            let exit_code = child
                .wait()
                .map(|st| st.exit_code() as i32)
                .unwrap_or(-1);
            std::thread::sleep(Duration::from_millis(80));
            let _ = tx_exit.blocking_send(TerminalMsg::PtyExit(exit_code));
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; IO_READ_BUF];
            let mut answered_dsr = false;
            while !stop_flag.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let slice = &buf[..n];
                        // Answer ConPTY startup DSR cursor-position probe automatically so Windows shells unblock.
                        if !answered_dsr
                            && slice.windows(4).any(|w| w == b"\x1b[6n")
                            && let Ok(mut guard) = writer_for_reader.lock()
                            && let Some(w) = guard.as_mut()
                        {
                            let _ = w.write_all(b"\x1b[1;1R");
                            let _ = w.flush();
                            answered_dsr = true;
                        }
                        if tx
                            .blocking_send(TerminalMsg::PtyData(slice.to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        self.pty = Some(ActivePty {
            writer: shared_writer,
            master: pair.master,
            killer,
            stop,
        });

        TerminalMsg::PtyOpened {
            ok: true,
            shell: shell_prog,
            error: None,
        }
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        #[cfg(not(target_os = "android"))]
        if let Some(pty) = &self.pty
            && let Ok(mut guard) = pty.writer.lock()
            && let Some(w) = guard.as_mut()
        {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
        #[cfg(target_os = "android")]
        let _ = bytes;
    }

    fn resize_pty(&mut self, cols: u16, rows: u16) {
        #[cfg(not(target_os = "android"))]
        if let Some(pty) = &self.pty {
            let _ = pty.master.resize(PtySize {
                rows: rows.clamp(4, 500),
                cols: cols.clamp(20, 500),
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        #[cfg(target_os = "android")]
        let _ = (cols, rows);
    }

    fn close_pty(&mut self) {
        #[cfg(not(target_os = "android"))]
        if let Some(mut pty) = self.pty.take() {
            pty.stop.store(true, Ordering::Relaxed);
            if let Ok(mut guard) = pty.writer.lock() {
                let _ = guard.take();
            }
            let _ = pty.killer.kill();
        }
    }

    #[cfg(target_os = "android")]
    fn open_serial(
        &mut self,
        port: &str,
        _baud_rate: u32,
        _data_bits: u8,
        _parity: SerialParity,
        _stop_bits: u8,
    ) -> TerminalMsg {
        TerminalMsg::SerialOpened {
            ok: false,
            port: port.to_string(),
            error: Some("Serial console is not supported on Android hosts.".into()),
        }
    }

    #[cfg(not(target_os = "android"))]
    fn open_serial(
        &mut self,
        port: &str,
        baud_rate: u32,
        data_bits: u8,
        parity: SerialParity,
        stop_bits: u8,
    ) -> TerminalMsg {
        self.close_serial();
        let trimmed = port.trim();
        if trimmed.is_empty() {
            return TerminalMsg::SerialOpened {
                ok: false,
                port: String::new(),
                error: Some("serial port name cannot be empty".into()),
            };
        }

        let db = match data_bits {
            5 => serialport::DataBits::Five,
            6 => serialport::DataBits::Six,
            7 => serialport::DataBits::Seven,
            _ => serialport::DataBits::Eight,
        };
        let par = match parity {
            SerialParity::Odd => serialport::Parity::Odd,
            SerialParity::Even => serialport::Parity::Even,
            SerialParity::None => serialport::Parity::None,
        };
        let sb = if stop_bits == 2 {
            serialport::StopBits::Two
        } else {
            serialport::StopBits::One
        };

        let sp = match serialport::new(trimmed, baud_rate.max(300))
            .data_bits(db)
            .parity(par)
            .stop_bits(sb)
            .timeout(Duration::from_millis(50))
            .open()
        {
            Ok(p) => p,
            Err(e) => {
                return TerminalMsg::SerialOpened {
                    ok: false,
                    port: trimmed.to_string(),
                    error: Some(format!("open serial port failed: {e}")),
                };
            }
        };

        let mut reader = match sp.try_clone() {
            Ok(r) => r,
            Err(e) => {
                return TerminalMsg::SerialOpened {
                    ok: false,
                    port: trimmed.to_string(),
                    error: Some(format!("clone serial reader failed: {e}")),
                };
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; IO_READ_BUF];
            while !stop_flag.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        if tx
                            .blocking_send(TerminalMsg::SerialData(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(_) => break,
                }
            }
        });

        self.serial = Some(ActiveSerial { writer: sp, stop });
        TerminalMsg::SerialOpened {
            ok: true,
            port: trimmed.to_string(),
            error: None,
        }
    }

    fn write_serial(&mut self, bytes: &[u8]) {
        #[cfg(not(target_os = "android"))]
        if let Some(serial) = &mut self.serial {
            let _ = serial.writer.write_all(bytes);
            let _ = serial.writer.flush();
        }
        #[cfg(target_os = "android")]
        let _ = bytes;
    }

    fn close_serial(&mut self) {
        #[cfg(not(target_os = "android"))]
        if let Some(serial) = self.serial.take() {
            serial.stop.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for TerminalSessionHandler {
    fn drop(&mut self) {
        self.cleanup_all();
    }
}

#[cfg(not(target_os = "android"))]
fn list_serial_ports() -> TerminalMsg {
    match serialport::available_ports() {
        Ok(list) => TerminalMsg::ListSerialPortsResp {
            ports: list.into_iter().map(|p| p.port_name).collect(),
            error: None,
        },
        Err(e) => TerminalMsg::ListSerialPortsResp {
            ports: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

#[cfg(target_os = "android")]
fn list_serial_ports() -> TerminalMsg {
    TerminalMsg::ListSerialPortsResp {
        ports: Vec::new(),
        error: None,
    }
}

/// Resolves an allowed interactive shell executable cleanly across Windows and Unix.
#[cfg(not(target_os = "android"))]
pub fn resolve_shell_program(hint: Option<&str>) -> String {
    if let Some(raw) = hint.map(str::trim).filter(|s| !s.is_empty()) {
        let lower = raw.to_ascii_lowercase();
        let allowed = if cfg!(target_os = "windows") {
            matches!(lower.as_str(), "cmd.exe" | "cmd" | "powershell.exe" | "powershell" | "pwsh.exe" | "pwsh")
        } else {
            matches!(lower.as_str(), "/bin/sh" | "/bin/bash" | "/bin/zsh" | "/usr/bin/bash" | "/usr/bin/zsh" | "sh" | "bash" | "zsh")
        };
        if allowed {
            return raw.to_string();
        }
    }
    if cfg!(target_os = "windows") {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pty_spawn_resize_io_and_close_roundtrip() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut handler = TerminalSessionHandler::new(tx);

        handler
            .handle_message(TerminalMsg::OpenPty {
                cols: 80,
                rows: 24,
                shell_hint: None,
            })
            .await;

        let opened = rx.recv().await.unwrap();
        assert!(matches!(opened, TerminalMsg::PtyOpened { ok: true, .. }));

        handler
            .handle_message(TerminalMsg::PtyResize {
                cols: 110,
                rows: 32,
            })
            .await;

        let cmd = if cfg!(target_os = "windows") {
            b"echo DESKBOLT_PTY_CHECK & exit 0\r\n".to_vec()
        } else {
            b"echo DESKBOLT_PTY_CHECK; exit 0\n".to_vec()
        };
        handler.handle_message(TerminalMsg::PtyData(cmd)).await;

        let mut output = String::new();
        let mut saw_exit = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(TerminalMsg::PtyData(bytes))) => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                    if saw_exit && output.contains("DESKBOLT_PTY_CHECK") {
                        break;
                    }
                }
                Ok(Some(TerminalMsg::PtyExit(_))) => {
                    saw_exit = true;
                    if output.contains("DESKBOLT_PTY_CHECK") {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    if saw_exit && output.contains("DESKBOLT_PTY_CHECK") {
                        break;
                    }
                }
            }
        }
        assert!(output.contains("DESKBOLT_PTY_CHECK"), "captured output: {output:?}");
        assert!(saw_exit, "expected PtyExit signal");
        handler.cleanup_all();
    }
}
