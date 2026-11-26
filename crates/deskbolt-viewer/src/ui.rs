use std::path::PathBuf;
use std::sync::Arc;

use deskbolt_proto::{
    ClipboardSync, ControlMsg, FileTransferMsg, InputMsg, MouseBtn, PointerEvent, SessionMode,
    SpecialAction, SysAdminMsg, TerminalMsg, TransferDir, VideoQualityMode,
};
#[cfg(not(target_os = "android"))]
use deskbolt_proto::native_clipboard::NativeClipboard;
use egui::{
    Align, Color32, ColorImage, Context, Key, Layout, Pos2, Rect, ScrollArea, Sense, TextureHandle,
    TextureOptions, Vec2, Window,
};
#[cfg(not(target_os = "android"))]
use egui::{ViewportBuilder, ViewportId};

use crate::StartupConnectArgs;
use crate::admin_ui::{AdminUiState, NetToolTab};
use crate::comms_ui::{HostPropertiesModal, OobDialogState, OptionsModalState};
use crate::db::{
    FolderRow, HostProtocol, HostRow, PhonebookDb, ScaleMode, StartupView, SystemKeysMode,
    TextureFilterMode, ThemeChoice, ViewLayout, ViewerSettings,
};
use crate::hook::{KeyboardHookController, egui_key_to_key_event};
use crate::net_tools::send_wol_packet;
use crate::oob::OobAction;
use crate::session::{
    ConnectTarget, ConnectionPhase, LogTailBuffer, OverwritePrompt, SessionCommand,
    TransferItem, TransferItemStatus, ViewerSession,
};
use crate::sftp_sync::is_quick_edit_ext;
use crate::shortcuts::create_desktop_shortcut;
use crate::typing::TypingHotkey;

pub struct DeskBoltViewerApp {
    pub(crate) db: PhonebookDb,
    pub(crate) runtime: tokio::runtime::Handle,
    hook: KeyboardHookController,
    pub(crate) next_session_id: u64,

    // Address book & settings state
    folders: Vec<FolderRow>,
    pub(crate) hosts: Vec<HostRow>,
    pub(crate) selected_folder: Option<i64>,
    pub(crate) selected_host_id: Option<i64>,
    search_query: String,
    focus_search: bool,
    show_quick_connect: bool,
    quick_host: String,
    quick_port: String,
    quick_mode: SessionMode,
    pub(crate) status_line: String,
    pub(crate) settings: ViewerSettings,
    theme_initialized: bool,
    pending_startup: Option<StartupConnectArgs>,

    // Modals
    pub(crate) props_modal: Option<HostPropertiesModal>,
    pub(crate) options_modal: Option<OptionsModalState>,
    pub(crate) rename_host_modal: Option<(i64, String)>,
    pub(crate) oob_dialog: Option<OobDialogState>,
    pub(crate) show_shortcuts_help: bool,
    folder_modal: Option<FolderModal>,
    confirm_delete_host: Option<HostRow>,

    // Active sessions
    pub(crate) sessions: Vec<ViewerSession>,
    pub(crate) active_tab_id: Option<u64>,
    session_scales: std::collections::HashMap<u64, ScaleMode>,
    textures: std::collections::HashMap<u64, (u64, TextureOptions, TextureHandle)>,
    canvas_uploads: std::collections::HashMap<u64, CanvasUploads>,
    password_inputs: std::collections::HashMap<u64, String>,
    local_mkdir_input: String,
    remote_mkdir_input: String,
    rename_modal: Option<RenameItemModal>,
    #[cfg(not(target_os = "android"))]
    clip_seq: u64,
    #[cfg(not(target_os = "android"))]
    clipboard: Option<NativeClipboard>,
    show_about: bool,
    touch_two_finger: Option<(Pos2, f64, f32)>,
    #[cfg(target_os = "android")]
    android_kbd_open: bool,

    pub admin: AdminUiState,
}

struct FolderModal {
    rename_id: Option<i64>,
    parent_id: Option<i64>,
    name: String,
    error: Option<String>,
}

#[derive(Default)]
struct CanvasUploads {
    first_id: u64,
    targets: Vec<(std::path::PathBuf, String)>,
    finished_at: Option<f64>,
}

impl CanvasUploads {
    fn enqueue(&mut self, session: &mut ViewerSession, files: Vec<egui::DroppedFileHandle>) {
        if self.targets.is_empty() || self.finished_at.take().is_some() {
            self.targets.clear();
            self.first_id = session.file_transfer.queue.last().map_or(1, |entry| entry.id + 1);
        }
        let remote_dir = &session.file_transfer.remote_path;
        let use_directory = !remote_dir.is_empty() && session.file_transfer.remote_error.is_none();
        for file in files {
            let local = file.path().to_path_buf();
            let Some(name) = local.file_name().and_then(|name| name.to_str()) else { continue };
            if !deskbolt_proto::valid_drop_filename(name) {
                session.status_banner = "A dropped file has an unsupported name.".into();
                continue;
            }
            let remote = if use_directory {
                join_remote_path(remote_dir, name)
            } else {
                format!("{}{name}", deskbolt_proto::DESKTOP_UPLOAD_PREFIX)
            };
            if self.targets.contains(&(local.clone(), remote.clone())) {
                continue;
            }
            if session.cmd_tx.try_send(SessionCommand::QueueUpload {
                local_path: local.clone(),
                remote_path: remote.clone(),
            }).is_ok() {
                self.targets.push((local, remote));
            } else {
                session.status_banner = "The connection is too busy to queue every dropped file.".into();
            }
        }
    }

    fn paint(&mut self, ui: &egui::Ui, canvas: Rect, queue: &[TransferItem]) -> bool {
        if self.targets.is_empty() {
            return false;
        }
        let mut complete = 0;
        let mut failed = 0;
        let mut done = 0u64;
        let mut total = 0u64;
        for item in queue.iter().filter(|item| item.id >= self.first_id && item.direction == TransferDir::Upload) {
            if !self.targets.iter().any(|(local, remote)| local == &item.local_path && remote == &item.remote_path) {
                continue;
            }
            done = done.saturating_add(item.bytes_done);
            total = total.saturating_add(item.total_bytes);
            match item.status {
                TransferItemStatus::Verified => complete += 1,
                TransferItemStatus::Failed(_) | TransferItemStatus::Skipped => failed += 1,
                _ => {}
            }
        }
        let now = ui.input(|input| input.time);
        let count = self.targets.len();
        let text = if complete + failed >= count {
            let first = *self.finished_at.get_or_insert(now);
            let remaining = 4.0 - (now - first);
            if remaining <= 0.0 {
                return false;
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(remaining));
            if failed == 0 {
                format!("Uploaded {complete} of {count} files")
            } else {
                format!("Uploaded {complete} of {count} files. Open Files for details.")
            }
        } else {
            format!("Uploading files · {complete}/{count} · {} / {}", format_bytes(done), format_bytes(total))
        };
        if canvas.is_positive() {
            let painter = ui.painter_at(canvas);
            let label = painter.layout_no_wrap(text, egui::FontId::proportional(13.0), Color32::WHITE);
            let pos = Pos2::new(canvas.left() + 12.0, canvas.bottom() - label.size().y - 12.0);
            let bounds = Rect::from_min_size(pos, label.size()).expand(6.0);
            painter.rect_filled(bounds, 4.0, Color32::from_black_alpha(220));
            painter.galley(pos, label, Color32::WHITE);
        }
        true
    }
}

struct RenameItemModal {
    session_id: u64,
    is_remote: bool,
    old_path: String,
    new_name: String,
}

impl DeskBoltViewerApp {
    pub fn new(db: PhonebookDb, runtime: tokio::runtime::Handle) -> Self {
        Self::new_with_startup(db, runtime, None)
    }

    pub fn new_with_startup(
        db: PhonebookDb,
        runtime: tokio::runtime::Handle,
        startup: Option<StartupConnectArgs>,
    ) -> Self {
        let snippets = db.list_snippets().unwrap_or_default();
        let settings = db.load_settings();
        let mut admin = AdminUiState::new(&runtime, snippets);
        if startup.is_none() && settings.startup_view == StartupView::NetworkTools {
            admin.net_tools_open = true;
        }
        let mut app = Self {
            db,
            runtime,
            hook: KeyboardHookController::new(),
            next_session_id: 1,
            folders: Vec::new(),
            hosts: Vec::new(),
            selected_folder: None,
            selected_host_id: None,
            search_query: String::new(),
            focus_search: false,
            show_quick_connect: false,
            quick_host: String::new(),
            quick_port: "4899".into(),
            quick_mode: settings.default_mode,
            status_line: "Ready".into(),
            settings,
            theme_initialized: false,
            pending_startup: startup,
            props_modal: None,
            options_modal: None,
            rename_host_modal: None,
            oob_dialog: None,
            show_shortcuts_help: false,
            folder_modal: None,
            confirm_delete_host: None,
            sessions: Vec::new(),
            active_tab_id: None,
            session_scales: std::collections::HashMap::new(),
            textures: std::collections::HashMap::new(),
            canvas_uploads: std::collections::HashMap::new(),
            password_inputs: std::collections::HashMap::new(),
            local_mkdir_input: String::new(),
            remote_mkdir_input: String::new(),
            rename_modal: None,
            #[cfg(not(target_os = "android"))]
            clip_seq: 1,
            #[cfg(not(target_os = "android"))]
            clipboard: None,
            show_about: false,
            touch_two_finger: None,
            #[cfg(target_os = "android")]
            android_kbd_open: false,
            admin,
        };
        app.reload_phonebook();
        app
    }

    pub(crate) fn apply_theme(&self, ctx: &Context) {
        match self.settings.theme {
            ThemeChoice::Dark => ctx.set_visuals(egui::Visuals::dark()),
            ThemeChoice::Light => ctx.set_visuals(egui::Visuals::light()),
            ThemeChoice::System => {}
        }
    }

    fn selected_host(&self) -> Option<HostRow> {
        self.selected_host_id
            .and_then(|id| self.hosts.iter().find(|h| h.id == id).cloned())
    }

    pub(crate) fn reload_phonebook(&mut self) {
        self.folders = self.db.list_folders().unwrap_or_default();
        self.hosts = self
            .db
            .list_hosts(self.selected_folder, &self.search_query)
            .unwrap_or_default();
    }

    pub(crate) fn launch_connection(&mut self, ctx: &Context, mut target: ConnectTarget) {
        let id = self.next_session_id;
        self.next_session_id += 1;
        if let Some(hid) = target.host_id
            && let Some(h) = self.hosts.iter().find(|x| x.id == hid)
            && let Some(cq) = h.custom_quality
        {
            target.quality = cq;
        }

        let ctx_clone = ctx.clone();
        let repaint = Arc::new(move || ctx_clone.request_repaint());
        let host_id = target.host_id;
        let mut sess = ViewerSession::start(id, target, &self.runtime, repaint);
        sess.system_audio_controls.volume.store(self.settings.system_audio_volume, std::sync::atomic::Ordering::Relaxed);
        sess.system_audio_controls.muted.store(self.settings.system_audio_muted, std::sync::atomic::Ordering::Relaxed);
        if self.settings.system_audio_default && sess.mode != SessionMode::FileTransfer {
            sess.system_audio_on_connect = Some(self.settings.speaker_device.clone());
        }
        sess.voice_controls.noise_gate_milli.store(
            (self.settings.noise_gate * 1000.0).round() as u32,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.session_scales.insert(id, self.settings.scale_mode);
        self.sessions.push(sess);
        self.active_tab_id = Some(id);
        if let Some(hid) = host_id {
            let _ = self.db.touch_last_connected(hid);
            self.reload_phonebook();
        }
    }

    fn poll_all_sessions(&mut self, _ctx: &Context) {
        let mut pins_to_save = Vec::new();
        for s in &mut self.sessions {
            let (pin_opt, clip_opt) = s.poll_events();
            if s.typing_clipboard
                && (self.active_tab_id != Some(s.id) || !_ctx.input(|input| input.focused))
            {
                s.send_cmd(SessionCommand::StopTyping);
            }
            if let Some(pin_info) = pin_opt {
                pins_to_save.push(pin_info);
            }
            if let Some(message) = clip_opt
                && self.active_tab_id == Some(s.id)
            {
                let limit = self.settings.clipboard_max_bytes;
                if !message.within_limit(limit) {
                    s.status_banner = "Clipboard exceeds the viewer size limit.".into();
                    continue;
                }
                #[cfg(not(target_os = "android"))]
                {
                    if self.clipboard.is_none() {
                        self.clipboard = NativeClipboard::open();
                    }
                    s.status_banner = if self.clipboard.as_mut().is_some_and(|c| c.write(&message, limit)) {
                        "Clipboard updated.".into()
                    } else {
                        "Clipboard could not be updated.".into()
                    };
                }
                #[cfg(target_os = "android")]
                if let ClipboardSync::SetText { utf8_text, .. } = message {
                    _ctx.copy_text(utf8_text);
                }
            }
        }
        for (host_id_opt, fp) in pins_to_save {
            if let Some(hid) = host_id_opt {
                let _ = self.db.set_pinned_fingerprint(hid, Some(&fp));
                self.reload_phonebook();
            }
        }
    }
}

impl eframe::App for DeskBoltViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.theme_initialized {
            self.theme_initialized = true;
            self.apply_theme(&ctx);
        }
        if let Some(launch) = self.pending_startup.take() {
            match self.db.find_host_by_id_or_uuid(&launch.connect_id) {
                Ok(Some(mut host)) => {
                    if let Some(m) = launch.mode_override {
                        host.default_mode = m;
                    }
                    self.selected_host_id = Some(host.id);
                    self.launch_host_protocol(&ctx, &host);
                }
                Ok(None) => {
                    self.status_line = format!("Saved connection '{}' not found.", launch.connect_id);
                }
                Err(e) => {
                    self.status_line = format!("Startup lookup failed: {e}");
                }
            }
        }

        self.poll_all_sessions(&ctx);
        for session in &mut self.sessions {
            crate::monitor_workspace::show_windows(&ctx, session, self.settings.remote_cursor_sync);
        }
        self.poll_admin_background(&ctx);

        #[cfg(target_os = "android")]
        if let Some(s_idx) = self.sessions.iter().position(|s| s.file_transfer.open) {
            self.render_android_file_transfer(ui, s_idx);
            self.render_about_modal(&ctx);
            return;
        }

        #[cfg(target_os = "android")]
        if self.active_tab_id.is_some_and(|aid| {
            self.sessions
                .iter()
                .any(|s| s.id == aid && s.mode != SessionMode::FileTransfer && !s.headless_admin)
        }) {
            self.render_session_inner(ui);
            self.render_about_modal(&ctx);
            return;
        }

        // Address book hotkeys (only when no text input has focus)
        if !ctx.egui_wants_keyboard_input() {
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(Key::F)) {
                self.focus_search = true;
            }
            if ui.input(|i| i.key_pressed(Key::F2))
                && let Some(h) = self.selected_host()
            {
                self.rename_host_modal = Some((h.id, h.name));
            }
            if ui.input(|i| i.key_pressed(Key::Delete))
                && let Some(h) = self.selected_host()
            {
                self.confirm_delete_host = Some(h);
            }
            if ui.input(|i| i.modifiers.alt && i.key_pressed(Key::Enter))
                && let Some(h) = self.selected_host()
            {
                self.props_modal = Some(HostPropertiesModal::from_host(self.selected_folder, Some(&h)));
            }
        }

        // 1. Classic Top Menu Bar
        egui::Panel::top("classic_menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Connection", |ui| {
                    let sel = self.selected_host();
                    if ui.add_enabled(sel.is_some(), egui::Button::new("Connect")).clicked()
                        && let Some(h) = sel.as_ref()
                    {
                        self.launch_host_protocol(&ctx, h);
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.active_tab_id.is_some(), egui::Button::new("Disconnect Active"))
                        .clicked()
                    {
                        if let Some(aid) = self.active_tab_id {
                            if let Some(s) = self.sessions.iter().find(|s| s.id == aid) {
                                s.send_cmd(SessionCommand::Disconnect);
                            }
                            self.sessions.retain(|s| s.id != aid);
                            self.active_tab_id = None;
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("New Host...").clicked() {
                        self.props_modal = Some(HostPropertiesModal::from_host(self.selected_folder, None));
                        ui.close();
                    }
                    if ui.button("New Folder...").clicked() {
                        self.folder_modal = Some(FolderModal {
                            rename_id: None,
                            parent_id: self.selected_folder,
                            name: String::new(),
                            error: None,
                        });
                        ui.close();
                    }
                    if ui.add_enabled(sel.is_some(), egui::Button::new("Rename (F2)")).clicked()
                        && let Some(h) = sel.as_ref()
                    {
                        self.rename_host_modal = Some((h.id, h.name.clone()));
                        ui.close();
                    }
                    if ui.add_enabled(sel.is_some(), egui::Button::new("Delete (Del)")).clicked()
                        && let Some(h) = sel.clone()
                    {
                        self.confirm_delete_host = Some(h);
                        ui.close();
                    }
                    if let Some(h) = sel.as_ref() {
                        ui.menu_button("Create Desktop Shortcut", |ui| {
                            for m in [
                                SessionMode::FullControl,
                                SessionMode::ViewOnly,
                                SessionMode::FileTransfer,
                            ] {
                                if ui.button(mode_label(m)).clicked() {
                                    match create_desktop_shortcut(h, m) {
                                        Ok(p) => self.status_line = format!("Created shortcut {}", p.display()),
                                        Err(e) => self.status_line = format!("Shortcut failed: {e}"),
                                    }
                                    ui.close();
                                }
                            }
                        });
                    }
                    if ui.add_enabled(sel.is_some(), egui::Button::new("Properties (Alt+Enter)")).clicked()
                        && let Some(h) = sel.as_ref()
                    {
                        self.props_modal = Some(HostPropertiesModal::from_host(self.selected_folder, Some(h)));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("Mode", |ui| {
                    let sel = self.selected_host();
                    let has_sel = sel.is_some();
                    for (label, action) in [
                        ("Full Control", "full"),
                        ("View Only", "view"),
                        ("Terminal", "terminal"),
                        ("File Transfer", "files"),
                        ("Shutdown / Power...", "power"),
                        ("Text Chat", "chat"),
                        ("Voice Chat", "voice"),
                        ("Send Message...", "popup"),
                    ] {
                        if ui.add_enabled(has_sel, egui::Button::new(label)).clicked()
                            && let Some(h) = sel.as_ref()
                        {
                            self.launch_mode_action(&ctx, h, action);
                            ui.close();
                        }
                    }
                });

                ui.menu_button("View", |ui| {
                    let mut changed = false;
                    if ui.checkbox(&mut self.settings.show_folder_tree, "Folder Tree").changed() {
                        changed = true;
                    }
                    if ui.checkbox(&mut self.settings.show_toolbar, "Toolbar").changed() {
                        changed = true;
                    }
                    if ui.checkbox(&mut self.settings.show_status_bar, "Status Bar").changed() {
                        changed = true;
                    }
                    ui.separator();
                    for lay in ViewLayout::ALL {
                        if ui.selectable_value(&mut self.settings.view_layout, lay, lay.label()).clicked() {
                            changed = true;
                        }
                    }
                    if changed {
                        let _ = self.db.save_settings(&self.settings);
                    }
                });

                ui.menu_button("Tools", |ui| {
                    if ui.button("Subnet Scanner").clicked() {
                        self.admin.net_tools_open = true;
                        self.admin.net_tab = NetToolTab::Scanner;
                        ui.close();
                    }
                    if ui.button("Wake-on-LAN").clicked() {
                        self.admin.net_tools_open = true;
                        self.admin.net_tab = NetToolTab::Wol;
                        ui.close();
                    }
                    if ui.button("Serial Console").clicked() {
                        self.open_local_serial_tab("Console", "", 115200);
                        ui.close();
                    }
                    if ui.button("Out-of-Band Control...").clicked() {
                        let sel = self.selected_host();
                        self.oob_dialog = Some(OobDialogState {
                            host_id: sel.as_ref().map(|h| h.id),
                            label: sel.as_ref().map(|h| h.name.clone()).unwrap_or_else(|| "Manual Target".into()),
                            host: sel.as_ref().and_then(|h| h.oob_host.clone()).or_else(|| sel.as_ref().map(|h| h.host.clone())).unwrap_or_default(),
                            port: sel.as_ref().map(|h| h.oob_port.to_string()).unwrap_or_else(|| "16992".into()),
                            use_tls: sel.as_ref().map(|h| h.oob_tls > 0).unwrap_or(true),
                            tls_port: sel.as_ref().map(|h| if h.oob_tls > 0 { h.oob_tls } else { 16993 }.to_string()).unwrap_or_else(|| "16993".into()),
                            username: sel.as_ref().and_then(|h| h.oob_username.clone()).unwrap_or_default(),
                            password: sel.as_ref().and_then(|h| h.oob_password.clone()).unwrap_or_default(),
                        });
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Options...").clicked() {
                        self.options_modal = Some(OptionsModalState::open_from(&self.settings));
                        ui.close();
                    }
                });

                ui.menu_button("Help", |ui| {
                    if ui.button("Keyboard Shortcuts").clicked() {
                        self.show_shortcuts_help = true;
                        ui.close();
                    }
                    if ui.button("About DeskBolt").clicked() {
                        self.show_about = true;
                        ui.close();
                    }
                });
            });
        });

        // 2. Classic Icon & Search Toolbar
        if self.settings.show_toolbar {
            egui::Panel::top("address_book_toolbar").show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.selectable_label(self.show_quick_connect, "⚡ Quick Connect").clicked() {
                        self.show_quick_connect = !self.show_quick_connect;
                    }
                    if ui.button("+ Add").clicked() {
                        self.props_modal = Some(HostPropertiesModal::from_host(self.selected_folder, None));
                    }
                    let sel = self.selected_host();
                    if ui.add_enabled(sel.is_some(), egui::Button::new("Delete")).clicked()
                        && let Some(h) = sel.clone()
                    {
                        self.confirm_delete_host = Some(h);
                    }
                    if ui.add_enabled(sel.is_some(), egui::Button::new("Properties")).clicked()
                        && let Some(h) = sel.as_ref()
                    {
                        self.props_modal = Some(HostPropertiesModal::from_host(self.selected_folder, Some(h)));
                    }
                    ui.separator();
                    ui.add_enabled_ui(sel.is_some(), |ui| {
                        for (lbl, act) in [
                            ("Full", "full"),
                            ("View", "view"),
                            ("Term", "terminal"),
                            ("Files", "files"),
                            ("Chat", "chat"),
                            ("Voice", "voice"),
                            ("Msg", "popup"),
                            ("Power", "power"),
                        ] {
                            if ui.small_button(lbl).clicked()
                                && let Some(h) = sel.as_ref()
                            {
                                self.launch_mode_action(&ctx, h, act);
                            }
                        }
                    });
                    ui.separator();
                    for (lay, lbl) in [
                        (ViewLayout::LargeIcons, "Icons"),
                        (ViewLayout::List, "List"),
                        (ViewLayout::Details, "Details"),
                    ] {
                        if ui.selectable_label(self.settings.view_layout == lay, lbl).clicked() {
                            self.settings.view_layout = lay;
                            let _ = self.db.save_settings(&self.settings);
                        }
                    }
                    ui.separator();
                    if ui.selectable_label(self.admin.net_tools_open, "Scanner").clicked() {
                        self.admin.net_tools_open = !self.admin.net_tools_open;
                    }
                    ui.checkbox(&mut self.admin.ping_enabled, "Live Ping");
                    ui.separator();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .hint_text("Search name or IP...")
                            .desired_width(150.0),
                    );
                    if self.focus_search {
                        resp.request_focus();
                        self.focus_search = false;
                    }
                    if resp.changed() {
                        self.reload_phonebook();
                    }
                    if !self.search_query.is_empty() && ui.small_button("x").clicked() {
                        self.search_query.clear();
                        self.reload_phonebook();
                    }
                });

                if self.show_quick_connect {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Quick Connect:");
                        let host_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.quick_host)
                                .hint_text("Host or IP")
                                .desired_width(180.0),
                        );
                        ui.label("Port:");
                        ui.add(egui::TextEdit::singleline(&mut self.quick_port).desired_width(56.0));
                        egui::ComboBox::from_id_salt("quick_mode")
                            .selected_text(mode_label(self.quick_mode))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.quick_mode, SessionMode::FullControl, "Full Control");
                                ui.selectable_value(&mut self.quick_mode, SessionMode::ViewOnly, "View Only");
                                ui.selectable_value(&mut self.quick_mode, SessionMode::FileTransfer, "File Transfer");
                            });
                        let enter_connect = host_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (ui.button("Connect").clicked() || enter_connect) && !self.quick_host.trim().is_empty() {
                            let port = self.quick_port.trim().parse::<u16>().unwrap_or(4899).max(1);
                            let h = self.quick_host.trim().to_string();
                            let pin = self
                                .db
                                .find_pinned_by_endpoint(&h, port)
                                .ok()
                                .flatten()
                                .and_then(|(id, p)| p.map(|fp| (Some(id), fp)));
                            let (hid, pfp) = match pin {
                                Some((id, fp)) => (id, Some(fp)),
                                None => (None, None),
                            };
                            self.launch_connection(
                                &ctx,
                                ConnectTarget {
                                    host_id: hid,
                                    title: h.clone(),
                                    host: h,
                                    port,
                                    mode: self.quick_mode,
                                    quality: self.settings.default_quality,
                                    pinned_fingerprint: pfp,
                                },
                            );
                        }
                    });
                }
                ui.add_space(2.0);
            });
        }

        // 3. Bottom Status Strip
        if self.settings.show_status_bar {
            egui::Panel::bottom("address_book_status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&self.status_line);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(format!(
                            "{} saved hosts  |  {} active sessions",
                            self.hosts.len(),
                            self.sessions.len()
                        ));
                    });
                });
            });
        }

        // 4. Left Folder Sidebar
        if self.settings.show_folder_tree {
            egui::Panel::left("folders_sidebar")
                .default_size(205.0)
                .min_size(145.0)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.heading("Folders");
                    ui.separator();
                    if ui
                        .selectable_label(self.selected_folder.is_none(), "All Connections")
                        .clicked()
                    {
                        self.selected_folder = None;
                        self.reload_phonebook();
                    }

                    ScrollArea::vertical().show(ui, |ui| {
                        self.render_folder_tree(ui, None, 0);
                    });
                });
        }

        // 5. Main Host Area (Large Icons / List / Details)
        egui::CentralPanel::default().show(ui, |ui| {
            let hosts = self.hosts.clone();
            if hosts.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No saved connections in this folder. Click '+ Add' or 'Quick Connect'.");
                });
                return;
            }

            ScrollArea::vertical().show(ui, |ui| match self.settings.view_layout {
                ViewLayout::LargeIcons => {
                    ui.horizontal_wrapped(|ui| {
                        for host in hosts {
                            let is_sel = self.selected_host_id == Some(host.id);
                            let ping = self.format_ping_badge(host.id);
                            let txt = format!(
                                "{}\n{}\n{}:{}\n{}",
                                host.name,
                                host.protocol_type.label(),
                                host.host,
                                host.port,
                                ping.0
                            );
                            let resp = ui.add_sized(
                                [158.0, 92.0],
                                egui::Button::selectable(is_sel, txt),
                            );
                            if resp.clicked() {
                                self.selected_host_id = Some(host.id);
                            }
                            if resp.double_clicked() {
                                self.selected_host_id = Some(host.id);
                                self.launch_host_protocol(&ctx, &host);
                            }
                            resp.context_menu(|ui| self.render_host_context_menu(ui, &ctx, &host));
                        }
                    });
                }
                ViewLayout::List => {
                    for host in hosts {
                        let is_sel = self.selected_host_id == Some(host.id);
                        let ping = self.format_ping_badge(host.id);
                        let resp = ui.selectable_label(
                            is_sel,
                            format!(
                                "{}  [{}]  {}:{}  ({})",
                                host.name,
                                host.protocol_type.label(),
                                host.host,
                                host.port,
                                ping.0
                            ),
                        );
                        if resp.clicked() {
                            self.selected_host_id = Some(host.id);
                        }
                        if resp.double_clicked() {
                            self.selected_host_id = Some(host.id);
                            self.launch_host_protocol(&ctx, &host);
                        }
                        resp.context_menu(|ui| self.render_host_context_menu(ui, &ctx, &host));
                    }
                }
                ViewLayout::Details => {
                    egui::Grid::new("hosts_grid")
                        .num_columns(7)
                        .striped(true)
                        .spacing([16.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("Name");
                            ui.strong("Address");
                            ui.strong("Protocol");
                            ui.strong("Ping");
                            ui.strong("Default Mode");
                            ui.strong("Security Key");
                            ui.strong("Actions");
                            ui.end_row();

                            for host in hosts {
                                let is_sel = self.selected_host_id == Some(host.id);
                                let row_resp = ui.selectable_label(is_sel, &host.name);
                                if row_resp.clicked() {
                                    self.selected_host_id = Some(host.id);
                                }
                                if row_resp.double_clicked() {
                                    self.selected_host_id = Some(host.id);
                                    self.launch_host_protocol(&ctx, &host);
                                }
                                row_resp.context_menu(|ui| self.render_host_context_menu(ui, &ctx, &host));

                                ui.label(format!("{}:{}", host.host, host.port));
                                ui.label(host.protocol_type.label());
                                let (p_txt, p_col) = self.format_ping_badge(host.id);
                                ui.colored_label(p_col, p_txt);
                                ui.label(mode_label(host.default_mode));
                                ui.label(if host.pinned_fingerprint.is_some() {
                                    "Verified"
                                } else {
                                    "Not Verified"
                                });
                                ui.horizontal(|ui| {
                                    if ui.small_button("Connect").clicked() {
                                        self.selected_host_id = Some(host.id);
                                        self.launch_host_protocol(&ctx, &host);
                                    }
                                    if host.protocol_type != HostProtocol::Serial && ui.small_button("Files").clicked() {
                                        self.selected_host_id = Some(host.id);
                                        if matches!(host.protocol_type, HostProtocol::Ssh | HostProtocol::Sftp) {
                                            self.open_sftp_for_host(&host);
                                        } else {
                                            self.launch_connection(
                                                &ctx,
                                                ConnectTarget {
                                                    host_id: Some(host.id),
                                                    title: host.name.clone(),
                                                    host: host.host.clone(),
                                                    port: host.port,
                                                    mode: SessionMode::FileTransfer,
                                                    quality: host.quality_mode,
                                                    pinned_fingerprint: host.pinned_fingerprint.clone(),
                                                },
                                            );
                                        }
                                    }
                                });
                                ui.end_row();
                            }
                        });
                }
            });
        });

        self.render_address_book_modals(&ctx);
        self.render_phase8_modals(&ctx);
        self.render_about_modal(&ctx);
        self.render_session_viewport(&ctx);
        self.render_file_transfer_viewports(&ctx);
        self.render_admin_windows(&ctx);
    }
}

impl DeskBoltViewerApp {
    fn format_ping_badge(&self, host_id: i64) -> (String, Color32) {
        match self.admin.latencies.get(&host_id) {
            Some(st) if st.online => {
                let ms = st.last_rtt_ms.unwrap_or(0.0);
                let tag = if st.used_tcp_fallback { "TCP" } else { "ICMP" };
                (format!("{ms:.0} ms ({tag})"), Color32::from_rgb(46, 175, 92))
            }
            Some(st) if st.sent > 0 => ("Offline".into(), Color32::from_rgb(215, 75, 75)),
            _ => ("-".into(), Color32::GRAY),
        }
    }

    fn launch_mode_action(&mut self, ctx: &Context, host: &HostRow, action: &str) {
        match action {
            "full" => self.launch_connection(
                ctx,
                ConnectTarget {
                    host_id: Some(host.id),
                    title: host.name.clone(),
                    host: host.host.clone(),
                    port: host.port,
                    mode: SessionMode::FullControl,
                    quality: host.quality_mode,
                    pinned_fingerprint: host.pinned_fingerprint.clone(),
                },
            ),
            "view" => self.launch_connection(
                ctx,
                ConnectTarget {
                    host_id: Some(host.id),
                    title: host.name.clone(),
                    host: host.host.clone(),
                    port: host.port,
                    mode: SessionMode::ViewOnly,
                    quality: host.quality_mode,
                    pinned_fingerprint: host.pinned_fingerprint.clone(),
                },
            ),
            "files" => {
                if matches!(host.protocol_type, HostProtocol::Ssh | HostProtocol::Sftp) {
                    self.open_sftp_for_host(host);
                } else {
                    self.launch_connection(
                        ctx,
                        ConnectTarget {
                            host_id: Some(host.id),
                            title: host.name.clone(),
                            host: host.host.clone(),
                            port: host.port,
                            mode: SessionMode::FileTransfer,
                            quality: host.quality_mode,
                            pinned_fingerprint: host.pinned_fingerprint.clone(),
                        },
                    );
                }
            }
            "terminal" => {
                if host.protocol_type == HostProtocol::Ssh {
                    self.open_ssh_terminal_for_host(host);
                } else if host.protocol_type == HostProtocol::Serial {
                    self.open_local_serial_tab(&host.name, &host.host, host.port as u32);
                } else {
                    self.open_headless_deskbolt_tool(ctx, host, "terminal");
                }
            }
            other => self.open_headless_deskbolt_tool(ctx, host, other),
        }
    }

    fn render_host_context_menu(&mut self, ui: &mut egui::Ui, ctx: &Context, host: &HostRow) {
        if host.protocol_type == HostProtocol::DeskBolt {
            for (lbl, act) in [
                ("Full Control", "full"),
                ("View Only", "view"),
                ("Terminal", "terminal"),
                ("File Transfer", "files"),
                ("Shutdown / Power...", "power"),
                ("Text Chat", "chat"),
                ("Voice Chat", "voice"),
                ("Send Message...", "popup"),
                ("Port Tunnels", "tunnels"),
                ("System & Services", "sys"),
            ] {
                if ui.button(lbl).clicked() {
                    self.launch_mode_action(ctx, host, act);
                    ui.close();
                }
            }
            ui.separator();
        }
        if ui.button("Open SSH Terminal").clicked() {
            self.open_ssh_terminal_for_host(host);
            ui.close();
        }
        if ui.button("Open SFTP Browser").clicked() {
            self.open_sftp_for_host(host);
            ui.close();
        }
        if let Some(mac) = host.mac_address.clone().filter(|m| !m.trim().is_empty())
            && ui.button("Wake-on-LAN").clicked()
        {
            match send_wol_packet(
                &mac,
                &self.admin.wol_broadcast,
                self.admin.wol_port.trim().parse::<u16>().unwrap_or(9),
            ) {
                Ok(()) => self.status_line = format!("Wake-on-LAN packet sent to {mac}"),
                Err(e) => self.status_line = format!("Wake-on-LAN failed: {e}"),
            }
            ui.close();
        }
        ui.menu_button("Out-of-Band Hardware Control", |ui| {
            for act in [
                OobAction::ColdRestart,
                OobAction::TurnOn,
                OobAction::ColdTurnOff,
                OobAction::BiosRemoteControl,
                OobAction::StartupRemoteControl,
                OobAction::NetworkBoot,
            ] {
                if ui.button(act.label()).clicked() {
                    self.trigger_oob_action(ctx, host, act);
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("Rename (F2)").clicked() {
            self.rename_host_modal = Some((host.id, host.name.clone()));
            ui.close();
        }
        ui.menu_button("Create Shortcut on Desktop", |ui| {
            for m in [
                SessionMode::FullControl,
                SessionMode::ViewOnly,
                SessionMode::FileTransfer,
            ] {
                if ui.button(mode_label(m)).clicked() {
                    match create_desktop_shortcut(host, m) {
                        Ok(p) => self.status_line = format!("Created shortcut {}", p.display()),
                        Err(e) => self.status_line = format!("Shortcut failed: {e}"),
                    }
                    ui.close();
                }
            }
        });
        if ui.button("Properties (Alt+Enter)").clicked() {
            self.props_modal = Some(HostPropertiesModal::from_host(self.selected_folder, Some(host)));
            ui.close();
        }
        if host.pinned_fingerprint.is_some() && ui.button("Clear Trusted Key").clicked() {
            let _ = self.db.set_pinned_fingerprint(host.id, None);
            self.reload_phonebook();
            ui.close();
        }
        if ui.button("Delete (Del)").clicked() {
            self.confirm_delete_host = Some(host.clone());
            ui.close();
        }
    }

    fn render_folder_tree(&mut self, ui: &mut egui::Ui, parent_id: Option<i64>, depth: usize) {
        let children: Vec<FolderRow> = self
            .folders
            .iter()
            .filter(|f| f.parent_id == parent_id)
            .cloned()
            .collect();
        for folder in children {
            ui.horizontal(|ui| {
                ui.add_space((depth as f32) * 14.0);
                let selected = self.selected_folder == Some(folder.id);
                let resp = ui.selectable_label(selected, &folder.name);
                if resp.clicked() {
                    self.selected_folder = Some(folder.id);
                    self.reload_phonebook();
                }
                resp.context_menu(|ui| {
                    if ui.button("Broadcast Command...").clicked() {
                        self.open_broadcast_for_folder(folder.id, &folder.name);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("New Subfolder").clicked() {
                        self.folder_modal = Some(FolderModal {
                            rename_id: None,
                            parent_id: Some(folder.id),
                            name: String::new(),
                            error: None,
                        });
                        ui.close();
                    }
                    if ui.button("Rename Folder").clicked() {
                        self.folder_modal = Some(FolderModal {
                            rename_id: Some(folder.id),
                            parent_id: folder.parent_id,
                            name: folder.name.clone(),
                            error: None,
                        });
                        ui.close();
                    }
                    if ui.button("Delete Folder").clicked() {
                        let _ = self.db.delete_folder(folder.id);
                        if self.selected_folder == Some(folder.id) {
                            self.selected_folder = None;
                        }
                        self.reload_phonebook();
                        ui.close();
                    }
                });
            });
            self.render_folder_tree(ui, Some(folder.id), depth + 1);
        }
    }

    fn render_address_book_modals(&mut self, ctx: &Context) {
        if let Some(mut modal) = self.folder_modal.take() {
            let mut keep_open = true;
            Window::new(if modal.rename_id.is_some() {
                "Rename Folder"
            } else {
                "New Folder"
            })
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Folder Name:");
                    ui.text_edit_singleline(&mut modal.name);
                });
                if let Some(err) = &modal.error {
                    ui.colored_label(Color32::RED, err);
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let res = if let Some(id) = modal.rename_id {
                            self.db.rename_folder(id, &modal.name)
                        } else {
                            self.db.create_folder(modal.parent_id, &modal.name).map(|_| ())
                        };
                        match res {
                            Ok(()) => {
                                keep_open = false;
                                self.reload_phonebook();
                            }
                            Err(e) => modal.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }
                });
            });
            if keep_open {
                self.folder_modal = Some(modal);
            }
        }

        if let Some(host) = self.confirm_delete_host.take() {
            let mut keep_open = true;
            Window::new("Delete Connection")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!("Remove '{}' ({}:{}) from address book?", host.name, host.host, host.port));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            let _ = self.db.delete_host(host.id);
                            keep_open = false;
                            self.reload_phonebook();
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
            if keep_open {
                self.confirm_delete_host = Some(host);
            }
        }
    }

    fn render_about_modal(&mut self, ctx: &Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        Window::new("About DeskBolt")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.heading("DeskBolt Viewer");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.separator();
                ui.label("Self-Hosted Cross-Platform Remote Desktop");
                ui.label("License: Mozilla Public License 2.0 (MPL-2.0)");
                ui.label("Publisher: StackSync Solutions");
                ui.add_space(6.0);
                if ui.button("Close").clicked() {
                    self.show_about = false;
                }
            });
        if !open {
            self.show_about = false;
        }
    }

    fn render_session_viewport(&mut self, ctx: &Context) {
        let has_desktop_tab = self
            .sessions
            .iter()
            .any(|s| s.mode != SessionMode::FileTransfer && !s.headless_admin);
        if !has_desktop_tab {
            self.hook.update_state(false, None);
            return;
        }

        #[cfg(not(target_os = "android"))]
        {
            let vp_id = ViewportId::from_hash_of("deskbolt_sessions_viewport");
            let builder = ViewportBuilder::default()
                .with_title("DeskBolt - Remote Sessions")
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([640.0, 480.0]);

            ctx.show_viewport_immediate(vp_id, builder, |ui, _class| {
                self.render_session_inner(ui);
            });
        }
        #[cfg(target_os = "android")]
        let _ = ctx;
    }

    fn render_session_inner(&mut self, ui: &mut egui::Ui) {
            let close_req = ui.input(|i| i.viewport().close_requested());
            let mut closed_ids = Vec::new();

            if close_req {
                for s in &self.sessions {
                    if s.mode != SessionMode::FileTransfer && !s.headless_admin {
                        s.send_cmd(SessionCommand::Disconnect);
                        closed_ids.push(s.id);
                    }
                }
            }

            // Tab Strip
            egui::Panel::top("session_tabs_bar").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    #[cfg(target_os = "android")]
                    if ui.button("< Connections").clicked() {
                        self.active_tab_id = None;
                    }
                    let tabs: Vec<(u64, String, SessionMode, bool)> = self
                        .sessions
                        .iter()
                        .filter(|s| s.mode != SessionMode::FileTransfer && !s.headless_admin)
                        .map(|s| {
                            (
                                s.id,
                                s.title.clone(),
                                s.mode,
                                matches!(s.phase, ConnectionPhase::Connected),
                            )
                        })
                        .collect();

                    for (id, title, mode, connected) in tabs {
                        let active = self.active_tab_id == Some(id);
                        let label = format!(
                            "{} {} ({})",
                            if connected { "●" } else { "○" },
                            title,
                            mode_label(mode)
                        );
                        if ui.selectable_label(active, label).clicked() {
                            self.active_tab_id = Some(id);
                        }
                        if ui.small_button("x").clicked() {
                            if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
                                s.send_cmd(SessionCommand::Disconnect);
                            }
                            closed_ids.push(id);
                        }
                    }
                });
            });

            if !closed_ids.is_empty() {
                self.sessions.retain(|s| !closed_ids.contains(&s.id));
                for id in &closed_ids {
                    self.textures.remove(id);
                    self.canvas_uploads.remove(id);
                    self.password_inputs.remove(id);
                }
                if self.active_tab_id.is_some_and(|a| closed_ids.contains(&a)) {
                    self.active_tab_id = self
                        .sessions
                        .iter()
                        .find(|s| s.mode != SessionMode::FileTransfer && !s.headless_admin)
                        .map(|s| s.id);
                }
            }

            let Some(active_id) = self.active_tab_id else {
                self.hook.update_state(false, None);
                return;
            };
            let Some(idx) = self.sessions.iter().position(|s| s.id == active_id) else {
                self.hook.update_state(false, None);
                return;
            };

            // Compact Session Toolbar
            let mut hook_allowed = false;
            let mut hook_tx = None;

            egui::Panel::top("session_action_toolbar").show(ui, |ui| {
                let s = &mut self.sessions[idx];
                ui.horizontal_wrapped(|ui| {
                    let is_full = s.mode == SessionMode::FullControl;
                    if ui.selectable_label(is_full, "Full Control").clicked() {
                        s.mode = SessionMode::FullControl;
                        s.send_cmd(SessionCommand::Control(ControlMsg::SessionModeSwitch {
                            mode: SessionMode::FullControl,
                        }));
                    }
                    if ui.selectable_label(!is_full, "View Only").clicked() {
                        s.mode = SessionMode::ViewOnly;
                        s.capture_system_keys = false;
                        s.send_cmd(SessionCommand::Control(ControlMsg::SessionModeSwitch {
                            mode: SessionMode::ViewOnly,
                        }));
                    }
                    if ui.button("File Transfer").clicked() {
                        s.file_transfer.open = true;
                        s.send_cmd(SessionCommand::File(FileTransferMsg::ListRootsReq));
                    }
                    if ui.button("Terminal").clicked() {
                        s.terminal_open = true;
                        if !s.terminal_active {
                            s.send_cmd(SessionCommand::Terminal(TerminalMsg::OpenPty {
                                cols: 100,
                                rows: 30,
                                shell_hint: None,
                            }));
                        }
                    }
                    if ui.button("Tunnels").clicked() {
                        s.tunnels_open = true;
                    }
                    if ui.button("System").clicked() {
                        s.sys_open = true;
                        s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::SystemSnapshotReq));
                    }
                    let chat_lbl = if s.chat_unread > 0 {
                        format!("Chat ({})", s.chat_unread)
                    } else {
                        "Chat".into()
                    };
                    if ui.button(chat_lbl).clicked() {
                        s.chat_open = !s.chat_open;
                        if s.chat_open {
                            s.chat_unread = 0;
                        }
                    }
                    if ui.button(if s.voice_active { "Voice (Active)" } else { "Voice" }).clicked() {
                        s.voice_open = !s.voice_open;
                    }
                    ui.add_enabled_ui(!s.system_audio_pending && matches!(s.phase, ConnectionPhase::Connected), |ui| {
                        use std::sync::atomic::Ordering::Relaxed;

                        if ui.selectable_label(s.system_audio_active, "System Audio").clicked() {
                            s.request_system_audio(self.settings.speaker_device.clone());
                        }
                        if s.system_audio_active {
                            let mut volume = s.system_audio_controls.volume.load(Relaxed);
                            let mut muted = s.system_audio_controls.muted.load(Relaxed);
                            if ui.add(egui::Slider::new(&mut volume, 0..=100).suffix("%")).on_hover_text("System audio volume").changed() {
                                s.system_audio_controls.volume.store(volume, Relaxed);
                            }
                            if ui.checkbox(&mut muted, "Mute").on_hover_text("Mute system audio").changed() {
                                s.system_audio_controls.muted.store(muted, Relaxed);
                            }
                        }
                    });
                    if ui.selectable_label(s.quality_hud, "Quality HUD").clicked() {
                        s.quality_hud = !s.quality_hud;
                    }
                    if ui.button("Message").clicked() {
                        s.popup_open = true;
                    }
                    if ui.button("Power").clicked() {
                        s.power_open = true;
                    }
                    ui.separator();
                    ui.add_enabled_ui(is_full, |ui| {
                        if ui.selectable_label(s.privacy_curtain, "Blank Remote Screen").clicked() {
                            s.send_cmd(SessionCommand::Control(ControlMsg::SpecialAction(
                                SpecialAction::PrivacyCurtain { enabled: !s.privacy_curtain },
                            )));
                        }
                        if ui.selectable_label(s.local_input_lock, "Lock Remote Keyboard & Mouse").clicked() {
                            s.send_cmd(SessionCommand::Control(ControlMsg::SpecialAction(
                                SpecialAction::LockLocalInput { enabled: !s.local_input_lock },
                            )));
                        }
                        if ui.button("Send Ctrl+Alt+Del").clicked() {
                            s.send_cmd(SessionCommand::Control(ControlMsg::SpecialAction(
                                SpecialAction::SendCtrlAltDel,
                            )));
                        }
                    });
                    if !s.monitors.is_empty() {
                        let cur_label = s
                            .monitors
                            .iter()
                            .find(|m| m.id == s.active_monitor_id)
                            .map(|m| format!("Display {} ({}x{})", u16::from(m.id) + 1, m.width, m.height))
                            .unwrap_or_else(|| format!("Display {}", u16::from(s.active_monitor_id) + 1));
                        let cur_label = if s.workspace.stitched { "All Screens".into() } else { cur_label };
                        egui::ComboBox::from_id_salt("monitor_select")
                            .selected_text(cur_label)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(s.workspace.stitched, "All Screens").clicked() {
                                    s.workspace.stitched = true;
                                }
                                for m in &s.monitors {
                                    let text = format!("Display {} ({}x{})", u16::from(m.id) + 1, m.width, m.height);
                                    if ui.selectable_label(!s.workspace.stitched && s.active_monitor_id == m.id, text).clicked() {
                                        s.workspace.stitched = false;
                                        s.active_monitor_id = m.id;
                                        s.send_cmd(SessionCommand::Control(ControlMsg::MonitorSelect { monitor_id: m.id }));
                                    }
                                }
                            });
                        #[cfg(not(target_os = "android"))]
                        if ui.selectable_label(s.workspace.has_windows(), "Pop Out Displays").clicked() {
                            if s.workspace.has_windows() {
                                s.send_cmd(SessionCommand::Input(InputMsg::ReleaseAllKeys));
                            }
                            s.workspace.toggle_windows(&s.monitors, &s.cmd_tx);
                        }
                        s.workspace.synchronize(&s.monitors, &s.cmd_tx);
                        if s.workspace.stitched {
                            s.window_match.disable(&s.cmd_tx);
                        }
                        if ui.add_enabled(is_full && !s.workspace.stitched, egui::Checkbox::new(
                            &mut s.window_match.enabled, "Match Viewer Window Size",
                        )).changed() && !s.window_match.enabled {
                            s.window_match.disable(&s.cmd_tx);
                        }
                    }
                    if s.android_controls && is_full {
                        use deskbolt_proto::DeviceAction;
                        ui.menu_button("Device", |ui| {
                            for (label, action) in [
                                ("Home", DeviceAction::Home),
                                ("Back", DeviceAction::Back),
                                ("Recent Apps", DeviceAction::Recents),
                                ("Notifications", DeviceAction::Notifications),
                                ("Quick Settings", DeviceAction::QuickSettings),
                                ("Lock Screen", DeviceAction::LockScreen),
                            ] {
                                if ui.button(label).clicked() {
                                    s.send_cmd(SessionCommand::Input(InputMsg::DeviceAction(action)));
                                    ui.close();
                                }
                            }
                        });
                    }
                    ui.menu_button("Clipboard", |ui| {
                        ui.checkbox(&mut s.auto_clipboard, "Auto-sync clipboard");
                        #[cfg(not(target_os = "android"))]
                        if is_full && ui.button("Send Local Clipboard").clicked() {
                            if self.clipboard.is_none() {
                                self.clipboard = NativeClipboard::open();
                            }
                            let limit = self.settings.clipboard_max_bytes;
                            if let Some(data) = self.clipboard.as_mut().and_then(|c| c.read(limit)) {
                                self.clip_seq += 1;
                                s.send_cmd(SessionCommand::Clipboard(ClipboardSync::set(self.clip_seq, data)));
                            } else {
                                s.status_banner = "Clipboard is empty, unavailable, or exceeds the size limit.".into();
                            }
                            ui.close();
                        }
                    });
                    #[cfg(not(target_os = "android"))]
                    if is_full && matches!(s.phase, ConnectionPhase::Connected)
                        && ui.button(if s.typing_clipboard { "Stop Typing" } else { "Type Clipboard" }).clicked()
                    {
                        toggle_clipboard_typing(s, &mut self.clipboard, &self.settings);
                    }
                    ui.separator();
                    let cur_scale = self
                        .session_scales
                        .get(&s.id)
                        .copied()
                        .unwrap_or(self.settings.scale_mode);
                    egui::ComboBox::from_id_salt("scale_mode_select")
                        .selected_text(cur_scale.label())
                        .show_ui(ui, |ui| {
                            for sm in ScaleMode::ALL {
                                if ui.selectable_label(cur_scale == sm, sm.label()).clicked() {
                                    self.session_scales.insert(s.id, sm);
                                    s.fit_scale = sm == ScaleMode::Fit;
                                }
                            }
                        });
                    let prev_q = s.quality;
                    egui::ComboBox::from_id_salt("quality_select")
                        .selected_text(quality_label(s.quality))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut s.quality, VideoQualityMode::Auto, "Auto");
                            ui.selectable_value(&mut s.quality, VideoQualityMode::CrispText, "Crisp Text");
                            ui.selectable_value(&mut s.quality, VideoQualityMode::SmoothVideo, "Smooth Video");
                        });
                    if s.quality != prev_q {
                        s.send_cmd(SessionCommand::Control(ControlMsg::QualityPreset {
                            mode: s.quality,
                            target_fps: 30,
                            h264_bitrate_kbps: 4000,
                        }));
                    }
                    if is_full {
                        ui.checkbox(&mut s.capture_system_keys, "Capture System Keys");
                    }
                    #[cfg(target_os = "android")]
                    if is_full && ui.button("Virtual Keyboard").clicked() {
                        self.android_kbd_open = !self.android_kbd_open;
                        crate::set_android_soft_keyboard(self.android_kbd_open);
                    }
                });
            });

            // Bottom Session Status Strip
            egui::Panel::bottom("session_status_strip").show(ui, |ui| {
                let s = &self.sessions[idx];
                ui.horizontal(|ui| {
                    ui.label(&s.status_banner);
                    if let Some(rtt) = s.last_rtt_ms {
                        ui.separator();
                        ui.label(format!("Latency: {rtt} ms"));
                    }
                    if let Some(frame) = s.latest_frame.lock().unwrap().as_ref() {
                        ui.separator();
                        ui.label(format!("{}x{}", frame.width, frame.height));
                    }
                    if s.capture_system_keys {
                        ui.separator();
                        ui.label("Press Ctrl+Alt+Home to release system key capture");
                    }
                });
            });

            // Central Viewport Area
            egui::CentralPanel::no_frame().show(ui, |ui| {
                let s = &mut self.sessions[idx];
                match &s.phase {
                    ConnectionPhase::Connected => {
                        if s.workspace.stitched {
                            crate::monitor_workspace::paint_stitched(ui, s, self.settings.remote_cursor_sync);
                            return;
                        }
                        if s.mode == SessionMode::FullControl && !s.monitors.is_empty() {
                            s.window_match.tick(ui, s.active_monitor_id, &s.cmd_tx);
                        }
                        let latest_opt = s.monitor_frames.lock().unwrap().get(&s.active_monitor_id).cloned();
                        let Some(frame) = latest_opt else {
                            ui.centered_and_justified(|ui| {
                                ui.label("Waiting for host display...");
                            });
                            return;
                        };

                        let cur_scale = self
                            .session_scales
                            .get(&s.id)
                            .copied()
                            .unwrap_or(self.settings.scale_mode);
                        let filter_opts = match self.settings.texture_filter {
                            TextureFilterMode::Nearest => TextureOptions::NEAREST,
                            TextureFilterMode::Linear => {
                                if cur_scale == ScaleMode::OneToOne || s.quality == VideoQualityMode::CrispText {
                                    TextureOptions::NEAREST
                                } else {
                                    TextureOptions::LINEAR
                                }
                            }
                        };

                        // Session hotkeys from ViewerSettings
                        let typing_hotkey = TypingHotkey::parse(&self.settings.hotkey_type_clipboard);
                        #[cfg(not(target_os = "android"))]
                        if s.mode == SessionMode::FullControl
                            && (self.hook.take_typing_request() || typing_hotkey.is_some_and(|key| {
                                ui.input(|input| input.focused && input.events.iter().any(|event| {
                                    matches!(event, egui::Event::Key { key: pressed, pressed: true, repeat: false, modifiers, .. }
                                        if *pressed == key.key && modifiers.ctrl == key.ctrl
                                            && modifiers.alt == key.alt && modifiers.shift == key.shift)
                                }))
                            }))
                        {
                            toggle_clipboard_typing(s, &mut self.clipboard, &self.settings);
                        }
                        if hotkey_triggered(ui, &self.settings.hotkey_mute_mic) {
                            let prev = s.voice_controls.muted.load(std::sync::atomic::Ordering::Relaxed);
                            s.voice_controls.muted.store(!prev, std::sync::atomic::Ordering::Relaxed);
                        }
                        if s.mode == SessionMode::FullControl && hotkey_triggered(ui, &self.settings.hotkey_cad) {
                            s.send_cmd(SessionCommand::Control(ControlMsg::SpecialAction(
                                SpecialAction::SendCtrlAltDel,
                            )));
                        }
                        if hotkey_triggered(ui, &self.settings.hotkey_fullscreen) {
                            let is_fs = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fs));
                        }
                        if s.monitors.len() > 1 && hotkey_triggered(ui, &self.settings.hotkey_next_monitor) {
                            let pos = s.monitors.iter().position(|m| m.id == s.active_monitor_id).unwrap_or(0);
                            let next_id = s.monitors[(pos + 1) % s.monitors.len()].id;
                            s.active_monitor_id = next_id;
                            s.send_cmd(SessionCommand::Control(ControlMsg::MonitorSelect {
                                monitor_id: next_id,
                            }));
                        }

                        let need_upload = match self.textures.get(&s.id) {
                            Some((seq, opts, _)) => *seq != frame.seq || *opts != filter_opts,
                            None => true,
                        };
                        if need_upload {
                            let img = ColorImage::from_rgba_unmultiplied(
                                [frame.width as usize, frame.height as usize],
                                &frame.rgba,
                            );
                            if let Some((seq, opts, tex)) = self.textures.get_mut(&s.id) {
                                tex.set(img, filter_opts);
                                *seq = frame.seq;
                                *opts = filter_opts;
                            } else {
                                let tex = ui.ctx().load_texture(
                                    format!("remote_fb_{}", s.id),
                                    img,
                                    filter_opts,
                                );
                                self.textures.insert(s.id, (frame.seq, filter_opts, tex));
                            }
                        }

                        let native_size = s.monitors.iter()
                            .find(|monitor| monitor.id == s.active_monitor_id && monitor.width > 0 && monitor.height > 0)
                            .map_or((frame.width, frame.height), |monitor| (monitor.width, monitor.height));
                        let tex_id = self.textures.get(&s.id).map(|(_, _, t)| t.id());
                        if let Some(tid) = tex_id {
                            let ppp = ui.ctx().pixels_per_point();
                            let sys_mode = self.settings.sys_keys_mode;
                            let mut pinch_fit_update: Option<bool> = None;
                            let mut render_panel = |ui: &mut egui::Ui| {
                                let avail = ui.available_rect_before_wrap();
                                let display_rect = compute_display_rect_with_mode(
                                    avail,
                                    native_size.0,
                                    native_size.1,
                                    cur_scale,
                                    ppp,
                                );
                                let response = ui.allocate_rect(display_rect, Sense::click_and_drag());
                                ui.painter().image(
                                    tid,
                                    display_rect,
                                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                    Color32::WHITE,
                                );

                                {
                                    let mut stats = s.video_stats.lock().unwrap();
                                    stats.record_render(frame.seq, std::time::Instant::now());
                                    if s.quality_hud {
                                        stats.paint(ui, display_rect.intersect(ui.clip_rect()), s.last_rtt_ms);
                                    }
                                }
                                if self.settings.remote_cursor_sync {
                                    let local = if s.mode == SessionMode::FullControl && response.hovered() {
                                        ui.input(|input| input.pointer.hover_pos().filter(|_| input.focused))
                                    } else {
                                        None
                                    };
                                    if let Some(cursor) = s.cursors.lock().unwrap().get_mut(&s.active_monitor_id)
                                        && cursor.paint(ui, display_rect, s.active_monitor_id, &frame, native_size, local)
                                        && local.is_some()
                                    {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                                    }
                                }

                                if s.mode == SessionMode::FullControl {
                                    let canvas = display_rect.intersect(ui.clip_rect());
                                    let files = ui.input_mut(|input| {
                                        if !input.raw.dropped_files.is_empty()
                                            && drop_position(input, ppp).is_some_and(|pos| canvas.contains(pos))
                                        {
                                            std::mem::take(&mut input.raw.dropped_files)
                                        } else {
                                            Vec::new()
                                        }
                                    });
                                    if !files.is_empty() {
                                        self.canvas_uploads.entry(s.id).or_default().enqueue(s, files);
                                    }
                                    if let Some(uploads) = self.canvas_uploads.get_mut(&s.id)
                                        && !uploads.paint(ui, canvas, &s.file_transfer.queue)
                                    {
                                        self.canvas_uploads.remove(&s.id);
                                    }
                                }

                                if s.mode == SessionMode::FullControl {
                                    let focused = ui.input(|i| i.viewport().focused.unwrap_or(false));
                                    let is_fs = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
                                    let policy_allows = match sys_mode {
                                        SystemKeysMode::Focused => focused,
                                        SystemKeysMode::Fullscreen => focused && is_fs,
                                        SystemKeysMode::Never => false,
                                    };
                                    hook_allowed = (s.capture_system_keys && focused) || policy_allows;
                                    if hook_allowed {
                                        hook_tx = Some(forward_input_sender(s, &self.runtime));
                                    }

                                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                                        && let Some((nx, ny)) = pointer_to_normalized(pos, display_rect)
                                    {
                                        if ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                                            s.send_cmd(SessionCommand::Input(InputMsg::Pointer(
                                                PointerEvent::MoveAbs {
                                                    norm_x: nx,
                                                    norm_y: ny,
                                                },
                                            )));
                                        }
                                        for (ep_btn, mb) in [
                                            (egui::PointerButton::Primary, MouseBtn::Left),
                                            (egui::PointerButton::Secondary, MouseBtn::Right),
                                            (egui::PointerButton::Middle, MouseBtn::Middle),
                                            (egui::PointerButton::Extra1, MouseBtn::X1),
                                            (egui::PointerButton::Extra2, MouseBtn::X2),
                                        ] {
                                            if ui.input(|i| i.pointer.button_pressed(ep_btn)) {
                                                s.send_cmd(SessionCommand::Input(InputMsg::Pointer(
                                                    PointerEvent::Button {
                                                        button: mb,
                                                        down: true,
                                                        norm_x: nx,
                                                        norm_y: ny,
                                                    },
                                                )));
                                            }
                                            if ui.input(|i| i.pointer.button_released(ep_btn)) {
                                                s.send_cmd(SessionCommand::Input(InputMsg::Pointer(
                                                    PointerEvent::Button {
                                                        button: mb,
                                                        down: false,
                                                        norm_x: nx,
                                                        norm_y: ny,
                                                    },
                                                )));
                                            }
                                        }
                                        let wheel = ui.input(|i| i.smooth_scroll_delta);
                                        if wheel.x.abs() > 0.1 || wheel.y.abs() > 0.1 {
                                            s.send_cmd(SessionCommand::Input(InputMsg::Pointer(
                                                PointerEvent::Wheel {
                                                    delta_x: (wheel.x * 2.0) as i16,
                                                    delta_y: (wheel.y * 2.0) as i16,
                                                },
                                            )));
                                        }
                                    }

                                    // Multi-touch gesture handling (pinch zoom, two-finger scroll & right-click tap)
                                    let now_t = ui.input(|i| i.time);
                                    if let Some(mt) = ui.input(|i| i.multi_touch()) {
                                        if let Some((pinch_fit, wheel_dx, wheel_dy)) = classify_multitouch_step(
                                            mt.num_touches,
                                            mt.zoom_delta,
                                            mt.translation_delta,
                                            mt.start_pos,
                                            now_t,
                                            &mut self.touch_two_finger,
                                        ) {
                                            if let Some(fit) = pinch_fit {
                                                s.fit_scale = fit;
                                                pinch_fit_update = Some(fit);
                                            }
                                            if wheel_dx != 0 || wheel_dy != 0 {
                                                s.send_cmd(SessionCommand::Input(InputMsg::Pointer(
                                                    PointerEvent::Wheel {
                                                        delta_x: wheel_dx,
                                                        delta_y: wheel_dy,
                                                    },
                                                )));
                                            }
                                        }
                                    } else if let Some((tap_pos, start_t, travel)) = self.touch_two_finger.take()
                                        && (now_t - start_t) <= 0.45
                                        && travel < 12.0
                                        && let Some((nx, ny)) = pointer_to_normalized(tap_pos, display_rect)
                                    {
                                        for down in [true, false] {
                                            s.send_cmd(SessionCommand::Input(InputMsg::Pointer(
                                                PointerEvent::Button {
                                                    button: MouseBtn::Right,
                                                    down,
                                                    norm_x: nx,
                                                    norm_y: ny,
                                                },
                                            )));
                                        }
                                    }

                                    // Standard in-window key forwarding when low-level hook isn't active
                                    if !hook_allowed && (response.hovered() || ui.input(|i| i.viewport().focused.unwrap_or(false))) {
                                        let events = ui.input(|i| i.events.clone());
                                        for ev in events {
                                            if let egui::Event::Key { key, pressed, repeat: false, .. } = ev
                                                && !matches!(ev, egui::Event::Key { modifiers, .. } if typing_hotkey.is_some_and(|hotkey| {
                                                    key == hotkey.key && modifiers.ctrl == hotkey.ctrl
                                                        && modifiers.alt == hotkey.alt && modifiers.shift == hotkey.shift
                                                }))
                                                && let Some(ke) = egui_key_to_key_event(key, pressed)
                                            {
                                                s.send_cmd(SessionCommand::Input(InputMsg::Key(ke)));
                                            }
                                        }
                                    }
                                }
                            };

                            let use_scroll = cur_scale == ScaleMode::OneToOne;
                            if use_scroll {
                                ScrollArea::both().show(ui, |ui| {
                                    render_panel(ui);
                                });
                            } else {
                                render_panel(ui);
                            }
                            drop(render_panel);
                            if !s.file_transfer.open
                                && let Some(prompt) = s.file_transfer.pending_overwrite.clone()
                            {
                                render_overwrite_modal(ui.ctx(), s, &prompt);
                            }
                            if let Some(fit) = pinch_fit_update {
                                self.session_scales.insert(
                                    s.id,
                                    if fit {
                                        ScaleMode::Fit
                                    } else {
                                        ScaleMode::OneToOne
                                    },
                                );
                            }
                        }
                    }
                    ConnectionPhase::Connecting => {
                        ui.centered_and_justified(|ui| {
                            ui.label(format!("Connecting to {}:{}...", s.host, s.port));
                        });
                    }
                    ConnectionPhase::TofuPrompt { fingerprint } => {
                        let fp = fingerprint.clone();
                        ui.centered_and_justified(|ui| {
                            egui::Frame::window(ui.style()).show(ui, |ui| {
                                ui.set_max_width(480.0);
                                ui.heading("Verify Host Security Key");
                                ui.label(format!("Connecting to {} ({}:{})", s.title, s.host, s.port));
                                ui.label("Confirm the host certificate fingerprint before continuing:");
                                ui.monospace(&fp);
                                ui.horizontal(|ui| {
                                    if ui.button("Trust & Continue").clicked() {
                                        s.send_cmd(SessionCommand::TrustTofuAndContinue { save_pin: true });
                                    }
                                    if ui.button("Cancel").clicked() {
                                        s.send_cmd(SessionCommand::CancelConnect);
                                    }
                                });
                            });
                        });
                    }
                    ConnectionPhase::TofuMismatch { expected, observed } => {
                        let exp = expected.clone();
                        let obs = observed.clone();
                        ui.centered_and_justified(|ui| {
                            egui::Frame::window(ui.style()).show(ui, |ui| {
                                ui.set_max_width(520.0);
                                ui.colored_label(Color32::RED, "Warning: Host Security Key Changed");
                                ui.label("The certificate presented by this host does not match the saved fingerprint.");
                                ui.label("Expected:");
                                ui.monospace(&exp);
                                ui.label("Received:");
                                ui.monospace(&obs);
                                ui.horizontal(|ui| {
                                    if ui.button("Disconnect").clicked() {
                                        s.send_cmd(SessionCommand::CancelConnect);
                                    }
                                    if ui.button("Replace Trusted Key").clicked() {
                                        s.send_cmd(SessionCommand::TrustTofuAndContinue { save_pin: true });
                                    }
                                });
                            });
                        });
                    }
                    ConnectionPhase::PasswordPrompt { error } => {
                        let err_opt = error.clone();
                        let pw = self.password_inputs.entry(s.id).or_default();
                        ui.centered_and_justified(|ui| {
                            egui::Frame::window(ui.style()).show(ui, |ui| {
                                ui.set_max_width(380.0);
                                ui.heading(format!("Sign in to {}", s.title));
                                ui.label(format!("Mode: {}", mode_label(s.mode)));
                                let resp = ui.add(
                                    egui::TextEdit::singleline(pw)
                                        .password(true)
                                        .hint_text("Host password")
                                        .desired_width(260.0),
                                );
                                if err_opt.is_none() && pw.is_empty() {
                                    resp.request_focus();
                                }
                                if let Some(err) = err_opt {
                                    ui.colored_label(Color32::RED, err);
                                }
                                let submit_enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                                ui.horizontal(|ui| {
                                    if (ui.button("Connect").clicked() || submit_enter) && !pw.is_empty() {
                                        let secret = std::mem::take(pw);
                                        s.send_cmd(SessionCommand::SubmitPassword(secret));
                                    }
                                    if ui.button("Cancel").clicked() {
                                        s.send_cmd(SessionCommand::CancelConnect);
                                    }
                                });
                            });
                        });
                    }
                    ConnectionPhase::Disconnected(reason) => {
                        let reason_str = reason.clone();
                        ui.centered_and_justified(|ui| {
                            ui.vertical_centered(|ui| {
                                ui.heading("Disconnected");
                                ui.label(reason_str);
                            });
                        });
                    }
                }
            });

            self.hook.set_typing_shortcut(TypingHotkey::parse(&self.settings.hotkey_type_clipboard), ui.ctx());
            if !self.hook.update_state(hook_allowed, hook_tx)
                && let Some(s) = self.sessions.iter_mut().find(|s| s.id == active_id)
                && s.capture_system_keys
                && !hook_allowed
            {
                s.capture_system_keys = false;
            }
    }

    fn render_file_transfer_viewports(&mut self, ctx: &Context) {
        #[cfg(target_os = "android")]
        {
            let _ = ctx;
            return;
        }
        #[cfg(not(target_os = "android"))]
        {
        let mut to_close = Vec::new();
        for s_idx in 0..self.sessions.len() {
            let s_id = self.sessions[s_idx].id;
            if !self.sessions[s_idx].file_transfer.open {
                continue;
            }

            let vp_id = ViewportId::from_hash_of(("deskbolt_ft", s_id));
            let title = format!("File Transfer - {}", self.sessions[s_idx].title);
            let builder = ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([1040.0, 680.0])
                .with_min_inner_size([720.0, 480.0]);

            ctx.show_viewport_immediate(vp_id, builder, |ui, _class| {
                if ui.input(|i| i.viewport().close_requested()) {
                    if self.sessions[s_idx].mode == SessionMode::FileTransfer {
                        self.sessions[s_idx].send_cmd(SessionCommand::Disconnect);
                        to_close.push(s_id);
                    } else {
                        self.sessions[s_idx].file_transfer.open = false;
                    }
                    return;
                }
                self.render_file_transfer_inner(ui, s_idx);
            });
        }
        if !to_close.is_empty() {
            self.sessions.retain(|s| !to_close.contains(&s.id));
        }
        }
    }

    #[cfg(target_os = "android")]
    fn render_android_file_transfer(&mut self, ui: &mut egui::Ui, s_idx: usize) {
        let s_id = self.sessions[s_idx].id;
        let mut close_now = false;
        egui::Panel::top("android_ft_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("< Back").clicked() {
                    close_now = true;
                }
                ui.strong(format!("File Transfer - {}", self.sessions[s_idx].title));
            });
        });
        if close_now {
            if self.sessions[s_idx].mode == SessionMode::FileTransfer {
                self.sessions[s_idx].send_cmd(SessionCommand::Disconnect);
                self.sessions.retain(|s| s.id != s_id);
            } else {
                self.sessions[s_idx].file_transfer.open = false;
            }
            return;
        }
        self.render_file_transfer_inner(ui, s_idx);
    }

    fn render_file_transfer_inner(&mut self, ui: &mut egui::Ui, s_idx: usize) {
        let s_id = self.sessions[s_idx].id;

                // Bottom Queue Panel
                egui::Panel::bottom(format!("ft_queue_{s_id}"))
                    .default_size(165.0)
                    .resizable(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.strong("Transfer Queue");
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Clear Done").clicked() {
                                    self.sessions[s_idx].file_transfer.queue.retain(|q| {
                                        !matches!(
                                            q.status,
                                            TransferItemStatus::Verified
                                                | TransferItemStatus::Skipped
                                                | TransferItemStatus::Failed(_)
                                        )
                                    });
                                }
                            });
                        });
                        ui.separator();
                        ScrollArea::vertical().show(ui, |ui| {
                            let items = self.sessions[s_idx].file_transfer.queue.clone();
                            for item in items {
                                ui.horizontal(|ui| {
                                    let dir = match item.direction {
                                        TransferDir::Upload => "->",
                                        TransferDir::Download => "<-",
                                    };
                                    ui.label(format!("{dir} {}", item.file_name));
                                    let frac = if item.total_bytes > 0 {
                                        (item.bytes_done as f32 / item.total_bytes as f32).clamp(0.0, 1.0)
                                    } else if item.status == TransferItemStatus::Verified {
                                        1.0
                                    } else {
                                        0.0
                                    };
                                    ui.add(egui::ProgressBar::new(frac).desired_width(180.0).show_percentage());
                                    let status_text = match &item.status {
                                        TransferItemStatus::Queued => "Queued".into(),
                                        TransferItemStatus::WaitingOverwrite => "Waiting overwrite decision".into(),
                                        TransferItemStatus::Transferring => {
                                            format!("{:.1} MB/s", item.speed_bps as f64 / (1024.0 * 1024.0))
                                        }
                                        TransferItemStatus::Verifying => "Verifying SHA-256...".into(),
                                        TransferItemStatus::Verified => "Verified (SHA-256)".into(),
                                        TransferItemStatus::Skipped => "Skipped".into(),
                                        TransferItemStatus::Failed(e) => format!("Failed: {e}"),
                                    };
                                    ui.label(status_text);
                                    if matches!(
                                        item.status,
                                        TransferItemStatus::Queued | TransferItemStatus::Transferring
                                    ) && ui.small_button("Cancel").clicked()
                                    {
                                        self.sessions[s_idx].send_cmd(SessionCommand::CancelTransfer(item.id));
                                    }
                                });
                            }
                        });
                    });

                let mut open_sync = false;
                // Dual-Pane Central Area
                egui::CentralPanel::default().show(ui, |ui| {
                    let s = &mut self.sessions[s_idx];
                    if !matches!(s.phase, ConnectionPhase::Connected) {
                        ui.centered_and_justified(|ui| {
                            ui.label("Complete authentication in the session tab to browse remote files.");
                        });
                        return;
                    }

                    ui.columns(2, |cols| {
                        // Left Pane: Local Computer
                        cols[0].group(|ui| {
                            ui.strong("Local Computer");
                            ui.horizontal(|ui| {
                                let cur_drive = s
                                    .file_transfer
                                    .local_path
                                    .to_string_lossy()
                                    .chars()
                                    .take(3)
                                    .collect::<String>();
                                egui::ComboBox::from_id_salt("local_drive")
                                    .selected_text(cur_drive)
                                    .show_ui(ui, |ui| {
                                        for drv in s.file_transfer.local_roots.clone() {
                                            if ui.selectable_label(false, &drv).clicked() {
                                                s.file_transfer.local_path = PathBuf::from(&drv);
                                                s.file_transfer.refresh_local();
                                            }
                                        }
                                    });
                                let mut path_str = s.file_transfer.local_path.to_string_lossy().into_owned();
                                let p_resp = ui.text_edit_singleline(&mut path_str);
                                if p_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                    s.file_transfer.local_path = PathBuf::from(path_str);
                                    s.file_transfer.refresh_local();
                                } else if p_resp.changed() {
                                    s.file_transfer.local_path = PathBuf::from(path_str);
                                }
                                if ui.button("Up").clicked()
                                    && let Some(parent) = s.file_transfer.local_path.parent()
                                {
                                    s.file_transfer.local_path = parent.to_path_buf();
                                    s.file_transfer.refresh_local();
                                }
                                if ui.button("Refresh").clicked() {
                                    s.file_transfer.refresh_local();
                                }
                            });

                            ui.horizontal(|ui| {
                                if ui.button("Upload ->").clicked() {
                                    let selected: Vec<String> = s.file_transfer.local_selected.iter().cloned().collect();
                                    for name in selected {
                                        if let Some(entry) = s.file_transfer.local_entries.iter().find(|e| e.name == name)
                                            && !entry.is_dir
                                        {
                                            let lp = s.file_transfer.local_path.join(&name);
                                            let rp = join_remote_path(&s.file_transfer.remote_path, &name);
                                            s.send_cmd(SessionCommand::QueueUpload {
                                                local_path: lp,
                                                remote_path: rp,
                                            });
                                        }
                                    }
                                }
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.local_mkdir_input)
                                        .hint_text("New folder")
                                        .desired_width(100.0),
                                );
                                if ui.button("Create").clicked() && !self.local_mkdir_input.trim().is_empty() {
                                    let p = s.file_transfer.local_path.join(self.local_mkdir_input.trim());
                                    let _ = std::fs::create_dir_all(p);
                                    self.local_mkdir_input.clear();
                                    s.file_transfer.refresh_local();
                                }
                            });

                            if let Some(err) = &s.file_transfer.local_error {
                                ui.colored_label(Color32::RED, err);
                            }

                            ScrollArea::vertical().id_salt("local_file_scroll").show(ui, |ui| {
                                let entries = s.file_transfer.local_entries.clone();
                                for e in entries {
                                    let sel = s.file_transfer.local_selected.contains(&e.name);
                                    let label = if e.is_dir {
                                        format!("[DIR] {}", e.name)
                                    } else {
                                        format!("{}  ({})", e.name, format_bytes(e.size))
                                    };
                                    let r = ui.selectable_label(sel, label);
                                    if r.clicked() {
                                        if !ui.input(|i| i.modifiers.ctrl) {
                                            s.file_transfer.local_selected.clear();
                                        }
                                        if sel {
                                            s.file_transfer.local_selected.remove(&e.name);
                                        } else {
                                            s.file_transfer.local_selected.insert(e.name.clone());
                                        }
                                    }
                                    if r.double_clicked() && e.is_dir {
                                        s.file_transfer.local_path = s.file_transfer.local_path.join(&e.name);
                                        s.file_transfer.refresh_local();
                                    }
                                }
                            });
                        });

                        // Right Pane: Remote Host
                        cols[1].group(|ui| {
                            ui.strong(format!("Remote: {}", s.title));
                            ui.horizontal(|ui| {
                                let cur_rdrv = s
                                    .file_transfer
                                    .remote_path
                                    .chars()
                                    .take(3)
                                    .collect::<String>();
                                egui::ComboBox::from_id_salt("remote_drive")
                                    .selected_text(cur_rdrv)
                                    .show_ui(ui, |ui| {
                                        for drv in s.file_transfer.remote_roots.clone() {
                                            if ui.selectable_label(false, &drv.letter).clicked() {
                                                s.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq {
                                                    path: drv.letter,
                                                }));
                                            }
                                        }
                                    });
                                let p_resp = ui.text_edit_singleline(&mut s.file_transfer.remote_path);
                                if p_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                    s.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq {
                                        path: s.file_transfer.remote_path.clone(),
                                    }));
                                }
                                if ui.button("Up").clicked() {
                                    let parent = parent_remote_path(&s.file_transfer.remote_path);
                                    s.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq {
                                        path: parent,
                                    }));
                                }
                                if ui.button("Refresh").clicked() {
                                    s.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq {
                                        path: s.file_transfer.remote_path.clone(),
                                    }));
                                }
                            });

                            ui.horizontal(|ui| {
                                if ui.button("<- Download").clicked() {
                                    let selected: Vec<String> = s.file_transfer.remote_selected.iter().cloned().collect();
                                    for name in selected {
                                        if let Some(entry) = s.file_transfer.remote_entries.iter().find(|e| e.name == name)
                                            && !entry.is_dir
                                        {
                                            let rp = join_remote_path(&s.file_transfer.remote_path, &name);
                                            let lp = s.file_transfer.local_path.join(&name);
                                            s.send_cmd(SessionCommand::QueueDownload {
                                                remote_path: rp,
                                                local_path: lp,
                                                total_bytes: entry.size,
                                            });
                                        }
                                    }
                                }
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.remote_mkdir_input)
                                        .hint_text("New folder")
                                        .desired_width(100.0),
                                );
                                if ui.button("Create").clicked() && !self.remote_mkdir_input.trim().is_empty() {
                                    let rp = join_remote_path(&s.file_transfer.remote_path, self.remote_mkdir_input.trim());
                                    self.remote_mkdir_input.clear();
                                    s.send_cmd(SessionCommand::File(FileTransferMsg::MkdirReq { path: rp }));
                                }
                                if ui.button("Compare & Sync...").clicked() {
                                    open_sync = true;
                                }
                            });

                            if let Some(err) = &s.file_transfer.remote_error {
                                ui.colored_label(Color32::RED, err);
                            }

                            ScrollArea::vertical().id_salt("remote_file_scroll").show(ui, |ui| {
                                let entries = s.file_transfer.remote_entries.clone();
                                for e in entries {
                                    let sel = s.file_transfer.remote_selected.contains(&e.name);
                                    let label = if e.is_dir {
                                        format!("[DIR] {}", e.name)
                                    } else {
                                        format!("{}  ({})", e.name, format_bytes(e.size))
                                    };
                                    let r = ui.selectable_label(sel, label);
                                    if r.clicked() {
                                        if !ui.input(|i| i.modifiers.ctrl) {
                                            s.file_transfer.remote_selected.clear();
                                        }
                                        if sel {
                                            s.file_transfer.remote_selected.remove(&e.name);
                                        } else {
                                            s.file_transfer.remote_selected.insert(e.name.clone());
                                        }
                                    }
                                    r.context_menu(|ui| {
                                        if !e.is_dir && ui.button("Quick Edit").clicked() {
                                            let rp = join_remote_path(&s.file_transfer.remote_path, &e.name);
                                            s.send_cmd(SessionCommand::QuickEditFetch { remote_path: rp });
                                            ui.close();
                                        }
                                        if !e.is_dir && ui.button("Tail Live Log").clicked() {
                                            let rp = join_remote_path(&s.file_transfer.remote_path, &e.name);
                                            s.log_tail = Some(LogTailBuffer {
                                                remote_path: rp.clone(),
                                                text: String::new(),
                                                next_offset: 0,
                                                following: true,
                                                status: Some("Loading tail...".into()),
                                            });
                                            s.send_cmd(SessionCommand::File(FileTransferMsg::TailChunkReq {
                                                path: rp,
                                                offset: 0,
                                                max_bytes: 64 * 1024,
                                            }));
                                            ui.close();
                                        }
                                        if ui.button("Rename...").clicked() {
                                            let old_path = join_remote_path(&s.file_transfer.remote_path, &e.name);
                                            self.rename_modal = Some(RenameItemModal {
                                                session_id: s.id,
                                                is_remote: true,
                                                old_path,
                                                new_name: e.name.clone(),
                                            });
                                            ui.close();
                                        }
                                        if ui.button("Delete").clicked() {
                                            let rp = join_remote_path(&s.file_transfer.remote_path, &e.name);
                                            s.send_cmd(SessionCommand::File(FileTransferMsg::DeleteReq {
                                                path: rp,
                                                recursive: e.is_dir,
                                            }));
                                            ui.close();
                                        }
                                    });
                                    if r.double_clicked() {
                                        if e.is_dir {
                                            let next = join_remote_path(&s.file_transfer.remote_path, &e.name);
                                            s.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq { path: next }));
                                        } else if is_quick_edit_ext(&e.name) {
                                            let rp = join_remote_path(&s.file_transfer.remote_path, &e.name);
                                            s.send_cmd(SessionCommand::QuickEditFetch { remote_path: rp });
                                        }
                                    }
                                }
                            });
                        });
                    });

                    // Overwrite Prompt Modal
                    if let Some(prompt) = s.file_transfer.pending_overwrite.clone() {
                        render_overwrite_modal(ui.ctx(), s, &prompt);
                    }
        });

        if open_sync {
            self.open_compare_sync_for_session(s_id);
        }

        if let Some(mut rmodal) = self.rename_modal.take() {
            let mut keep_open = true;
            Window::new("Rename Item")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("New Name:");
                        ui.text_edit_singleline(&mut rmodal.new_name);
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() && !rmodal.new_name.trim().is_empty() {
                            if let Some(s) = self.sessions.iter().find(|s| s.id == rmodal.session_id)
                                && rmodal.is_remote
                            {
                                let parent = parent_remote_path(&rmodal.old_path);
                                let to_path = join_remote_path(&parent, rmodal.new_name.trim());
                                s.send_cmd(SessionCommand::File(FileTransferMsg::RenameReq {
                                    from_path: rmodal.old_path.clone(),
                                    to_path,
                                }));
                            }
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
            if keep_open {
                self.rename_modal = Some(rmodal);
            }
        }
    }
}

fn render_overwrite_modal(ctx: &Context, s: &mut ViewerSession, prompt: &OverwritePrompt) {
    Window::new("Overwrite Existing File")
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(format!("File '{}' already exists at destination.", prompt.file_name));
            ui.label(format!("Target: {}", prompt.remote_path));
            ui.label(format!(
                "Local: {}  |  Remote: {}",
                format_bytes(prompt.local_size),
                format_bytes(prompt.remote_size)
            ));
            ui.horizontal(|ui| {
                if ui.button("Overwrite").clicked() {
                    s.file_transfer.pending_overwrite = None;
                    s.send_cmd(SessionCommand::ResolveOverwrite {
                        transfer_id: prompt.transfer_id,
                        overwrite: true,
                        apply_all: false,
                    });
                }
                if ui.button("Overwrite All").clicked() {
                    s.file_transfer.pending_overwrite = None;
                    s.send_cmd(SessionCommand::ResolveOverwrite {
                        transfer_id: prompt.transfer_id,
                        overwrite: true,
                        apply_all: true,
                    });
                }
                if ui.button("Skip").clicked() {
                    s.file_transfer.pending_overwrite = None;
                    s.send_cmd(SessionCommand::ResolveOverwrite {
                        transfer_id: prompt.transfer_id,
                        overwrite: false,
                        apply_all: false,
                    });
                }
                if ui.button("Skip All").clicked() {
                    s.file_transfer.pending_overwrite = None;
                    s.send_cmd(SessionCommand::ResolveOverwrite {
                        transfer_id: prompt.transfer_id,
                        overwrite: false,
                        apply_all: true,
                    });
                }
            });
        });
}

#[cfg(not(target_os = "android"))]
fn toggle_clipboard_typing(
    session: &mut ViewerSession,
    clipboard: &mut Option<NativeClipboard>,
    settings: &ViewerSettings,
) {
    if session.typing_clipboard {
        session.send_cmd(SessionCommand::StopTyping);
        return;
    }
    if clipboard.is_none() {
        *clipboard = NativeClipboard::open();
    }
    if let Some(text) = clipboard.as_mut().and_then(|c| c.text(settings.clipboard_max_bytes))
        && !text.is_empty()
    {
        session.send_cmd(SessionCommand::TypeClipboard { text, delay_ms: settings.clipboard_key_delay_ms });
    } else {
        session.status_banner = "Clipboard text is empty, unavailable, or exceeds the size limit.".into();
    }
}

fn forward_input_sender(
    s: &ViewerSession,
    rt: &tokio::runtime::Handle,
) -> tokio::sync::mpsc::Sender<InputMsg> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<InputMsg>(64);
    let cmd_tx = s.cmd_tx.clone();
    rt.spawn(async move {
        while let Some(inp) = rx.recv().await {
            if cmd_tx.send(SessionCommand::Input(inp)).await.is_err() {
                break;
            }
        }
    });
    tx
}

fn drop_position(input: &egui::InputState, ppp: f32) -> Option<Pos2> {
    if let Some(window) = input.viewport().inner_rect
        && let Some(screen) = native_drop_point(input.viewport())
    {
        return Some(Pos2::ZERO + (screen / ppp - window.min));
    }
    input.pointer.hover_pos()
}

#[cfg(target_os = "windows")]
fn native_drop_point(_: &egui::ViewportInfo) -> Option<Pos2> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut cursor = POINT::default();
    // SAFETY: The OS writes the current screen point to this initialized structure.
    unsafe { GetCursorPos(&mut cursor).ok()?; }
    Some(Pos2::new(cursor.x as f32, cursor.y as f32))
}

#[cfg(target_os = "macos")]
fn native_drop_point(viewport: &egui::ViewportInfo) -> Option<Pos2> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Point { x: f64, y: f64 }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: *const c_void) -> *const c_void;
        fn CGEventGetLocation(event: *const c_void) -> Point;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: *const c_void);
    }
    // SAFETY: A null source requests the current cursor position. The owned event is released once.
    let point = unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() { return None; }
        let location = CGEventGetLocation(event);
        CFRelease(event);
        location
    };
    let scale = viewport.native_pixels_per_point.unwrap_or(1.0);
    Some(Pos2::new(point.x as f32 * scale, point.y as f32 * scale))
}

#[cfg(target_os = "linux")]
fn native_drop_point(_: &egui::ViewportInfo) -> Option<Pos2> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let pointer = connection.query_pointer(root).ok()?.reply().ok()?;
    pointer.same_screen.then_some(Pos2::new(f32::from(pointer.root_x), f32::from(pointer.root_y)))
}

#[cfg(target_os = "android")]
fn native_drop_point(_: &egui::ViewportInfo) -> Option<Pos2> {
    None
}

/// Computes the rendered screen rect inside `avail` using `ScaleMode` (`Fit`, `1:1 Pixel`, or `Stretch`).
pub fn compute_display_rect_with_mode(
    avail: Rect,
    remote_w: u32,
    remote_h: u32,
    mode: ScaleMode,
    pixels_per_point: f32,
) -> Rect {
    match mode {
        ScaleMode::Stretch => avail,
        ScaleMode::Fit => compute_display_rect(avail, remote_w, remote_h, true, pixels_per_point),
        ScaleMode::OneToOne => compute_display_rect(avail, remote_w, remote_h, false, pixels_per_point),
    }
}

fn hotkey_triggered(ui: &egui::Ui, spec: &str) -> bool {
    let parts: Vec<String> = spec
        .split('+')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return false;
    }
    let want_ctrl = parts.iter().any(|p| p == "ctrl" || p == "control");
    let want_alt = parts.iter().any(|p| p == "alt");
    let want_shift = parts.iter().any(|p| p == "shift");
    let main_key = match parts.last().map(String::as_str) {
        Some("f10") => Key::F10,
        Some("f11") => Key::F11,
        Some("f12") => Key::F12,
        Some("end") => Key::End,
        Some("m") => Key::M,
        Some("s") => Key::S,
        _ => return false,
    };
    ui.input(|i| {
        i.modifiers.ctrl == want_ctrl
            && i.modifiers.alt == want_alt
            && i.modifiers.shift == want_shift
            && i.key_pressed(main_key)
    })
}

/// Computes the rendered screen rect inside `avail` for either `1:1` physical pixels or aspect-locked `Fit`.
pub fn compute_display_rect(
    avail: Rect,
    remote_w: u32,
    remote_h: u32,
    fit_scale: bool,
    pixels_per_point: f32,
) -> Rect {
    let rw = remote_w.max(1) as f32;
    let rh = remote_h.max(1) as f32;
    if !fit_scale {
        let ppp = pixels_per_point.max(0.5);
        let size = Vec2::new(rw / ppp, rh / ppp);
        return Rect::from_min_size(avail.min, size);
    }
    let aw = avail.width().max(1.0);
    let ah = avail.height().max(1.0);
    let scale = (aw / rw).min(ah / rh);
    let draw_w = rw * scale;
    let draw_h = rh * scale;
    let x = avail.min.x + (aw - draw_w) * 0.5;
    let y = avail.min.y + (ah - draw_h) * 0.5;
    Rect::from_min_size(Pos2::new(x, y), Vec2::new(draw_w, draw_h))
}

/// Maps pointer position inside `display_rect` into `[0.0, 1.0]` normalized coordinates; `None` if outside.
pub fn pointer_to_normalized(pos: Pos2, display_rect: Rect) -> Option<(f32, f32)> {
    if !display_rect.contains(pos) || display_rect.width() <= 0.0 || display_rect.height() <= 0.0 {
        return None;
    }
    let nx = ((pos.x - display_rect.min.x) / display_rect.width()).clamp(0.0, 1.0);
    let ny = ((pos.y - display_rect.min.y) / display_rect.height()).clamp(0.0, 1.0);
    Some((nx, ny))
}

fn mode_label(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::FullControl => "Full Control",
        SessionMode::ViewOnly => "View Only",
        SessionMode::FileTransfer => "File Transfer",
    }
}

fn quality_label(q: VideoQualityMode) -> &'static str {
    match q {
        VideoQualityMode::Auto => "Auto",
        VideoQualityMode::CrispText => "Crisp Text",
        VideoQualityMode::SmoothVideo => "Smooth Video",
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

pub(crate) fn join_remote_path(base: &str, name: &str) -> String {
    if base.ends_with('\\') || base.ends_with('/') {
        format!("{base}{name}")
    } else if base.starts_with('/') {
        format!("{base}/{name}")
    } else {
        format!("{base}\\{name}")
    }
}

pub(crate) fn parent_remote_path(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }
    let is_posix = path.starts_with('/');
    let trimmed = path.trim_end_matches(['\\', '/']);
    if !is_posix && trimmed.len() <= 2 && trimmed.ends_with(':') {
        return format!("{trimmed}\\");
    }
    if let Some(pos) = trimmed.rfind(['\\', '/']) {
        let prefix = &trimmed[..pos];
        if is_posix && prefix.is_empty() {
            return "/".to_string();
        }
        if !is_posix && prefix.len() == 2 && prefix.ends_with(':') {
            return format!("{prefix}\\");
        }
        return prefix.to_string();
    }
    path.to_string()
}

/// Processes a single frame of two-finger/pinch touch gesture state.
/// Returns `Some((optional_fit_scale_toggle, wheel_dx, wheel_dy))` when active.
pub fn classify_multitouch_step(
    num_touches: usize,
    zoom_delta: f32,
    translation_delta: Vec2,
    start_pos: Pos2,
    now_secs: f64,
    state: &mut Option<(Pos2, f64, f32)>,
) -> Option<(Option<bool>, i16, i16)> {
    if num_touches < 2 {
        return None;
    }
    let entry = state.get_or_insert((start_pos, now_secs, 0.0));
    entry.2 += translation_delta.length();

    let fit_toggle = if zoom_delta > 1.06 {
        Some(false)
    } else if zoom_delta < 0.94 {
        Some(true)
    } else {
        None
    };

    let (dx, dy) = if num_touches == 2 && translation_delta.length() > 0.5 && fit_toggle.is_none() {
        (
            (translation_delta.x * 2.0) as i16,
            (translation_delta.y * 2.0) as i16,
        )
    } else {
        (0, 0)
    };
    Some((fit_toggle, dx, dy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_and_one_to_one_viewport_math_and_letterbox_filtering() {
        let avail = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0));
        // 1920x1080 into 1000x800 -> width-limited (1000 x 562.5), letterboxed vertically
        let fit = compute_display_rect(avail, 1920, 1080, true, 1.0);
        assert!((fit.width() - 1000.0).abs() < 0.1);
        assert!((fit.height() - 562.5).abs() < 0.1);
        assert!(fit.min.y > 100.0);

        // Click inside letterbox top bar returns None
        assert_eq!(pointer_to_normalized(Pos2::new(500.0, 20.0), fit), None);
        // Click at center returns ~(0.5, 0.5)
        let (nx, ny) = pointer_to_normalized(fit.center(), fit).unwrap();
        assert!((nx - 0.5).abs() < 0.01 && (ny - 0.5).abs() < 0.01);

        // 1:1 Scale with 2.0 DPI scale maps physical 1920x1080 to 960x540 logical points
        let one_to_one = compute_display_rect(avail, 1920, 1080, false, 2.0);
        assert_eq!(one_to_one.size(), Vec2::new(960.0, 540.0));

        // Stretch ignores aspect ratio and fills full available viewport
        let stretch = compute_display_rect_with_mode(avail, 1920, 1080, ScaleMode::Stretch, 1.0);
        assert_eq!(stretch, avail);
    }

    #[test]
    fn remote_path_helpers_and_multitouch_classification() {
        assert_eq!(join_remote_path(r"C:\", "Users"), r"C:\Users");
        assert_eq!(join_remote_path(r"C:\Users", "Admin"), r"C:\Users\Admin");
        assert_eq!(parent_remote_path(r"C:\Users\Admin"), r"C:\Users");
        assert_eq!(parent_remote_path(r"C:\Users"), r"C:\");

        assert_eq!(join_remote_path("/", "home"), "/home");
        assert_eq!(join_remote_path("/home", "admin"), "/home/admin");
        assert_eq!(parent_remote_path("/home/admin"), "/home");
        assert_eq!(parent_remote_path("/home"), "/");
        assert_eq!(parent_remote_path("/"), "/");

        let mut st = None;
        let out = classify_multitouch_step(2, 1.15, Vec2::ZERO, Pos2::new(50.0, 50.0), 1.0, &mut st);
        assert_eq!(out, Some((Some(false), 0, 0)));
        let drag = classify_multitouch_step(2, 1.0, Vec2::new(0.0, 12.0), Pos2::new(50.0, 50.0), 1.1, &mut st);
        assert_eq!(drag, Some((None, 0, 24)));
        assert!(st.unwrap().2 >= 12.0);
    }
}
