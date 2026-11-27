use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use deskbolt_proto::{ControlMsg, InputMsg, MonitorInfo, MouseBtn, PointerEvent, SessionMode};
use egui::{Color32, Context, Event, Pos2, Rect, Sense, TextureHandle, TextureOptions, Ui, Vec2};
use tokio::sync::mpsc;

use crate::hook::egui_key_to_key_event;
use crate::session::{
    ConnectionPhase, MonitorCursors, MonitorFrames, SessionCommand, ViewerSession,
};

pub const MAX_SURFACES: usize = 16;

#[derive(Default)]
pub(crate) struct Workspace {
    pub stitched: bool,
    popouts: HashMap<u8, Arc<Popout>>,
    subscribed: bool,
    canvas: Canvas,
}

impl Workspace {
    #[cfg(not(target_os = "android"))]
    pub fn toggle_windows(
        &mut self,
        monitors: &[MonitorInfo],
        sender: &mpsc::Sender<SessionCommand>,
    ) {
        if self.popouts.is_empty() {
            for monitor in monitors.iter().take(MAX_SURFACES) {
                self.popouts.insert(monitor.id, Arc::new(Popout::default()));
            }
        } else {
            for window in self.popouts.values() {
                window.canvas.lock().unwrap().release_pointer(sender);
            }
            self.popouts.clear();
        }
    }

    pub fn has_windows(&self) -> bool {
        !self.popouts.is_empty()
    }

    pub fn subscribed(&self) -> bool {
        self.subscribed
    }

    pub fn synchronize(&mut self, monitors: &[MonitorInfo], sender: &mpsc::Sender<SessionCommand>) {
        self.popouts.retain(|id, window| {
            !window.closed.load(Ordering::Relaxed)
                && monitors.iter().any(|monitor| monitor.id == *id)
        });
        let enabled = self.stitched || self.has_windows();
        if enabled != self.subscribed
            && sender
                .try_send(SessionCommand::Control(ControlMsg::MonitorWorkspace {
                    enabled,
                }))
                .is_ok()
        {
            self.subscribed = enabled;
        }
    }
}

#[derive(Default)]
struct Popout {
    closed: AtomicBool,
    canvas: Mutex<Canvas>,
}

#[derive(Default)]
struct Canvas {
    drawing: crate::annotations::Drawing,
    textures: HashMap<u8, (u64, TextureHandle)>,
    pressed: [Option<Hit>; 5],
    hover: Option<Hit>,
}

#[derive(Clone, Copy)]
struct Hit {
    monitor: u8,
    x: f32,
    y: f32,
}

struct Layout {
    canvas: Rect,
    screens: Vec<(u8, Rect)>,
}

impl Layout {
    fn fit(monitors: &[MonitorInfo], area: Rect) -> Option<Self> {
        let screens: Vec<_> = monitors
            .iter()
            .filter(|m| m.width > 0 && m.height > 0 && m.width <= 16_384 && m.height <= 16_384)
            .take(MAX_SURFACES)
            .collect();
        let left = screens.iter().map(|m| m.left).min()?;
        let top = screens.iter().map(|m| m.top).min()?;
        let right = screens
            .iter()
            .map(|m| i64::from(m.left) + i64::from(m.width))
            .max()?;
        let bottom = screens
            .iter()
            .map(|m| i64::from(m.top) + i64::from(m.height))
            .max()?;
        let size = Vec2::new(
            (right - i64::from(left)) as f32,
            (bottom - i64::from(top)) as f32,
        );
        let scale = (area.width() / size.x).min(area.height() / size.y);
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let canvas = Rect::from_center_size(area.center(), size * scale);
        let screens = screens
            .into_iter()
            .map(|monitor| {
                let origin = canvas.min
                    + Vec2::new(
                        (i64::from(monitor.left) - i64::from(left)) as f32,
                        (i64::from(monitor.top) - i64::from(top)) as f32,
                    ) * scale;
                (
                    monitor.id,
                    Rect::from_min_size(
                        origin,
                        Vec2::new(monitor.width as f32, monitor.height as f32) * scale,
                    ),
                )
            })
            .collect();
        Some(Self { canvas, screens })
    }

    fn hit(&self, pos: Pos2) -> Option<Hit> {
        self.screens.iter().rev().find_map(|&(monitor, area)| {
            if pos.x >= area.min.x
                && pos.x < area.max.x
                && pos.y >= area.min.y
                && pos.y < area.max.y
            {
                Some(Hit {
                    monitor,
                    x: (pos.x - area.min.x) / area.width(),
                    y: (pos.y - area.min.y) / area.height(),
                })
            } else {
                None
            }
        })
    }
}

impl Hit {
    fn send(self, sender: &mpsc::Sender<SessionCommand>, event: PointerEvent) {
        let _ = sender.try_send(SessionCommand::Input(InputMsg::MonitorPointer {
            monitor_id: self.monitor,
            event,
        }));
    }
}

struct Sources<'a> {
    monitors: &'a [MonitorInfo],
    frames: &'a MonitorFrames,
    cursors: Option<&'a MonitorCursors>,
    sender: &'a mpsc::Sender<SessionCommand>,
    controllable: bool,
    annotation: crate::annotations::Tool,
}

impl Canvas {
    fn paint(&mut self, ui: &mut Ui, sources: Sources<'_>) -> Option<Rect> {
        let layout = Layout::fit(sources.monitors, ui.available_rect_before_wrap())?;
        ui.allocate_rect(layout.canvas, Sense::click_and_drag());
        let painter = ui.painter_at(layout.canvas);
        painter.rect_filled(layout.canvas, 0.0, Color32::BLACK);
        let frames = sources.frames.lock().unwrap().clone();
        self.textures
            .retain(|id, _| layout.screens.iter().any(|(monitor, _)| monitor == id));
        for &(id, area) in &layout.screens {
            let Some(frame) = frames.get(&id) else {
                continue;
            };
            let entry = self.textures.entry(id);
            let texture = match entry {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().0 != frame.seq {
                        let (seq, texture) = entry.get_mut();
                        texture.set(image(frame), TextureOptions::LINEAR);
                        *seq = frame.seq;
                    }
                    entry.into_mut()
                }
                std::collections::hash_map::Entry::Vacant(entry) => entry.insert((
                    frame.seq,
                    ui.ctx().load_texture(
                        format!("Display {id}"),
                        image(frame),
                        TextureOptions::LINEAR,
                    ),
                )),
            };
            painter.image(
                texture.1.id(),
                area,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            if let Some(cursors) = sources.cursors
                && let Some(monitor) = sources.monitors.iter().find(|monitor| monitor.id == id)
                && let Some(cursor) = cursors.lock().unwrap().get_mut(&id)
            {
                let local = sources
                    .controllable
                    .then(|| ui.input(|i| i.pointer.hover_pos()))
                    .flatten()
                    .filter(|pos| area.contains(*pos));
                if cursor.paint(ui, area, id, frame, (monitor.width, monitor.height), local)
                    && local.is_some()
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                }
            }
        }
        if sources.annotation != crate::annotations::Tool::Off {
            self.release_pointer(sources.sender);
            self.drawing.paint(ui, &layout.screens, sources.annotation, sources.sender);
        } else {
            self.drawing = crate::annotations::Drawing::default();
            if sources.controllable {
                self.route_input(ui, &layout, sources.sender);
            }
        }
        Some(layout.canvas)
    }

    fn release_pointer(&mut self, sender: &mpsc::Sender<SessionCommand>) {
        for (index, state) in self.pressed.iter_mut().enumerate() {
            if let Some(hit) = state.take() {
                hit.send(sender, button_event(index, false, hit));
            }
        }
    }

    fn route_input(&mut self, ui: &Ui, layout: &Layout, sender: &mpsc::Sender<SessionCommand>) {
        let (events, focused) = ui.input(|i| (i.events.clone(), i.focused));
        if !focused {
            self.release_pointer(sender);
            if events
                .iter()
                .any(|event| matches!(event, Event::WindowFocused(false)))
            {
                let _ = sender.try_send(SessionCommand::Input(InputMsg::ReleaseAllKeys));
            }
            self.hover = None;
            return;
        }
        self.hover = ui
            .input(|i| i.pointer.hover_pos())
            .and_then(|point| layout.hit(point));
        for event in events {
            match event {
                Event::PointerMoved(point) => {
                    self.hover = layout.hit(point);
                    if let Some(hit) = self.hover {
                        hit.send(
                            sender,
                            PointerEvent::MoveAbs {
                                norm_x: hit.x,
                                norm_y: hit.y,
                            },
                        );
                        for position in self.pressed.iter_mut().flatten() {
                            *position = hit;
                        }
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let index = button as usize;
                    let hit = layout.hit(pos);
                    let mapped = if pressed {
                        self.pressed[index] = hit;
                        hit
                    } else {
                        self.pressed[index]
                            .take()
                            .map(|previous| hit.unwrap_or(previous))
                    };
                    if let Some(hit) = mapped {
                        hit.send(sender, button_event(index, pressed, hit));
                    }
                }
                Event::PointerGone => self.hover = None,
                Event::MouseWheel { delta, .. } => {
                    if let Some(hit) = self.hover {
                        hit.send(
                            sender,
                            PointerEvent::Wheel {
                                delta_x: (delta.x * 2.0) as i16,
                                delta_y: (delta.y * 2.0) as i16,
                            },
                        );
                    }
                }
                Event::Key {
                    key,
                    pressed,
                    repeat: false,
                    ..
                } => {
                    if let Some(key) = egui_key_to_key_event(key, pressed) {
                        let _ = sender.try_send(SessionCommand::Input(InputMsg::Key(key)));
                    }
                }
                _ => {}
            }
        }
    }
}

fn button_event(index: usize, down: bool, hit: Hit) -> PointerEvent {
    let button = [
        MouseBtn::Left,
        MouseBtn::Right,
        MouseBtn::Middle,
        MouseBtn::X1,
        MouseBtn::X2,
    ][index];
    PointerEvent::Button {
        button,
        down,
        norm_x: hit.x,
        norm_y: hit.y,
    }
}

fn image(frame: &crate::session::DecodedFrame) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [frame.width as usize, frame.height as usize],
        &frame.rgba,
    )
}

pub(crate) fn paint_stitched(ui: &mut Ui, session: &mut ViewerSession, show_cursor: bool) {
    let sources = Sources {
        monitors: &session.monitors,
        frames: &session.monitor_frames,
        cursors: show_cursor.then_some(&session.cursors),
        sender: &session.cmd_tx,
        controllable: session.mode == SessionMode::FullControl && session.annotation_tool == crate::annotations::Tool::Off,
        annotation: session.annotation_tool,
    };
    if let Some(canvas) = session.workspace.canvas.paint(ui, sources)
        && session.quality_hud
    {
        let mut stats = session.video_stats.lock().unwrap();
        if let Some(frame) = session.latest_frame.lock().unwrap().as_ref() {
            stats.record_render(frame.seq, std::time::Instant::now());
        }
        stats.paint(ui, canvas.intersect(ui.clip_rect()), session.last_rtt_ms);
    }
}

pub(crate) fn show_windows(ctx: &Context, session: &mut ViewerSession, show_cursor: bool) {
    if !matches!(session.phase, ConnectionPhase::Connected)
        || session.mode == SessionMode::FileTransfer
    {
        return;
    }
    session
        .workspace
        .synchronize(&session.monitors, &session.cmd_tx);
    for monitor in &session.monitors {
        let Some(popout) = session.workspace.popouts.get(&monitor.id).cloned() else {
            continue;
        };
        let viewport = egui::ViewportId::from_hash_of(("remote display", session.id, monitor.id));
        let monitor = monitor.clone();
        let ppp = ctx.pixels_per_point();
        let size = Vec2::new(monitor.width as f32 / ppp, monitor.height as f32 / ppp)
            .clamp(Vec2::new(320.0, 240.0), Vec2::new(1920.0, 1200.0));
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("{} · {}", session.title, monitor.name))
            .with_inner_size(size);
        let origin = ctx.clone();
        let frames = session.monitor_frames.clone();
        let cursors = show_cursor.then(|| session.cursors.clone());
        let sender = session.cmd_tx.clone();
        let annotation = session.annotation_tool;
        let controllable = session.mode == SessionMode::FullControl && annotation == crate::annotations::Tool::Off;
        ctx.show_viewport_deferred(viewport, builder, move |ui, _| {
            if ui.input(|i| i.viewport().close_requested()) {
                popout.closed.store(true, Ordering::Relaxed);
                if controllable {
                    popout.canvas.lock().unwrap().release_pointer(&sender);
                    let _ = sender.try_send(SessionCommand::Input(InputMsg::ReleaseAllKeys));
                }
                origin.request_repaint_of(egui::ViewportId::ROOT);
                return;
            }
            egui::CentralPanel::default().show(ui, |ui| {
                popout.canvas.lock().unwrap().paint(
                    ui,
                    Sources {
                        monitors: std::slice::from_ref(&monitor),
                        frames: &frames,
                        cursors: cursors.as_ref(),
                        sender: &sender,
                        controllable,
                        annotation,
                    },
                );
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        });
    }
}

#[derive(Default)]
pub(crate) struct WindowMatch {
    pub enabled: bool,
    pending: Option<((u8, u16, u16), std::time::Instant)>,
    last_requested: Option<(u8, u16, u16)>,
}

impl WindowMatch {
    pub fn disable(&mut self, sender: &mpsc::Sender<SessionCommand>) {
        self.enabled = false;
        self.pending = None;
        if self.last_requested.is_some()
            && sender
                .try_send(SessionCommand::Control(ControlMsg::SpecialAction(
                    deskbolt_proto::SpecialAction::RestoreDisplayMode,
                )))
                .is_ok()
        {
            self.last_requested = None;
        }
    }

    pub fn tick(&mut self, ui: &Ui, monitor_id: u8, sender: &mpsc::Sender<SessionCommand>) {
        use std::time::{Duration, Instant};

        if !self.enabled {
            self.disable(sender);
            return;
        }
        let pixels = ui.available_size() * ui.ctx().pixels_per_point();
        if pixels.x < 320.0 || pixels.y < 200.0 {
            return;
        }
        let width = pixels.x.round().clamp(320.0, 16_384.0) as u16;
        let height = pixels.y.round().clamp(200.0, 16_384.0) as u16;
        let target = (monitor_id, width, height);
        if self.last_requested == Some(target) {
            self.pending = None;
            return;
        }
        let started = self
            .pending
            .filter(|(size, _)| *size == target)
            .map_or_else(Instant::now, |(_, time)| time);
        self.pending = Some((target, started));
        if started.elapsed() >= Duration::from_millis(600)
            && sender
                .try_send(SessionCommand::Control(ControlMsg::SpecialAction(
                    deskbolt_proto::SpecialAction::SetDisplayMode {
                        monitor_id,
                        width,
                        height,
                        refresh_hz: 0,
                    },
                )))
                .is_ok()
        {
            self.last_requested = Some(target);
            self.pending = None;
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
    }

    pub fn refused(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: u8, left: i32, top: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo {
            id,
            left,
            top,
            width,
            height,
            name: id.to_string(),
            is_primary: id == 1,
            dpi_scale: 1.0,
        }
    }

    #[test]
    fn closing_display_releases_pointer_on_its_last_monitor() {
        let (sender, mut commands) = mpsc::channel(2);
        let mut canvas = Canvas::default();
        canvas.pressed[0] = Some(Hit {
            monitor: 5,
            x: 0.5,
            y: 0.75,
        });
        canvas.release_pointer(&sender);
        assert!(matches!(
            commands.try_recv().unwrap(),
            SessionCommand::Input(InputMsg::MonitorPointer {
                monitor_id: 5,
                event: PointerEvent::Button {
                    button: MouseBtn::Left,
                    down: false,
                    ..
                },
            })
        ));
        canvas.release_pointer(&sender);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn stitched_coordinates_use_signed_origins_and_exclude_gaps() {
        let screens = [
            screen(0, -1000, 500, 1000, 500),
            screen(1, 0, 0, 1000, 1000),
        ];
        let layout = Layout::fit(
            &screens,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0)),
        )
        .unwrap();
        assert!(layout.hit(Pos2::new(250.0, 100.0)).is_none());
        let left = layout.hit(Pos2::new(250.0, 375.0)).unwrap();
        assert_eq!((left.monitor, left.x, left.y), (0, 0.5, 0.5));
        let edge = layout.hit(Pos2::new(500.0, 375.0)).unwrap();
        assert_eq!((edge.monitor, edge.x, edge.y), (1, 0.0, 0.75));
    }
}
