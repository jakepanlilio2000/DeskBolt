use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::thread;

use deskbolt_proto::{
    ClipboardMsg, ClipboardSync, DEFAULT_CLIPBOARD_MAX_BYTES, DeviceAction, IpcMsg, KeyEvent,
    MonitorInfo, MouseBtn, PointerEvent,
};
use jni::objects::{Global, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::jint;
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
static HOST_STOP: Mutex<Option<watch::Sender<bool>>> = Mutex::new(None);
static HOST_RUNNING: AtomicBool = AtomicBool::new(false);
static CLIPBOARD_BUS: Mutex<Option<broadcast::Sender<Arc<IpcMsg>>>> = Mutex::new(None);
static CLIPBOARD_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_CLIPBOARD_MAX_BYTES);
static CLIPBOARD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

fn with_accessibility(
    call: impl FnOnce(&mut jni::Env<'_>, &JObject<'static>) -> jni::errors::Result<()>,
) {
    if let Some((vm, service)) = a11y_target().lock().unwrap().as_ref() {
        let _ = vm.attach_current_thread(|env| call(env, service.as_obj()));
    }
}

pub(crate) fn set_clipboard_limit(bytes: usize) {
    CLIPBOARD_LIMIT.store(bytes, Ordering::Relaxed);
}

pub(crate) fn write_clipboard(message: ClipboardSync) {
    // Binder shares one transaction buffer between clipboard data and other app traffic.
    // UTF-16 text can occupy twice as many bytes as the wire representation.
    const CLIPBOARD_WRITE_LIMIT: usize = 192 * 1024;
    if !message.within_limit(
        CLIPBOARD_LIMIT
            .load(Ordering::Relaxed)
            .min(CLIPBOARD_WRITE_LIMIT),
    ) {
        return;
    }
    let (kind, value) = match &message {
        ClipboardSync::SetText { utf8_text, .. } => (0, utf8_text.as_str()),
        ClipboardSync::SetData {
            data: ClipboardMsg::Utf8Text(text),
            ..
        } => (0, text.as_str()),
        ClipboardSync::SetData {
            data: ClipboardMsg::Html(html),
            ..
        } => (1, html.as_str()),
        ClipboardSync::Clear { .. } => (2, ""),
        _ => return,
    };
    with_accessibility(|env, service| {
        let text = env.new_string(value)?;
        env.call_method(
            service,
            jni_str!("onRemoteClipboard"),
            jni_sig!("(ILjava/lang/String;)V"),
            &[JValue::Int(kind), JValue::Object(&text)],
        )?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeOnClipboardText<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    text: JString<'caller>,
    html: JString<'caller>,
) {
    env.with_env(|_| -> Result<(), jni::errors::Error> {
        let data = if html.as_raw().is_null() {
            ClipboardMsg::Utf8Text(text.to_string())
        } else {
            ClipboardMsg::Html(html.to_string())
        };
        let seq = CLIPBOARD_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
        let message = ClipboardSync::set(seq, data);
        if message.within_limit(CLIPBOARD_LIMIT.load(Ordering::Relaxed))
            && let Some(bus) = CLIPBOARD_BUS.lock().unwrap().as_ref()
        {
            let _ = bus.send(Arc::new(IpcMsg::Clipboard(message)));
        }
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}

struct AndroidA11yInjector;

impl InputInjector for AndroidA11yInjector {
    fn inject_pointer(&mut self, ev: PointerEvent, _mon: &MonitorInfo) {
        with_accessibility(|env, service| {
            match ev {
                PointerEvent::MoveAbs { norm_x, norm_y } => {
                    env.call_method(
                        service,
                        jni_str!("onRemotePointer"),
                        jni_sig!("(IFF)V"),
                        &[JValue::Int(0), JValue::Float(norm_x), JValue::Float(norm_y)],
                    )?;
                }
                PointerEvent::Button {
                    button,
                    down,
                    norm_x,
                    norm_y,
                } => {
                    let action = match (button, down) {
                        (MouseBtn::Left, true) => 1,
                        (MouseBtn::Left, false) => 2,
                        (MouseBtn::Right, true) => 3,
                        _ => return Ok(()),
                    };
                    env.call_method(
                        service,
                        jni_str!("onRemotePointer"),
                        jni_sig!("(IFF)V"),
                        &[
                            JValue::Int(action),
                            JValue::Float(norm_x),
                            JValue::Float(norm_y),
                        ],
                    )?;
                }
                PointerEvent::Wheel { delta_x, delta_y } => {
                    env.call_method(
                        service,
                        jni_str!("onRemoteScroll"),
                        jni_sig!("(II)V"),
                        &[
                            JValue::Int(i32::from(delta_x)),
                            JValue::Int(i32::from(delta_y)),
                        ],
                    )?;
                }
            }
            Ok(())
        });
    }

    fn inject_key(&mut self, ev: KeyEvent) {
        with_accessibility(|env, service| {
            env.call_method(
                service,
                jni_str!("onRemoteKey"),
                jni_sig!("(IZ)V"),
                &[JValue::Int(i32::from(ev.vk_code)), JValue::Bool(ev.down)],
            )?;
            Ok(())
        });
    }

    fn inject_unicode(&mut self, ch: char) {
        with_accessibility(|env, service| {
            let text = env.new_string(ch.encode_utf8(&mut [0; 4]))?;
            env.call_method(
                service,
                jni_str!("onRemoteText"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&text)],
            )?;
            Ok(())
        });
    }

    fn device_action(&mut self, action: DeviceAction) {
        let id = match action {
            DeviceAction::Home => 0,
            DeviceAction::Back => 1,
            DeviceAction::Recents => 2,
            DeviceAction::Notifications => 3,
            DeviceAction::QuickSettings => 4,
            DeviceAction::LockScreen => 5,
        };
        with_accessibility(|env, service| {
            env.call_method(
                service,
                jni_str!("onRemoteAction"),
                jni_sig!("(I)V"),
                &[JValue::Int(id)],
            )?;
            Ok(())
        });
    }

    fn release_all_keys(&mut self) {
        with_accessibility(|env, service| {
            env.call_method(service, jni_str!("onRemoteCancel"), jni_sig!("()V"), &[])?;
            Ok(())
        });
    }
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
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeRegisterAccessibility<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    service_obj: JObject<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> Result<(), jni::errors::Error> {
        let target = if service_obj.as_raw().is_null() {
            None
        } else {
            Some((env.get_java_vm()?, env.new_global_ref(service_obj)?))
        };
        *a11y_target().lock().unwrap() = target;
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
            return Ok(1);
        }

        let dir = android_data_dir();
        let prepare = (|| {
            let mut cfg = ServerConfig::load_or_default(&dir)?;
            cfg.set_password_bytes(password_str.as_bytes())?;
            cfg.save(&dir)?;
            let identity = load_or_generate_identity(&cfg, false)?;
            Ok::<_, String>((cfg, identity))
        })();
        let (cfg, identity) = match prepare {
            Ok(prepared) => prepared,
            Err(error) => {
                warn!("Could not start screen sharing: {error}");
                HOST_RUNNING.store(false, Ordering::SeqCst);
                return Ok(1);
            }
        };
        *ingress().latest.lock().unwrap() = None;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *HOST_STOP.lock().unwrap() = Some(shutdown_tx.clone());
        thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            else {
                *HOST_STOP.lock().unwrap() = None;
                HOST_RUNNING.store(false, Ordering::SeqCst);
                return;
            };
            let _ = rt.block_on(async move {
                let (to_agent_tx, to_agent_rx) = mpsc::channel(256);
                let (from_agent_tx, _) = broadcast::channel(32);
                set_clipboard_limit(cfg.clipboard_max_bytes);
                *CLIPBOARD_BUS.lock().unwrap() = Some(from_agent_tx.clone());
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
                if let Err(error) = run_tcp_server(
                    cfg,
                    identity.cert_der,
                    identity.key_der,
                    hub,
                    false,
                    shutdown_rx,
                )
                .await
                {
                    warn!("Screen sharing stopped: {error}");
                }
                let _ = shutdown_tx.send(true);
                *CLIPBOARD_BUS.lock().unwrap() = None;
                let _ = agent_task.await;
            });
            *HOST_STOP.lock().unwrap() = None;
            HOST_RUNNING.store(false, Ordering::SeqCst);
        });
        info!("Android MediaProjection host started");
        Ok(0)
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_stacksync_deskbolt_DeskBoltHostBridge_nativeStopHost<'caller>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) {
    if let Some(tx) = HOST_STOP.lock().unwrap().as_ref() {
        let _ = tx.send(true);
    }
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
