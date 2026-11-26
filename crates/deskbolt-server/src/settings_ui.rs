use std::path::{Path, PathBuf};

use deskbolt_proto::VideoQualityMode;
use eframe::egui;

use crate::config::ServerConfig;
use crate::platform::set_os_autostart;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    NetworkGeneral,
    SecurityAccess,
    Permissions,
    CapturePrivacy,
}

pub struct ServerSettingsApp {
    data_dir: PathBuf,
    cfg: ServerConfig,
    allow_text: String,
    block_text: String,
    audit_path_text: String,
    new_password: String,
    confirm_password: String,
    tab: SettingsTab,
    status: String,
    status_ok: bool,
}

impl ServerSettingsApp {
    pub fn load(data_dir: &Path) -> Self {
        let cfg = ServerConfig::load_or_default(data_dir)
            .unwrap_or_else(|_| ServerConfig::for_dir(data_dir));
        let allow_text = cfg.ip_allowlist.join(", ");
        let block_text = cfg.ip_blocklist.join(", ");
        let audit_path_text = cfg
            .audit_log_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            data_dir: data_dir.to_path_buf(),
            cfg,
            allow_text,
            block_text,
            audit_path_text,
            new_password: String::new(),
            confirm_password: String::new(),
            tab: SettingsTab::NetworkGeneral,
            status: format!(
                "Loaded {}",
                ServerConfig::config_path_in(data_dir).display()
            ),
            status_ok: true,
        }
    }

    fn apply_and_save(&mut self) -> bool {
        if self.cfg.port == 0 {
            self.status = "Listening TCP port must be between 1 and 65535.".into();
            self.status_ok = false;
            return false;
        }
        if !self.new_password.is_empty() || !self.confirm_password.is_empty() {
            if self.new_password != self.confirm_password {
                self.status = "New password fields do not match.".into();
                self.status_ok = false;
                return false;
            }
            if let Err(e) = self.cfg.set_password_bytes(self.new_password.as_bytes()) {
                self.status = format!("Password update failed: {e}");
                self.status_ok = false;
                return false;
            }
            self.new_password.clear();
            self.confirm_password.clear();
        }

        self.cfg.ip_allowlist = split_rules(&self.allow_text);
        self.cfg.ip_blocklist = split_rules(&self.block_text);
        let audit_trimmed = self.audit_path_text.trim();
        self.cfg.audit_log_path = if audit_trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(audit_trimmed))
        };
        self.cfg.max_fps = self.cfg.max_fps.clamp(15, 120);
        self.cfg.lockout_threshold = self.cfg.lockout_threshold.max(1);
        self.cfg.ban_duration_secs = self.cfg.ban_duration_secs.max(1);
        self.cfg.confirm_timeout_secs = self.cfg.confirm_timeout_secs.clamp(3, 120);

        if let Err(e) = self.cfg.save(&self.data_dir) {
            self.status = format!("Save failed: {e}");
            self.status_ok = false;
            return false;
        }
        if let Err(error) = set_os_autostart(self.cfg.start_with_os) {
            self.status = format!("Host settings saved. Automatic startup could not be changed: {error}");
            self.status_ok = false;
            return false;
        }
        self.status = "Host settings saved. Restart the host to apply changes.".into();
        self.status_ok = true;
        true
    }
}

impl eframe::App for ServerSettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("srv_settings_tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("DeskBolt Host Settings");
                ui.separator();
                ui.selectable_value(&mut self.tab, SettingsTab::NetworkGeneral, "Network & General");
                ui.selectable_value(&mut self.tab, SettingsTab::SecurityAccess, "Security & Access");
                ui.selectable_value(&mut self.tab, SettingsTab::Permissions, "Feature Permissions");
                ui.selectable_value(&mut self.tab, SettingsTab::CapturePrivacy, "Capture & Privacy");
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("srv_settings_footer").show(ui, |ui| {
            ui.horizontal(|ui| {
                let col = if self.status_ok {
                    egui::Color32::from_rgb(70, 185, 105)
                } else {
                    egui::Color32::from_rgb(220, 80, 80)
                };
                ui.colored_label(col, &self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Save & Close").clicked() && self.apply_and_save() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Apply").clicked() {
                        self.apply_and_save();
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.tab {
                    SettingsTab::NetworkGeneral => {
                        egui::Grid::new("srv_net_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                            ui.label("Bind IP / Interface:");
                            ui.add(egui::TextEdit::singleline(&mut self.cfg.bind_addr).hint_text("0.0.0.0"));
                            ui.end_row();
                            ui.label("Listening TCP Port:");
                            let mut port_val = self.cfg.port as i32;
                            if ui.add(egui::DragValue::new(&mut port_val).range(1..=65535)).changed() {
                                self.cfg.port = port_val.clamp(1, 65535) as u16;
                            }
                            ui.end_row();
                            ui.label("Startup:");
                            ui.checkbox(&mut self.cfg.start_with_os, "Start host automatically with operating system");
                            ui.end_row();
                            ui.label("System Tray:");
                            ui.checkbox(&mut self.cfg.show_tray_icon, "Show system tray icon in interactive mode");
                            ui.end_row();
                            ui.label("Controller Lock:");
                            ui.checkbox(&mut self.cfg.single_controller_lock, "Allow only one Full Control viewer at a time");
                            ui.end_row();
                        });
                    }
                    SettingsTab::SecurityAccess => {
                        egui::Grid::new("srv_sec_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                            ui.label("Change Password:");
                            ui.add(egui::TextEdit::singleline(&mut self.new_password).password(true).hint_text("Leave blank to keep current"));
                            ui.end_row();
                            ui.label("Confirm Password:");
                            ui.add(egui::TextEdit::singleline(&mut self.confirm_password).password(true));
                            ui.end_row();
                            ui.label("IP Allowlist (CIDR):");
                            ui.add(egui::TextEdit::singleline(&mut self.allow_text).hint_text("e.g. 192.168.1.0/24, 10.0.0.0/8 (blank = any)").desired_width(340.0));
                            ui.end_row();
                            ui.label("IP Blocklist (CIDR):");
                            ui.add(egui::TextEdit::singleline(&mut self.block_text).hint_text("e.g. 192.168.1.99").desired_width(340.0));
                            ui.end_row();
                            ui.label("Lockout Threshold:");
                            let mut thresh = self.cfg.lockout_threshold as i32;
                            if ui.add(egui::DragValue::new(&mut thresh).range(1..=50).suffix(" failed attempts")).changed() {
                                self.cfg.lockout_threshold = thresh.max(1) as u32;
                            }
                            ui.end_row();
                            ui.label("Ban Duration:");
                            let mut ban = self.cfg.ban_duration_secs as i32;
                            if ui.add(egui::DragValue::new(&mut ban).range(5..=3600).suffix(" seconds")).changed() {
                                self.cfg.ban_duration_secs = ban.clamp(5, 3600) as u16;
                            }
                            ui.end_row();
                            ui.label("User Permission:");
                            ui.checkbox(&mut self.cfg.confirm_connect, "Ask local desktop user permission before connecting");
                            ui.end_row();
                            ui.label("Prompt Timeout:");
                            let mut to = self.cfg.confirm_timeout_secs as i32;
                            if ui.add(egui::DragValue::new(&mut to).range(3..=120).suffix(" seconds")).changed() {
                                self.cfg.confirm_timeout_secs = to.clamp(3, 120) as u16;
                            }
                            ui.end_row();
                            ui.label("On Timeout:");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.cfg.confirm_default_allow, false, "Deny by Default");
                                ui.selectable_value(&mut self.cfg.confirm_default_allow, true, "Allow by Default");
                            });
                            ui.end_row();
                            ui.label("Access Restriction:");
                            ui.checkbox(&mut self.cfg.force_view_only, "Force all incoming desktop connections to View Only");
                            ui.end_row();
                            ui.label("Audit Log File:");
                            ui.add(egui::TextEdit::singleline(&mut self.audit_path_text).hint_text("Optional file path for security audit log").desired_width(340.0));
                            ui.end_row();
                        });
                    }
                    SettingsTab::Permissions => {
                        ui.strong("Enabled Remote Capabilities");
                        ui.add_space(6.0);
                        ui.checkbox(&mut self.cfg.permissions.full_control, "Allow Full Control (Keyboard & Mouse)");
                        ui.checkbox(&mut self.cfg.permissions.view_only, "Allow View Only Mode");
                        ui.checkbox(&mut self.cfg.permissions.file_transfer, "Allow File Transfer & Sync");
                        ui.checkbox(&mut self.cfg.permissions.terminal, "Allow Remote Terminal & Serial Console");
                        ui.checkbox(&mut self.cfg.permissions.power_control, "Allow Shutdown, Restart, Sleep & Lock");
                        ui.checkbox(&mut self.cfg.permissions.text_chat, "Allow Text Chat");
                        ui.checkbox(&mut self.cfg.permissions.voice_chat, "Allow Bidirectional Voice Chat");
                        ui.checkbox(&mut self.cfg.permissions.system_audio, "Allow System Audio Forwarding");
                        ui.checkbox(&mut self.cfg.permissions.send_message, "Allow Remote Desktop Popup Messages");
                        ui.checkbox(&mut self.cfg.permissions.tunnels, "Allow Encrypted Port Tunnels");
                    }
                    SettingsTab::CapturePrivacy => {
                        egui::Grid::new("srv_cap_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                            ui.label("Default Quality Preset:");
                            egui::ComboBox::from_id_salt("srv_def_quality")
                                .selected_text(match self.cfg.default_quality {
                                    VideoQualityMode::Auto => "Auto",
                                    VideoQualityMode::CrispText => "Crisp Text",
                                    VideoQualityMode::SmoothVideo => "Motion Video",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.cfg.default_quality, VideoQualityMode::Auto, "Auto");
                                    ui.selectable_value(&mut self.cfg.default_quality, VideoQualityMode::CrispText, "Crisp Text");
                                    ui.selectable_value(&mut self.cfg.default_quality, VideoQualityMode::SmoothVideo, "Motion Video");
                                });
                            ui.end_row();
                            ui.label("Maximum Frame Rate:");
                            let mut fps = self.cfg.max_fps as i32;
                            if ui.add(egui::Slider::new(&mut fps, 15..=120).suffix(" FPS")).changed() {
                                self.cfg.max_fps = fps.clamp(15, 120) as u8;
                            }
                            ui.end_row();
                            ui.label("Cursor Capture:");
                            ui.checkbox(&mut self.cfg.capture_cursor, "Include remote mouse cursor in capture");
                            ui.end_row();
                            ui.label("Privacy Curtain:");
                            ui.checkbox(&mut self.cfg.curtain_on_control, "Blank local monitor during active Full Control session");
                            ui.end_row();
                        });
                    }
                }
            });
        });
    }
}

fn split_rules(raw: &str) -> Vec<String> {
    raw.split([',', ';', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn run_settings_window(data_dir: &Path) -> Result<(), String> {
    let dir = data_dir.to_path_buf();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DeskBolt Host Settings")
            .with_inner_size([660.0, 480.0])
            .with_min_inner_size([520.0, 380.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DeskBolt Host Settings",
        options,
        Box::new(move |_cc| Ok(Box::new(ServerSettingsApp::load(&dir)))),
    )
    .map_err(|e| format!("failed to launch server settings UI: {e}"))
}
