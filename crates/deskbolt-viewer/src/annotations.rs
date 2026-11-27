use deskbolt_proto::{AnnotationEvent, CommsMsg};
use egui::{Color32, Pos2, Rect, Stroke};
use tokio::sync::mpsc;

use crate::session::SessionCommand;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    #[default]
    Off,
    Freehand,
    Arrow,
    Rectangle,
    Laser,
}

impl Tool {
    pub(crate) const OPTIONS: [(Self, &'static str); 5] = [
        (Self::Off, "Off"),
        (Self::Freehand, "Freehand"),
        (Self::Arrow, "Arrow"),
        (Self::Rectangle, "Rectangle"),
        (Self::Laser, "Laser Pointer"),
    ];
}

#[derive(Default)]
pub(crate) struct Drawing {
    drag: Option<(u8, Pos2, Pos2)>,
    laser: Option<(u8, [u16; 2])>,
}

impl Drawing {
    pub(crate) fn paint(
        &mut self, ui: &mut egui::Ui, screens: &[(u8, Rect)], tool: Tool,
        sender: &mpsc::Sender<SessionCommand>,
    ) {
        let (position, pressed, down, released) = ui.input(|i| {
            let position = i.focused.then(|| i.pointer.interact_pos()).flatten();
            (position, i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.primary_released())
        });
        let target = position.and_then(|p| {
            if ui.clip_rect().contains(p) {
                screens.iter().copied().find(|(_, rect)| rect.contains(p))
            } else {
                None
            }
        });
        if target.is_some() || self.drag.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        if tool == Tool::Laser {
            self.drag = None;
            if let Some((display, rect)) = target.zip(position).map(|((id, rect), p)| (id, normalize(rect, p))) {
                if self.laser != Some((display, rect)) {
                    send(sender, AnnotationEvent::Laser { display, point: rect });
                    self.laser = Some((display, rect));
                }
            } else {
                self.laser = None;
            }
            return;
        }
        self.laser = None;
        if pressed && let Some(((display, _), point)) = target.zip(position) {
            self.drag = Some((display, point, point));
        }
        let Some((display, start, last)) = self.drag else { return };
        let Some((_, rect)) = screens.iter().copied().find(|(id, _)| *id == display) else {
            self.drag = None;
            return;
        };
        let now = position.unwrap_or(last).clamp(rect.min, rect.max);
        let event = match tool {
            Tool::Freehand if (down || released) && now != last => Some(AnnotationEvent::Stroke {
                display, start: normalize(rect, last), end: normalize(rect, now),
            }),
            Tool::Arrow if released && now != start => Some(AnnotationEvent::Arrow {
                display, start: normalize(rect, start), end: normalize(rect, now),
            }),
            Tool::Rectangle if released && now != start => Some(AnnotationEvent::Rectangle {
                display, start: normalize(rect, start), end: normalize(rect, now),
            }),
            _ => None,
        };
        if let Some(event) = event {
            send(sender, event);
        }
        let stroke = Stroke::new(2.0, Color32::from_rgb(255, 107, 37));
        let painter = ui.painter_at(rect);
        if down && tool == Tool::Arrow {
            painter.arrow(start, now - start, stroke);
        } else if down && tool == Tool::Rectangle {
            painter.rect_stroke(Rect::from_two_pos(start, now), 0.0, stroke, egui::StrokeKind::Middle);
        }
        self.drag = down.then_some((display, start, now));
    }
}

pub(crate) fn send(sender: &mpsc::Sender<SessionCommand>, event: AnnotationEvent) {
    let _ = sender.try_send(SessionCommand::Comms(CommsMsg::AnnotationEvent(event)));
}

fn normalize(rect: Rect, point: Pos2) -> [u16; 2] {
    let factor = f32::from(u16::MAX);
    [
        ((point.x - rect.min.x) / rect.width().max(1.0) * factor).round() as u16,
        ((point.y - rect.min.y) / rect.height().max(1.0) * factor).round() as u16,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drawing_coordinates_are_relative_to_each_display() {
        let rect = Rect::from_min_max(Pos2::new(-200.0, 50.0), Pos2::new(200.0, 150.0));
        assert_eq!(normalize(rect, Pos2::new(0.0, 100.0)), [32768, 32768]);
        assert_eq!(normalize(rect, Pos2::new(-210.0, 200.0)), [0, u16::MAX]);
    }
}
