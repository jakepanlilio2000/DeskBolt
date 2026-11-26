use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, atomic::{AtomicBool, Ordering}};
use std::thread;

use deskbolt_proto::{ClipboardSync, IpcMsg, KeyEvent, MonitorInfo, MouseBtn, PointerEvent};
use jni::objects::{Global, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jfloat, jint};
use jni::{EnvUnowned, JavaVM, jni_sig, jni_str};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::agent::run_inprocess_agent;
use crate::config::{ServerConfig, load_or_generate_identity};
use crate::ipc::AgentHub;
use crate::net::run_tcp_server;

use super::{
    CaptureOutput, InputInjector, RawCapturedFrame, ScreenCapturer, diff_bgra_dirty_rects,
    rgba_to_bgra,
};

struct FrameIngress {
    latest: Mutex<Option<(u32, u32, Vec<u8>)>>,
    cv: Condvar,
}

static ANDROID_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static FRAME_INGRESS: OnceLock<FrameIngress> = OnceLock::new();
static ACCESSIBILITY_TARGET: OnceLock<Mutex<Option<(JavaVM, Global<JObject<'static>>)>>> =
    OnceLock::new();
static HOST_STOP: OnceLock<watch::Sender<bool>> = OnceLock::new();
static HOST_RUNNING: AtomicBool = AtomicBool::new(false);

fn ingress() -> &'static FrameIngress {
    FRAME_INGRESS.get_or_init(|| FrameIngress {
        latest: Mutex::new(None),
        cv: Condvar::new(),
    })
}

fn a11y_target() -> &'static Mutex<Option<(JavaVM, Global<JObject<'static>>)>> {
    ACCESSIBILITY_TARGET.get_or_init(|| Mutex::new(None))
}

pub fn android_data_dir() -> PathBuf {
    ANDROID_DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp/deskbolt"))
}

struct AndroidCapturer {
    prev_bgra: Vec<u8>,
    last_dims: (u32, u32),
}

impl ScreenCapturer for AndroidCapturer {
    fn monitors(&self) -> Vec<MonitorInfo> {
        let (w, h) = self.last_dims;
        vec![MonitorInfo {
            id: 0,
            name: "Android Screen".into(),
            is_primary: true,
            left: 0,
            top: 0,
            width: w.max(720),
            height: h.max(1280),
            dpi_scale: 1.0,
        }]
    }

    fn set_monitor(&mut self, _monitor_idx: u8) {}

    fn capture_next(&mut self, force_full: bool, timeout_ms: u32) -> CaptureOutput {
        let ing = ingress();
        let mut guard = ing.latest.lock().unwrap();
        if guard.is_none() {
            let (g, _) = ing
                .cv
                .wait_timeout(guard, std::time::Duration::from_millis(timeout_ms as u64))
                .unwrap();
            guard = g;
        }
        let Some((w, h, rgba)) = guard.take() else {
            return CaptureOutput::Timeout;
        };
        drop(guard);

        let bgra = rgba_to_bgra(&rgba);
        let dims_changed = (w, h) != self.last_dims && w > 0 && h > 0;
        if dims_changed {
            self.last_dims = (w, h);
            self.prev_bgra.clear();
        }
        let full = force_full || dims_changed || self.prev_bgra.len() != bgra.len();
        let dirty_rects = if full {
            Vec::new()
        } else {
            let rects = diff_bgra_dirty_rects(&self.prev_bgra, &bgra, w, h, 64);
            if rects.is_empty() {
                return CaptureOutput::Timeout;
            }
            rects
        };
        self.prev_bgra = bgra.clone();

        CaptureOutput::Frame(RawCapturedFrame {
            monitor_id: 0,
            width: w,
            height: h,
            bgra,
            move_rects: Vec::new(),
            dirty_rects,
            force_full: full,
        })
    }
}

struct AndroidA11yInjector;

impl InputInjector for AndroidA11yInjector {
    fn inject_pointer(&mut self, ev: PointerEvent, _mon: &MonitorInfo) {
        let guard = a11y_target().lock().unwrap();
        let Some((vm, obj)) = guard.as_ref() else {
            return;
        };
        if let PointerEvent::Button {
            button,
            down,
            norm_x,
            norm_y,
        } = ev
        {
            if button == MouseBtn::Left && down {
                let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
                    let _ = env.call_method(
                        obj.as_obj(),
                        jni_str!("onRemoteTap"),
                        jni_sig!("(FF)V"),
                        &[JValue::Float(norm_x as jfloat), JValue::Float(norm_y as jfloat)],
                    );
                    Ok(())
                });
            }
        }
    }

    fn inject_key(&mut self, ev: KeyEvent) {
        let guard = a11y_target().lock().unwrap();
        let Some((vm, obj)) = guard.as_ref() else {
            return;
        };
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            let _ = env.call_method(
                obj.as_obj(),
                jni_str!("onRemoteKey"),
                jni_sig!("(IZ)V"),
                &[
                    JValue::Int(ev.vk_code as jint),
                    JValue::Bool(ev.down),
                ],
            );
            Ok(())
        });
    }

    fn release_all_keys(&mut self) {}
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeOnFrameRgba<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    rgba_bytes: JByteArray<'caller>,
    width: jint,
    height: jint,
) {
    let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
        let len = rgba_bytes.len(env)?;
        if len == 0 || width <= 0 || height <= 0 {
            return Ok(());
        }
        let mut raw_i8 = vec![0i8; len];
        rgba_bytes.get_region(env, 0, &mut raw_i8)?;
        let rgba: Vec<u8> = raw_i8.into_iter().map(|b| b as u8).collect();
        let ing = ingress();
        *ing.latest.lock().unwrap() = Some((width as u32, height as u32, rgba));
        ing.cv.notify_one();
        Ok(())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeRegisterAccessibility<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    service_obj: JObject<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
        let vm = env.get_java_vm()?;
        let global = env.new_global_ref(service_obj)?;
        *a11y_target().lock().unwrap() = Some((vm, global));
        Ok(())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeStartHost<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    data_dir_j: JString<'caller>,
    password_j: JString<'caller>,
) -> jint {
    let outcome = unowned_env.with_env(|_env| -> Result<jint, jni::errors::Error> {
        let data_dir_str: String = data_dir_j.to_string();
        let password_str: String = password_j.to_string();
        let _ = ANDROID_DATA_DIR.set(PathBuf::from(&data_dir_str));
        if HOST_RUNNING.swap(true, Ordering::SeqCst) {
            return Ok(0);
        }

        let dir = android_data_dir();
        if !password_str.is_empty()
            && let Ok(mut cfg) = ServerConfig::load_or_default(&dir)
            && cfg.set_password_bytes(password_str.as_bytes()).is_ok()
        {
            let _ = cfg.save(&dir);
        }

        thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread().enable_all().build() else {
                HOST_RUNNING.store(false, Ordering::SeqCst);
                return;
            };
            let _ = rt.block_on(async move {
                let cfg = match ServerConfig::load_or_default(&dir) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Android host config error: {e}");
                        return;
                    }
                };
                let Ok(identity) = load_or_generate_identity(&cfg, false) else {
                    return;
                };
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let _ = HOST_STOP.set(shutdown_tx);
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
                let _ = run_tcp_server(
                    cfg,
                    identity.cert_der,
                    identity.key_der,
                    hub,
                    false,
                    shutdown_rx,
                )
                .await;
                let _ = agent_task.await;
            });
            HOST_RUNNING.store(false, Ordering::SeqCst);
        });
        info!("Android MediaProjection host started");
        Ok(1)
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeStopHost<'caller>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) {
    if let Some(tx) = HOST_STOP.get() {
        let _ = tx.send(true);
    }
    HOST_RUNNING.store(false, Ordering::SeqCst);
}

pub fn create_capturer(_monitor_idx: u8) -> Result<Box<dyn ScreenCapturer>, String> {
    Ok(Box::new(AndroidCapturer {
        prev_bgra: Vec::new(),
        last_dims: (0, 0),
    }))
}

pub fn create_injector() -> Box<dyn InputInjector> {
    Box::new(AndroidA11yInjector)
}

pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    vec![MonitorInfo {
        id: 0,
        name: "Android Screen".into(),
        is_primary: true,
        left: 0,
        top: 0,
        width: 1080,
        height: 2400,
        dpi_scale: 1.0,
    }]
}

pub fn bind_active_input_desktop() -> Result<String, String> {
    Ok("AndroidMediaProjection".into())
}

pub fn run_clipboard_loop(
    _rx: std::sync::mpsc::Receiver<ClipboardSync>,
    _ev_tx: mpsc::Sender<IpcMsg>,
    stop: Arc<AtomicBool>,
) {
    let mut _q: VecDeque<String> = VecDeque::new();
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_millis(250));
    }
}

pub fn install_service() -> Result<(), String> {
    Err("Android runs DeskBolt host as an in-app foreground MediaProjection service.".into())
}

pub fn uninstall_service() -> Result<(), String> {
    Err("Android service lifecycle is managed by Android Settings / App uninstall.".into())
}

pub fn run_service_dispatcher() -> Result<(), String> {
    crate::run_inprocess_server_mode()
}

pub fn trigger_send_sas() -> Result<(), String> {
    Err("Send Ctrl+Alt+Del (SAS) is only supported on Windows service hosts.".into())
}

pub fn set_restrictive_acl(_path: &Path) -> Result<(), String> {
    Ok(())
}

pub fn read_password_masked() -> Result<String, String> {
    Err("Interactive stdin password prompt is unsupported on Android.".into())
}

pub fn dpapi_protect_machine(_plain: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI not applicable on Android".into())
}

pub fn dpapi_unprotect_machine(_cipher: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI not applicable on Android".into())
}

use std::net::IpAddr;

use deskbolt_proto::{PowerAction, SessionMode};

pub fn startup_permission_check() {}

pub fn show_desktop_popup(title: &str, body: &str, urgent: bool) -> Result<(), String> {
    info!(urgent, %title, %body, "Remote popup notification received");
    Ok(())
}

pub fn prompt_connection_confirmation(
    _peer_ip: IpAddr,
    _mode: SessionMode,
    _timeout_secs: u16,
    default_allow: bool,
) -> bool {
    default_allow
}

pub fn execute_power_action(
    _action: PowerAction,
    _force: bool,
    _delay_secs: u32,
) -> Result<String, String> {
    Err("Remote system power operations are unsupported on Android.".into())
}

pub fn set_curtain_mode(_enabled: bool) -> Result<(), String> {
    Ok(())
}

pub fn set_os_autostart(_enabled: bool) -> Result<(), String> {
    Ok(())
}

pub fn start_tray_icon(
    _shutdown_rx: watch::Receiver<bool>,
    _shutdown_tx: watch::Sender<bool>,
) -> Option<std::thread::JoinHandle<()>> {
    None
}
