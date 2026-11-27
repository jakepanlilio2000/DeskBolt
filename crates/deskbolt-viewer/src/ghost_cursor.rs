use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deskbolt_proto::{CommsMsg, GhostCursor, ViewerPresence};
use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Ui, vec2};
use tokio::sync::mpsc;

use crate::session::SessionCommand;

const LIFETIME: Duration = Duration::from_millis(1_500);
pub(crate) type SharedGhosts = Arc<Mutex<HashMap<u64, (GhostCursor, Instant)>>>;

pub(crate) fn received(samples: &SharedGhosts, viewer: u64, cursor: GhostCursor) {
    let now = Instant::now();
    let mut samples = samples.lock().unwrap();
    samples.retain(|_, (_, time)| now.duration_since(*time) < LIFETIME);
    if samples.len() < 64 || samples.contains_key(&viewer) {
        samples.insert(viewer, (cursor, now));
    }
}

#[derive(Default)]
pub(crate) struct Emitter(Option<(GhostCursor, Instant)>);

impl Emitter {
    pub(crate) fn paint(
        &mut self, ui: &Ui, screens: &[(u8, Rect)], sender: &mpsc::Sender<SessionCommand>,
        samples: &SharedGhosts, viewers: &[ViewerPresence],
    ) {
        if let Some(point) = ui.input(|i| i.pointer.hover_pos().filter(|_| i.focused))
            && ui.clip_rect().contains(point)
            && let Some(&(display, rect)) = screens.iter().find(|(_, rect)| rect.contains(point))
        {
            let cursor = GhostCursor { display, point: crate::annotations::normalize(rect, point) };
            let now = Instant::now();
            let due = self.0.is_none_or(|(last, sent)| {
                let interval = if last == cursor { 500 } else { 100 };
                now.duration_since(sent) >= Duration::from_millis(interval)
            });
            if due && sender.try_send(SessionCommand::Comms(CommsMsg::GhostCursor { viewer: 0, cursor })).is_ok() {
                self.0 = Some((cursor, now));
            }
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        let now = Instant::now();
        let samples = samples.lock().unwrap();
        for (viewer, (cursor, time)) in samples.iter() {
            if now.duration_since(*time) >= LIFETIME {
                continue;
            }
            let Some(person) = viewers.iter().find(|person| person.id == *viewer) else { continue };
            let Some((_, rect)) = screens.iter().find(|(id, _)| *id == cursor.display) else { continue };
            let norm = vec2(f32::from(cursor.point[0]), f32::from(cursor.point[1])) / f32::from(u16::MAX);
            let point: Pos2 = rect.min + norm * rect.size();
            let painter = ui.painter_at(*rect);
            let color = [Color32::LIGHT_BLUE, Color32::LIGHT_GREEN, Color32::LIGHT_YELLOW, Color32::LIGHT_RED]
                [(*viewer % 4) as usize];
            painter.add(Shape::convex_polygon(vec![point, point + vec2(2.0, 13.0), point + vec2(10.0, 9.0)],
                color, Stroke::new(1.0, Color32::BLACK)));
            let galley = painter.layout_no_wrap(person.name.clone(), FontId::proportional(12.0), color);
            let label = Align2::LEFT_TOP.anchor_size(point + vec2(10.0, 10.0), galley.size());
            painter.rect_filled(label.expand(2.0), 2.0, Color32::from_black_alpha(185));
            painter.galley(label.min, galley, color);
            ui.ctx().request_repaint_after(LIFETIME.saturating_sub(now.duration_since(*time)));
        }
    }
}
