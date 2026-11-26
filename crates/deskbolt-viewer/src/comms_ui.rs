use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use deskbolt_proto::{CommsMsg, PowerAction, PowerMsg, SessionMode, VideoQualityMode, list_voice_devices};
use egui::{Color32, Context, Key, ScrollArea, Window};

use crate::admin_ui::close_headless_if_unused;
use crate::db::{
    HostInput, HostProtocol, HostRow, MultimonitorViewMode, ScaleMode, StartupView,
    SystemKeysMode, TextureFilterMode, ThemeChoice, ViewerSettings,
};
use crate::oob::{OobAction, OobTarget, execute_oob_action};
use crate::session::{ChatLine, ConnectionPhase, SessionCommand};
use crate::ui::DeskBoltViewerApp;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HostPropertiesTab {
    General,
    DisplayMode,
    WakeOnLan,
    OobHardware,
    PortTunnels,
}

pub struct HostPropertiesModal {
    pub id: Option<i64>,
    pub folder_id: Option<i64>,
    pub tab: HostPropertiesTab,
    pub name: String,
    pub host: String,
    pub port: String,
    pub protocol_type: HostProtocol,
    pub default_mode: SessionMode,
    pub quality_mode: VideoQualityMode,
    pub custom_quality_override: bool,
    pub custom_quality: VideoQualityMode,
    pub custom_fps_override: bool,
    pub custom_fps: String,
    pub mac_address: String,
    pub ssh_key_path: String,
    pub saved_tunnels_json: String,
    pub oob_enabled: bool,
    pub oob_host: String,
    pub oob_port: String,
    pub oob_use_tls: bool,
    pub oob_tls_port: String,
    pub oob_username: String,
    pub oob_password: String,
    pub notes: String,
    pub error: Option<String>,
}

impl HostPropertiesModal {
    pub fn from_host(folder_id: Option<i64>, host: Option<&HostRow>) -> Self {
        if let Some(h) = host {
            Self {
                id: Some(h.id),
                folder_id: h.folder_id,
                tab: HostPropertiesTab::General,
                name: h.name.clone(),
                host: h.host.clone(),
                port: h.port.to_string(),
                protocol_type: h.protocol_type,
                default_mode: h.default_mode,
                quality_mode: h.quality_mode,
                custom_quality_override: h.custom_quality.is_some(),
                custom_quality: h.custom_quality.unwrap_or(h.quality_mode),
                custom_fps_override: h.custom_fps.is_some(),
                custom_fps: h.custom_fps.map(|f| f.to_string()).unwrap_or_else(|| "60".into()),
                mac_address: h.mac_address.clone().unwrap_or_default(),
                ssh_key_path: h.ssh_key_path.clone().unwrap_or_default(),
                saved_tunnels_json: h.saved_tunnels_json.clone().unwrap_or_default(),
                oob_enabled: h.oob_enabled,
                oob_host: h.oob_host.clone().unwrap_or_default(),
                oob_port: h.oob_port.to_string(),
                oob_use_tls: h.oob_tls > 0,
                oob_tls_port: if h.oob_tls > 0 { h.oob_tls } else { 16993 }.to_string(),
                oob_username: h.oob_username.clone().unwrap_or_default(),
                oob_password: h.oob_password.clone().unwrap_or_default(),
                notes: h.notes.clone().unwrap_or_default(),
                error: None,
            }
        } else {
            Self {
                id: None,
                folder_id,
                tab: HostPropertiesTab::General,
                name: String::new(),
                host: String::new(),
                port: "4899".into(),
                protocol_type: HostProtocol::DeskBolt,
                default_mode: SessionMode::FullControl,
                quality_mode: VideoQualityMode::Auto,
                custom_quality_override: false,
                custom_quality: VideoQualityMode::Auto,
                custom_fps_override: false,
                custom_fps: "60".into(),
                mac_address: String::new(),
                ssh_key_path: String::new(),
                saved_tunnels_json: String::new(),
                oob_enabled: false,
                oob_host: String::new(),
                oob_port: "16992".into(),
                oob_use_tls: true,
                oob_tls_port: "16993".into(),
                oob_username: String::new(),
                oob_password: String::new(),
                notes: String::new(),
                error: None,
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OptionsTab {
    Connection,
    DisplayInput,
    AudioChat,
    HotkeysAppearance,
}

pub struct OptionsModalState {
    pub tab: OptionsTab,
    pub draft: ViewerSettings,
    pub input_devices: Vec<String>,
    pub output_devices: Vec<String>,
    pub status: String,
}

impl OptionsModalState {
    pub fn open_from(current: &ViewerSettings) -> Self {
        let (mut ins, mut outs) = list_voice_devices();
        if !ins.iter().any(|s| s == "Default") {
            ins.insert(0, "Default".into());
        }
        if !outs.iter().any(|s| s == "Default") {
            outs.insert(0, "Default".into());
        }
        Self {
            tab: OptionsTab::Connection,
            draft: current.clone(),
            input_devices: ins,
            output_devices: outs,
            status: String::new(),
        }
    }
}

pub struct OobDialogState {
    pub host_id: Option<i64>,
    pub label: String,
    pub host: String,
    pub port: String,
    pub use_tls: bool,
    pub tls_port: String,
    pub username: String,
    pub password: String,
}

impl DeskBoltViewerApp {
    pub fn trigger_oob_action(&mut self, ctx: &Context, host: &HostRow, action: OobAction) {
        if action == OobAction::BiosRemoteControl {
            self.open_bios_sol_terminal(ctx, host);
            return;
        }
        let target = OobTarget {
            host: host
                .oob_host
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&host.host)
                .to_string(),
            port: host.oob_port,
            tls_port: host.oob_tls,
            use_tls: host.oob_tls > 0,
            username: host.oob_username.clone().unwrap_or_default(),
            password: host.oob_password.clone().unwrap_or_default(),
        };
        if target.username.trim().is_empty() {
            self.status_line = format!(
                "Configure Out-of-Band credentials in Properties (Alt+Enter) for '{}'.",
                host.name
            );
            self.oob_dialog = Some(OobDialogState {
                host_id: Some(host.id),
                label: host.name.clone(),
                host: target.host,
                port: target.port.to_string(),
                use_tls: target.use_tls,
                tls_port: if target.tls_port > 0 { target.tls_port } else { 16993 }.to_string(),
                username: target.username,
                password: target.password,
            });
            return;
        }
        self.dispatch_oob_target_action(ctx, Some(host.id), target, action);
    }

    fn dispatch_oob_target_action(
        &mut self,
        ctx: &Context,
        host_id: Option<i64>,
        target: OobTarget,
        action: OobAction,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.admin.oob_rx = Some(rx);
        self.admin.oob_status = format!("Dispatching {}...", action.label());
        self.status_line = self.admin.oob_status.clone();
        let c = ctx.clone();
        self.runtime.spawn(async move {
            let res = execute_oob_action(&target, action).await;
            let _ = tx.send((host_id, res));
            c.request_repaint();
        });
    }

    pub fn render_comms_and_power_windows(&mut self, ctx: &Context) {
        let mut close_headless = Vec::new();
        for idx in 0..self.sessions.len() {
            let sid = self.sessions[idx].id;
            let title = self.sessions[idx].title.clone();
            let connected = matches!(self.sessions[idx].phase, ConnectionPhase::Connected);

            // Text Chat Window (Channel 0x09)
            if self.sessions[idx].chat_open {
                let mut open = true;
                Window::new(format!("Text Chat - {title}##chat_{sid}"))
                    .open(&mut open)
                    .default_size([460.0, 360.0])
                    .show(ctx, |ui| {
                        let s = &mut self.sessions[idx];
                        s.chat_unread = 0;
                        ui.horizontal(|ui| {
                            ui.label(if connected { "Connected" } else { "Waiting for authentication..." });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Save Transcript").clicked()
                                    && let Ok(path) = save_chat_transcript(&s.title, &s.chat_history)
                                {
                                    s.status_banner = format!("Saved chat to {}", path.display());
                                }
                            });
                        });
                        ui.separator();
                        ScrollArea::vertical()
                            .id_salt(("chat_scroll", sid))
                            .stick_to_bottom(true)
                            .max_height(240.0)
                            .show(ui, |ui| {
                                for msg in &s.chat_history {
                                    let col = if msg.outgoing {
                                        Color32::from_rgb(130, 195, 255)
                                    } else {
                                        Color32::from_rgb(175, 235, 165)
                                    };
                                    ui.horizontal_wrapped(|ui| {
                                        ui.weak(format_epoch_hms(msg.timestamp));
                                        ui.colored_label(col, format!("{}:", msg.sender));
                                        ui.label(&msg.text);
                                    });
                                }
                            });
                        ui.separator();
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut s.chat_input)
                                    .hint_text("Type a message...")
                                    .desired_width(ui.available_width() - 68.0),
                            );
                            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                            if (ui.button("Send").clicked() || enter)
                                && !s.chat_input.trim().is_empty()
                                && connected
                            {
                                let text = s.chat_input.trim().to_string();
                                s.chat_input.clear();
                                let ts = now_secs();
                                let sender = std::env::var("USERNAME")
                                    .or_else(|_| std::env::var("USER"))
                                    .unwrap_or_else(|_| "Administrator".into());
                                s.chat_history.push(ChatLine {
                                    sender: sender.clone(),
                                    timestamp: ts,
                                    text: text.clone(),
                                    outgoing: true,
                                });
                                s.send_cmd(SessionCommand::Comms(CommsMsg::ChatMessage {
                                    sender,
                                    timestamp: ts,
                                    text,
                                }));
                                resp.request_focus();
                            }
                        });
                    });
                if !open {
                    self.sessions[idx].chat_open = false;
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }

            // Voice Chat Window (Channel 0x09 + cpal)
            if self.sessions[idx].voice_open {
                let mut open = true;
                Window::new(format!("Voice Chat - {title}##voice_{sid}"))
                    .open(&mut open)
                    .default_size([360.0, 240.0])
                    .show(ctx, |ui| {
                        let s = &mut self.sessions[idx];
                        let ctrl = s.voice_controls.clone();
                        ui.label(&s.voice_status);
                        ui.separator();

                        let level = ctrl.mic_level_milli.load(Ordering::Relaxed) as f32 / 1000.0;
                        ui.add(
                            egui::ProgressBar::new((level * 4.0).clamp(0.0, 1.0))
                                .text(format!("Mic Level: {:.0}%", (level * 100.0).min(100.0))),
                        );

                        let mut muted = ctrl.muted.load(Ordering::Relaxed);
                        if ui.checkbox(&mut muted, "Mute Microphone").changed() {
                            ctrl.muted.store(muted, Ordering::Relaxed);
                        }
                        let mut ptt_req = ctrl.ptt_required.load(Ordering::Relaxed);
                        if ui.checkbox(&mut ptt_req, format!("Push-to-Talk ({})", self.settings.ptt_hotkey)).changed() {
                            ctrl.ptt_required.store(ptt_req, Ordering::Relaxed);
                        }
                        if ptt_req {
                            let held = ui.add(egui::Button::new("Hold to Talk").sense(egui::Sense::click_and_drag())).is_pointer_button_down_on()
                                || key_str_down(ui, &self.settings.ptt_hotkey);
                            ctrl.ptt_active.store(held, Ordering::Relaxed);
                        }

                        let mut vol = ctrl.speaker_vol_milli.load(Ordering::Relaxed) as f32 / 1000.0;
                        if ui.add(egui::Slider::new(&mut vol, 0.0..=2.0).text("Speaker Volume")).changed() {
                            ctrl.speaker_vol_milli.store((vol * 1000.0).round() as u32, Ordering::Relaxed);
                        }
                        let mut gate = ctrl.noise_gate_milli.load(Ordering::Relaxed) as f32 / 1000.0;
                        if ui.add(egui::Slider::new(&mut gate, 0.0..=0.2).text("Noise Gate")).changed() {
                            ctrl.noise_gate_milli.store((gate * 1000.0).round() as u32, Ordering::Relaxed);
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            if !s.voice_active {
                                if ui.add_enabled(connected, egui::Button::new("Start Voice Call")).clicked() {
                                    ctrl.noise_gate_milli.store((self.settings.noise_gate * 1000.0).round() as u32, Ordering::Relaxed);
                                    s.send_cmd(SessionCommand::StartVoice {
                                        input_device: Some(self.settings.mic_device.clone()),
                                        output_device: Some(self.settings.speaker_device.clone()),
                                    });
                                }
                            } else if ui.button("End Call").clicked() {
                                s.send_cmd(SessionCommand::StopVoice);
                            }
                        });
                    });
                if !open {
                    self.sessions[idx].voice_open = false;
                    if self.sessions[idx].voice_active {
                        self.sessions[idx].send_cmd(SessionCommand::StopVoice);
                    }
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }

            // Send Message Window (Channel 0x09 one-shot desktop popup)
            if self.sessions[idx].popup_open {
                let mut open = true;
                Window::new(format!("Send Message - {title}##popup_{sid}"))
                    .open(&mut open)
                    .default_size([420.0, 260.0])
                    .show(ctx, |ui| {
                        let s = &mut self.sessions[idx];
                        ui.label("Title:");
                        ui.text_edit_singleline(&mut s.popup_title);
                        ui.label("Message:");
                        ui.add(
                            egui::TextEdit::multiline(&mut s.popup_body)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        ui.checkbox(&mut s.popup_urgent, "Urgent warning alert");
                        if !s.popup_status.is_empty() {
                            ui.label(&s.popup_status);
                        }
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    connected && !s.popup_body.trim().is_empty(),
                                    egui::Button::new("Send Notification"),
                                )
                                .clicked()
                            {
                                let t = if s.popup_title.trim().is_empty() {
                                    "DeskBolt Notice".to_string()
                                } else {
                                    s.popup_title.trim().to_string()
                                };
                                let b = s.popup_body.trim().to_string();
                                let urgent = s.popup_urgent;
                                s.popup_status = "Delivering message...".into();
                                s.send_cmd(SessionCommand::Comms(CommsMsg::PopupMessage {
                                    title: t,
                                    body: b,
                                    urgent,
                                }));
                            }
                        });
                    });
                if !open {
                    self.sessions[idx].popup_open = false;
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }

            // Remote Power Control Window (Channel 0x0A)
            if self.sessions[idx].power_open {
                let mut open = true;
                Window::new(format!("Shutdown & Power Control - {title}##pwr_{sid}"))
                    .open(&mut open)
                    .default_size([380.0, 310.0])
                    .show(ctx, |ui| {
                        let s = &mut self.sessions[idx];
                        ui.label(if connected {
                            "Select remote operating system power action:"
                        } else {
                            "Waiting for host connection..."
                        });
                        ui.checkbox(&mut s.power_force, "Force open applications to close");
                        ui.horizontal(|ui| {
                            ui.label("Countdown Delay (sec):");
                            let mut d = s.power_delay_secs as i32;
                            if ui.add(egui::DragValue::new(&mut d).range(0..=600)).changed() {
                                s.power_delay_secs = d.clamp(0, 600) as u32;
                            }
                        });
                        ui.separator();
                        let actions = [
                            (PowerAction::Lock, "Lock Workstation"),
                            (PowerAction::LogOff, "Log Off Current User"),
                            (PowerAction::Sleep, "Sleep"),
                            (PowerAction::Hibernate, "Hibernate"),
                            (PowerAction::Restart, "Restart"),
                            (PowerAction::Shutdown, "Shut Down"),
                        ];
                        for (act, label) in actions {
                            if ui
                                .add_enabled(connected, egui::Button::new(label).min_size(egui::vec2(220.0, 26.0)))
                                .clicked()
                            {
                                s.power_status = format!("Sending {label}...");
                                s.send_cmd(SessionCommand::Power(PowerMsg::PowerCommand {
                                    action: act,
                                    force: s.power_force,
                                    delay_secs: s.power_delay_secs,
                                }));
                            }
                        }
                        if !s.power_status.is_empty() {
                            ui.separator();
                            ui.label(&s.power_status);
                        }
                    });
                if !open {
                    self.sessions[idx].power_open = false;
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }
        }
        self.close_headless_session_ids(&close_headless);
    }

    pub fn render_phase8_modals(&mut self, ctx: &Context) {
        // 1. Per-Host Tabbed Properties (Alt+Enter)
        if let Some(mut modal) = self.props_modal.take() {
            let mut keep_open = true;
            Window::new(if modal.id.is_some() {
                format!("Host Properties - {}", modal.name)
            } else {
                "New Host Connection".into()
            })
            .collapsible(false)
            .default_size([560.0, 420.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut modal.tab, HostPropertiesTab::General, "General");
                    ui.selectable_value(&mut modal.tab, HostPropertiesTab::DisplayMode, "Display & Mode");
                    ui.selectable_value(&mut modal.tab, HostPropertiesTab::WakeOnLan, "Wake-on-LAN");
                    ui.selectable_value(&mut modal.tab, HostPropertiesTab::OobHardware, "Out-of-Band Hardware");
                    ui.selectable_value(&mut modal.tab, HostPropertiesTab::PortTunnels, "Port Tunnels");
                });
                ui.separator();
                ScrollArea::vertical().max_height(310.0).show(ui, |ui| {
                    match modal.tab {
                        HostPropertiesTab::General => {
                            egui::Grid::new("prop_gen").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                                ui.label("Display Name:");
                                ui.text_edit_singleline(&mut modal.name);
                                ui.end_row();
                                ui.label("Host / IP:");
                                ui.text_edit_singleline(&mut modal.host);
                                ui.end_row();
                                ui.label("Protocol:");
                                egui::ComboBox::from_id_salt("prop_proto")
                                    .selected_text(modal.protocol_type.label())
                                    .show_ui(ui, |ui| {
                                        for p in HostProtocol::ALL {
                                            let prev_def = modal.protocol_type.default_port().to_string();
                                            if ui.selectable_value(&mut modal.protocol_type, p, p.label()).clicked()
                                                && (modal.port.trim().is_empty() || modal.port.trim() == prev_def)
                                            {
                                                modal.port = p.default_port().to_string();
                                            }
                                        }
                                    });
                                ui.end_row();
                                ui.label(if modal.protocol_type == HostProtocol::Serial { "Baud Rate:" } else { "Port:" });
                                ui.text_edit_singleline(&mut modal.port);
                                ui.end_row();
                                ui.label("SSH Key Path:");
                                ui.add(egui::TextEdit::singleline(&mut modal.ssh_key_path).hint_text("Optional private key path"));
                                ui.end_row();
                                ui.label("Notes:");
                                ui.add(egui::TextEdit::multiline(&mut modal.notes).desired_rows(3));
                                ui.end_row();
                            });
                        }
                        HostPropertiesTab::DisplayMode => {
                            egui::Grid::new("prop_disp").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                                ui.label("Default Session Mode:");
                                egui::ComboBox::from_id_salt("prop_mode")
                                    .selected_text(match modal.default_mode {
                                        SessionMode::FullControl => "Full Control",
                                        SessionMode::ViewOnly => "View Only",
                                        SessionMode::FileTransfer => "File Transfer",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut modal.default_mode, SessionMode::FullControl, "Full Control");
                                        ui.selectable_value(&mut modal.default_mode, SessionMode::ViewOnly, "View Only");
                                        ui.selectable_value(&mut modal.default_mode, SessionMode::FileTransfer, "File Transfer");
                                    });
                                ui.end_row();
                                ui.label("Override Quality:");
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut modal.custom_quality_override, "Custom:");
                                    let q_target = if modal.custom_quality_override {
                                        &mut modal.custom_quality
                                    } else {
                                        &mut modal.quality_mode
                                    };
                                    egui::ComboBox::from_id_salt("prop_q")
                                        .selected_text(match *q_target {
                                            VideoQualityMode::Auto => "Auto",
                                            VideoQualityMode::CrispText => "Crisp Text",
                                            VideoQualityMode::SmoothVideo => "Motion Video",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(q_target, VideoQualityMode::Auto, "Auto");
                                            ui.selectable_value(q_target, VideoQualityMode::CrispText, "Crisp Text");
                                            ui.selectable_value(q_target, VideoQualityMode::SmoothVideo, "Motion Video");
                                        });
                                });
                                ui.end_row();
                                ui.label("Custom Max FPS:");
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut modal.custom_fps_override, "Override (15-120):");
                                    ui.add_enabled(
                                        modal.custom_fps_override,
                                        egui::TextEdit::singleline(&mut modal.custom_fps).desired_width(60.0),
                                    );
                                });
                                ui.end_row();
                            });
                        }
                        HostPropertiesTab::WakeOnLan => {
                            egui::Grid::new("prop_wol").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                                ui.label("Target MAC Address:");
                                ui.add(egui::TextEdit::singleline(&mut modal.mac_address).hint_text("AA:BB:CC:DD:EE:FF"));
                                ui.end_row();
                            });
                            ui.label("Used by Wake-on-LAN actions to power on dormant hardware.");
                        }
                        HostPropertiesTab::OobHardware => {
                            egui::Grid::new("prop_oob").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                                ui.label("Enable Out-of-Band:");
                                ui.checkbox(&mut modal.oob_enabled, "Enable Out-of-Band Hardware Control");
                                ui.end_row();
                                ui.label("Management Host/IP:");
                                ui.add(egui::TextEdit::singleline(&mut modal.oob_host).hint_text("Leave blank to use main Host/IP"));
                                ui.end_row();
                                ui.label("HTTP Port:");
                                ui.text_edit_singleline(&mut modal.oob_port);
                                ui.end_row();
                                ui.label("TLS Management:");
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut modal.oob_use_tls, "Use TLS Port:");
                                    ui.add_enabled(
                                        modal.oob_use_tls,
                                        egui::TextEdit::singleline(&mut modal.oob_tls_port).desired_width(70.0),
                                    );
                                });
                                ui.end_row();
                                ui.label("Username:");
                                ui.text_edit_singleline(&mut modal.oob_username);
                                ui.end_row();
                                ui.label("Password:");
                                ui.add(egui::TextEdit::singleline(&mut modal.oob_password).password(true));
                                ui.end_row();
                            });
                        }
                        HostPropertiesTab::PortTunnels => {
                            ui.label("Saved Tunnel Definitions (JSON / Rule Notes):");
                            ui.add(
                                egui::TextEdit::multiline(&mut modal.saved_tunnels_json)
                                    .desired_rows(5)
                                    .desired_width(f32::INFINITY),
                            );
                        }
                    }
                });
                if let Some(err) = &modal.error {
                    ui.colored_label(Color32::RED, err);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match modal.port.trim().parse::<u16>() {
                            Ok(port) if port > 0 => {
                                let cfps = if modal.custom_fps_override {
                                    modal.custom_fps.trim().parse::<u8>().ok().map(|f| f.clamp(15, 120))
                                } else {
                                    None
                                };
                                let oob_p = modal.oob_port.trim().parse::<u16>().unwrap_or(16992).max(1);
                                let oob_tp = if modal.oob_use_tls {
                                    modal.oob_tls_port.trim().parse::<u16>().unwrap_or(16993).max(1)
                                } else {
                                    0
                                };
                                match self.db.upsert_host(&HostInput {
                                    id: modal.id,
                                    folder_id: modal.folder_id,
                                    name: modal.name.clone(),
                                    host: modal.host.clone(),
                                    port,
                                    default_mode: modal.default_mode,
                                    quality_mode: modal.quality_mode,
                                    mac_address: Some(modal.mac_address.clone()),
                                    protocol_type: modal.protocol_type,
                                    ssh_key_path: Some(modal.ssh_key_path.clone()),
                                    saved_tunnels_json: Some(modal.saved_tunnels_json.clone()),
                                    oob_enabled: modal.oob_enabled,
                                    oob_host: Some(modal.oob_host.clone()),
                                    oob_port: oob_p,
                                    oob_tls: oob_tp,
                                    oob_username: Some(modal.oob_username.clone()),
                                    oob_password: Some(modal.oob_password.clone()),
                                    custom_fps: cfps,
                                    custom_quality: modal.custom_quality_override.then_some(modal.custom_quality),
                                    notes: Some(modal.notes.clone()),
                                }) {
                                    Ok(_) => {
                                        keep_open = false;
                                        self.reload_phonebook();
                                    }
                                    Err(e) => modal.error = Some(e),
                                }
                            }
                            _ => modal.error = Some("Enter a valid port number (1-65535).".into()),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }
                });
            });
            if keep_open {
                self.props_modal = Some(modal);
            }
        }

        // 2. Tools -> Options (Viewer Preferences)
        if let Some(mut opt) = self.options_modal.take() {
            let mut keep_open = true;
            Window::new("DeskBolt Viewer Options")
                .collapsible(false)
                .default_size([580.0, 440.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut opt.tab, OptionsTab::Connection, "Connection");
                        ui.selectable_value(&mut opt.tab, OptionsTab::DisplayInput, "Display & Input");
                        ui.selectable_value(&mut opt.tab, OptionsTab::AudioChat, "Audio & Chat");
                        ui.selectable_value(&mut opt.tab, OptionsTab::HotkeysAppearance, "Hotkeys & Appearance");
                    });
                    ui.separator();
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| match opt.tab {
                        OptionsTab::Connection => {
                            egui::Grid::new("opt_conn").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                                ui.label("Default Mode:");
                                egui::ComboBox::from_id_salt("opt_mode")
                                    .selected_text(match opt.draft.default_mode {
                                        SessionMode::FullControl => "Full Control",
                                        SessionMode::ViewOnly => "View Only",
                                        SessionMode::FileTransfer => "File Transfer",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut opt.draft.default_mode, SessionMode::FullControl, "Full Control");
                                        ui.selectable_value(&mut opt.draft.default_mode, SessionMode::ViewOnly, "View Only");
                                        ui.selectable_value(&mut opt.draft.default_mode, SessionMode::FileTransfer, "File Transfer");
                                    });
                                ui.end_row();
                                ui.label("Default Quality:");
                                egui::ComboBox::from_id_salt("opt_qual")
                                    .selected_text(match opt.draft.default_quality {
                                        VideoQualityMode::Auto => "Auto",
                                        VideoQualityMode::CrispText => "Crisp Text",
                                        VideoQualityMode::SmoothVideo => "Motion Video",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut opt.draft.default_quality, VideoQualityMode::Auto, "Auto");
                                        ui.selectable_value(&mut opt.draft.default_quality, VideoQualityMode::CrispText, "Crisp Text");
                                        ui.selectable_value(&mut opt.draft.default_quality, VideoQualityMode::SmoothVideo, "Motion Video");
                                    });
                                ui.end_row();
                                ui.label("Color Depth:");
                                ui.horizontal(|ui| {
                                    ui.selectable_value(&mut opt.draft.color_depth, 32, "32-bit True Color");
                                    ui.selectable_value(&mut opt.draft.color_depth, 16, "16-bit Bandwidth Saver");
                                });
                                ui.end_row();
                                ui.label("Connection Timeout:");
                                let mut to = opt.draft.connect_timeout_secs as i32;
                                if ui.add(egui::DragValue::new(&mut to).range(2..=120).suffix(" sec")).changed() {
                                    opt.draft.connect_timeout_secs = to.clamp(2, 120) as u16;
                                }
                                ui.end_row();
                                ui.label("Auto-Reconnect Attempts:");
                                let mut att = opt.draft.auto_reconnect_attempts as i32;
                                if ui.add(egui::DragValue::new(&mut att).range(0..=20)).changed() {
                                    opt.draft.auto_reconnect_attempts = att.clamp(0, 20) as u8;
                                }
                                ui.end_row();
                                ui.label("Reconnect Retry Delay:");
                                let mut del = opt.draft.auto_reconnect_delay_secs as i32;
                                if ui.add(egui::DragValue::new(&mut del).range(1..=60).suffix(" sec")).changed() {
                                    opt.draft.auto_reconnect_delay_secs = del.clamp(1, 60) as u8;
                                }
                                ui.end_row();
                            });
                        }
                        OptionsTab::DisplayInput => {
                            egui::Grid::new("opt_disp").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                                ui.label("Default Viewport Scaling:");
                                egui::ComboBox::from_id_salt("opt_scale")
                                    .selected_text(opt.draft.scale_mode.label())
                                    .show_ui(ui, |ui| {
                                        for m in ScaleMode::ALL {
                                            ui.selectable_value(&mut opt.draft.scale_mode, m, m.label());
                                        }
                                    });
                                ui.end_row();
                                ui.label("Texture Filter:");
                                egui::ComboBox::from_id_salt("opt_filter")
                                    .selected_text(opt.draft.texture_filter.label())
                                    .show_ui(ui, |ui| {
                                        for m in TextureFilterMode::ALL {
                                            ui.selectable_value(&mut opt.draft.texture_filter, m, m.label());
                                        }
                                    });
                                ui.end_row();
                                ui.label("System Shortcut Capture:");
                                egui::ComboBox::from_id_salt("opt_syskeys")
                                    .selected_text(opt.draft.sys_keys_mode.label())
                                    .show_ui(ui, |ui| {
                                        for m in SystemKeysMode::ALL {
                                            ui.selectable_value(&mut opt.draft.sys_keys_mode, m, m.label());
                                        }
                                    });
                                ui.end_row();
                                ui.label("Cursor Sync:");
                                ui.checkbox(&mut opt.draft.remote_cursor_sync, "Synchronize remote cursor shape & visibility");
                                ui.end_row();
                                ui.label("Clipboard limit (bytes)");
                                ui.add(egui::DragValue::new(&mut opt.draft.clipboard_max_bytes)
                                    .range(0..=deskbolt_proto::MAX_CLIPBOARD_BYTES).speed(1024));
                                ui.end_row();
                                #[cfg(not(target_os = "android"))]
                                {
                                    ui.label("Clipboard typing delay (ms)");
                                    ui.add(egui::DragValue::new(&mut opt.draft.clipboard_key_delay_ms).range(1..=1000));
                                    ui.end_row();
                                }
                                ui.label("Multi-Monitor Mode:");
                                egui::ComboBox::from_id_salt("opt_multimon")
                                    .selected_text(opt.draft.multimonitor_mode.label())
                                    .show_ui(ui, |ui| {
                                        for m in MultimonitorViewMode::ALL {
                                            ui.selectable_value(&mut opt.draft.multimonitor_mode, m, m.label());
                                        }
                                    });
                                ui.end_row();
                            });
                        }
                        OptionsTab::AudioChat => {
                            egui::Grid::new("opt_audio").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                                ui.label("Microphone Input Device:");
                                egui::ComboBox::from_id_salt("opt_mic")
                                    .selected_text(&opt.draft.mic_device)
                                    .show_ui(ui, |ui| {
                                        for dev in &opt.input_devices {
                                            ui.selectable_value(&mut opt.draft.mic_device, dev.clone(), dev);
                                        }
                                    });
                                ui.end_row();
                                ui.label("Speaker Output Device:");
                                egui::ComboBox::from_id_salt("opt_spk")
                                    .selected_text(&opt.draft.speaker_device)
                                    .show_ui(ui, |ui| {
                                        for dev in &opt.output_devices {
                                            ui.selectable_value(&mut opt.draft.speaker_device, dev.clone(), dev);
                                        }
                                    });
                                ui.end_row();
                                ui.label("Noise Gate Threshold:");
                                ui.add(egui::Slider::new(&mut opt.draft.noise_gate, 0.0..=0.2));
                                ui.end_row();
                                ui.label("Push-to-Talk Key:");
                                ui.add(egui::TextEdit::singleline(&mut opt.draft.ptt_hotkey).desired_width(100.0));
                                ui.end_row();
                                ui.label("Chat Alerts:");
                                ui.checkbox(&mut opt.draft.chat_sound, "Play sound notification on incoming chat message");
                                ui.end_row();
                                ui.label("Transcripts:");
                                ui.checkbox(&mut opt.draft.auto_save_chat, "Auto-save chat transcripts on disconnect");
                                ui.end_row();
                            });
                        }
                        OptionsTab::HotkeysAppearance => {
                            egui::Grid::new("opt_keys").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                                ui.label("Toggle Fullscreen:");
                                ui.text_edit_singleline(&mut opt.draft.hotkey_fullscreen);
                                ui.end_row();
                                ui.label("Send Ctrl+Alt+Del:");
                                ui.text_edit_singleline(&mut opt.draft.hotkey_cad);
                                ui.end_row();
                                #[cfg(not(target_os = "android"))]
                                {
                                    ui.label("Type Clipboard");
                                    ui.text_edit_singleline(&mut opt.draft.hotkey_type_clipboard);
                                    ui.end_row();
                                }
                                ui.label("Next Monitor:");
                                ui.text_edit_singleline(&mut opt.draft.hotkey_next_monitor);
                                ui.end_row();
                                ui.label("Capture Screenshot:");
                                ui.text_edit_singleline(&mut opt.draft.hotkey_screenshot);
                                ui.end_row();
                                ui.label("Mute Microphone:");
                                ui.text_edit_singleline(&mut opt.draft.hotkey_mute_mic);
                                ui.end_row();
                                ui.label("Theme:");
                                egui::ComboBox::from_id_salt("opt_theme")
                                    .selected_text(opt.draft.theme.label())
                                    .show_ui(ui, |ui| {
                                        for t in ThemeChoice::ALL {
                                            ui.selectable_value(&mut opt.draft.theme, t, t.label());
                                        }
                                    });
                                ui.end_row();
                                ui.label("Startup View:");
                                egui::ComboBox::from_id_salt("opt_startup")
                                    .selected_text(opt.draft.startup_view.label())
                                    .show_ui(ui, |ui| {
                                        for sv in StartupView::ALL {
                                            ui.selectable_value(&mut opt.draft.startup_view, sv, sv.label());
                                        }
                                    });
                                ui.end_row();
                            });
                        }
                    });
                    if !opt.status.is_empty() {
                        ui.colored_label(Color32::RED, &opt.status);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            match self.db.save_settings(&opt.draft) {
                                Ok(()) => {
                                    self.settings = opt.draft.clone();
                                    self.apply_theme(ctx);
                                    self.status_line = "Viewer options saved.".into();
                                    keep_open = false;
                                }
                                Err(e) => opt.status = e,
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
            if keep_open {
                self.options_modal = Some(opt);
            }
        }

        // 3. Inline Host Rename (F2)
        if let Some((hid, mut name)) = self.rename_host_modal.take() {
            let mut keep_open = true;
            Window::new("Rename Host")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Connection Name:");
                    let r = ui.text_edit_singleline(&mut name);
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.horizontal(|ui| {
                        if (ui.button("Rename").clicked() || enter)
                            && self.db.rename_host(hid, &name).is_ok()
                        {
                            self.reload_phonebook();
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
            if keep_open {
                self.rename_host_modal = Some((hid, name));
            }
        }

        // 4. Out-of-Band Hardware Control Console
        if let Some(mut oob) = self.oob_dialog.take() {
            let mut keep_open = true;
            Window::new(format!("Out-of-Band Hardware Control - {}", oob.label))
                .collapsible(false)
                .default_size([460.0, 320.0])
                .show(ctx, |ui| {
                    egui::Grid::new("oob_dlg_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("Management Host / IP:");
                        ui.text_edit_singleline(&mut oob.host);
                        ui.end_row();
                        ui.label("HTTP Port:");
                        ui.text_edit_singleline(&mut oob.port);
                        ui.end_row();
                        ui.label("TLS Management:");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut oob.use_tls, "TLS Port:");
                            ui.add_enabled(oob.use_tls, egui::TextEdit::singleline(&mut oob.tls_port).desired_width(60.0));
                        });
                        ui.end_row();
                        ui.label("Username:");
                        ui.text_edit_singleline(&mut oob.username);
                        ui.end_row();
                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut oob.password).password(true));
                        ui.end_row();
                    });
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        let tgt = OobTarget {
                            host: oob.host.clone(),
                            port: oob.port.trim().parse::<u16>().unwrap_or(16992),
                            tls_port: oob.tls_port.trim().parse::<u16>().unwrap_or(16993),
                            use_tls: oob.use_tls,
                            username: oob.username.clone(),
                            password: oob.password.clone(),
                        };
                        for act in OobAction::ALL_POWER_AND_BOOT {
                            if ui.button(act.label()).clicked() {
                                self.dispatch_oob_target_action(ctx, oob.host_id, tgt.clone(), act);
                            }
                        }
                        if ui.button("BIOS Remote Control (SOL)").clicked() {
                            let id = self.admin.next_local_id;
                            self.admin.next_local_id += 1;
                            let c = ctx.clone();
                            let (tx, rx) = crate::oob::spawn_oob_sol_terminal(tgt, &self.runtime, move || c.request_repaint());
                            self.admin.direct_terminals.push(crate::admin_ui::DirectTerminalTab {
                                id,
                                host_id: oob.host_id,
                                title: format!("BIOS Console - {}", oob.label),
                                is_serial: true,
                                host: oob.host.clone(),
                                port: oob.port.trim().parse::<u16>().unwrap_or(16994),
                                username: String::new(),
                                password: String::new(),
                                key_path: String::new(),
                                passphrase: String::new(),
                                pinned_fp: None,
                                pending_tofu_fp: None,
                                mismatch_fps: None,
                                need_auth: false,
                                status: "Connecting Serial-over-LAN...".into(),
                                connected: false,
                                emu: crate::terminal::TerminalEmulator::new(100, 30),
                                cmd_tx: Some(tx),
                                ev_rx: Some(rx),
                                serial_port: String::new(),
                                baud_rate: 115200,
                            });
                        }
                    });
                    if !self.admin.oob_status.is_empty() {
                        ui.separator();
                        ui.label(&self.admin.oob_status);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            keep_open = false;
                        }
                    });
                });
            if keep_open {
                self.oob_dialog = Some(oob);
            }
        }

        // 5. Help -> Keyboard Shortcuts
        if self.show_shortcuts_help {
            let mut open = true;
            Window::new("Keyboard Shortcuts")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::Grid::new("shortcuts_help_grid").striped(true).num_columns(2).spacing([20.0, 6.0]).show(ui, |ui| {
                        ui.strong("F2"); ui.label("Rename selected host connection"); ui.end_row();
                        ui.strong("Del"); ui.label("Delete selected host connection"); ui.end_row();
                        ui.strong("Alt+Enter"); ui.label("Open tabbed host properties"); ui.end_row();
                        ui.strong("Ctrl+F"); ui.label("Focus address book search bar"); ui.end_row();
                        ui.strong("Ctrl+Alt+Home"); ui.label("Release local system key capture"); ui.end_row();
                        ui.strong(&self.settings.hotkey_fullscreen); ui.label("Toggle session fullscreen"); ui.end_row();
                        ui.strong(&self.settings.hotkey_cad); ui.label("Send Ctrl+Alt+Del (in session)"); ui.end_row();
                        ui.strong(&self.settings.hotkey_next_monitor); ui.label("Cycle active remote monitor"); ui.end_row();
                        ui.strong(&self.settings.hotkey_mute_mic); ui.label("Mute / unmute voice microphone"); ui.end_row();
                    });
                    if ui.button("Close").clicked() {
                        self.show_shortcuts_help = false;
                    }
                });
            if !open {
                self.show_shortcuts_help = false;
            }
        }
    }
}

fn save_chat_transcript(title: &str, lines: &[ChatLine]) -> Result<std::path::PathBuf, String> {
    let dir = crate::shortcuts::desktop_dir().unwrap_or_else(|_| std::env::temp_dir());
    let slug = crate::shortcuts::sanitize_shortcut_filename(title);
    let out = dir.join(format!("DeskBolt-Chat-{slug}-{}.txt", now_secs()));
    let mut text = String::new();
    for l in lines {
        use std::fmt::Write as _;
        let _ = writeln!(&mut text, "[{}] {}: {}", format_epoch_hms(l.timestamp), l.sender, l.text);
    }
    std::fs::write(&out, text).map_err(|e| format!("save transcript failed: {e}"))?;
    Ok(out)
}

fn format_epoch_hms(epoch_secs: u64) -> String {
    let day = epoch_secs % 86400;
    let h = day / 3600;
    let m = (day % 3600) / 60;
    let s = day % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn key_str_down(ui: &egui::Ui, raw: &str) -> bool {
    let k = match raw.trim().to_ascii_uppercase().as_str() {
        "V" => Key::V,
        "B" => Key::B,
        "T" => Key::T,
        "SPACE" => Key::Space,
        _ => Key::V,
    };
    ui.input(|i| i.key_down(k))
}
