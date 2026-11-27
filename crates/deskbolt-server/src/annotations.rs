use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use deskbolt_proto::{AnnotationEvent, MonitorInfo};
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

const LASER_LIFETIME: Duration = Duration::from_millis(900);
const MARK_LIFETIME: Duration = Duration::from_secs(30);
const MAX_MARKS: usize = 256;
static SCENE: Mutex<Scene> = Mutex::new(Scene { marks: VecDeque::new(), revision: 0 });

#[derive(Clone)]
struct Mark {
    monitor: MonitorInfo,
    event: AnnotationEvent,
    time: Instant,
}

impl Mark {
    fn is_laser(&self) -> bool {
        matches!(self.event, AnnotationEvent::Laser { .. })
    }

    fn active(&self, now: Instant) -> bool {
        now.duration_since(self.time) < if self.is_laser() { LASER_LIFETIME } else { MARK_LIFETIME }
    }
}

struct Scene {
    marks: VecDeque<Mark>,
    revision: u64,
}

pub(crate) struct Snapshot {
    marks: Vec<Mark>,
    now: Instant,
}

pub(crate) fn submit(event: AnnotationEvent, monitors: &[MonitorInfo]) -> Result<(), String> {
    if event == AnnotationEvent::Clear {
        clear();
        return Ok(());
    }
    let Some(monitor) = monitors.iter().find(|m| Some(m.id) == event.display()) else {
        return Err("The annotated display is no longer available.".into());
    };
    #[cfg(target_os = "windows")]
    windows::start()?;
    #[cfg(target_os = "linux")]
    crate::platform::start_annotation_overlay()?;
    #[cfg(target_os = "macos")]
    if !crate::platform::macos::interactive_session() {
        return Err("The local desktop is unavailable.".into());
    }

    let mut scene = SCENE.lock().unwrap();
    if scene.marks.len() == MAX_MARKS {
        scene.marks.pop_front();
    }
    scene.marks.push_back(Mark { monitor: monitor.clone(), event, time: Instant::now() });
    scene.revision += 1;
    Ok(())
}

pub(crate) fn clear() {
    let mut scene = SCENE.lock().unwrap();
    scene.marks.clear();
    scene.revision += 1;
}

pub(crate) fn snapshot(last: &mut u64) -> Option<Snapshot> {
    let now = Instant::now();
    let mut scene = SCENE.lock().unwrap();
    let before = scene.marks.len();
    scene.marks.retain(|mark| mark.active(now));
    if scene.marks.len() != before {
        scene.revision += 1;
    }
    if *last == scene.revision && !scene.marks.iter().any(Mark::is_laser) {
        return None;
    }
    *last = scene.revision;
    Some(Snapshot { marks: scene.marks.iter().cloned().collect(), now })
}

impl Snapshot {
    pub(crate) fn displays(&self) -> Vec<&MonitorInfo> {
        let mut monitors = Vec::<&MonitorInfo>::new();
        for mark in &self.marks {
            if !monitors.iter().any(|m| m.id == mark.monitor.id) {
                monitors.push(&mark.monitor);
            }
        }
        monitors
    }

    pub(crate) fn render(&self, display: u8, width: u32, height: u32) -> Option<Pixmap> {
        if u64::from(width) * u64::from(height) > 16_777_216
            || !self.marks.iter().any(|mark| mark.monitor.id == display)
        {
            return None;
        }
        let mut image = Pixmap::new(width, height)?;
        let point = |xy: [u16; 2]| (
            f32::from(xy[0]) * (width.saturating_sub(1) as f32 / f32::from(u16::MAX)),
            f32::from(xy[1]) * (height.saturating_sub(1) as f32 / f32::from(u16::MAX)),
        );
        let stroke = Stroke { width: 4.0, line_cap: LineCap::Round, ..Stroke::default() };

        for mark in self.marks.iter().filter(|mark| mark.monitor.id == display) {
            let mut paint = Paint::default();
            paint.set_color_rgba8(255, 107, 37, 255);
            let mut path = PathBuilder::new();
            match mark.event {
                AnnotationEvent::Stroke { start, end, .. } | AnnotationEvent::Arrow { start, end, .. } => {
                    let (x, y) = point(start);
                    let (ex, ey) = point(end);
                    path.move_to(x, y);
                    path.line_to(ex, ey);
                    if matches!(mark.event, AnnotationEvent::Arrow { .. }) {
                        let angle = (ey - y).atan2(ex - x);
                        for angle in [angle - 0.55, angle + 0.55] {
                            path.move_to(ex, ey);
                            path.line_to(ex - angle.cos() * 18.0, ey - angle.sin() * 18.0);
                        }
                    }
                }
                AnnotationEvent::Rectangle { start, end, .. } => {
                    let (x, y) = point(start);
                    let (ex, ey) = point(end);
                    if let Some(rect) = Rect::from_ltrb(x.min(ex), y.min(ey), x.max(ex), y.max(ey)) {
                        path.push_rect(rect);
                    }
                }
                AnnotationEvent::Laser { point: pos, .. } => {
                    let elapsed = self.now.duration_since(mark.time).as_secs_f32();
                    let strength = (1.0 - elapsed / LASER_LIFETIME.as_secs_f32()).clamp(0.0, 1.0);
                    let (x, y) = point(pos);
                    for (radius, alpha) in [(11.0, 75.0), (6.0, 220.0)] {
                        let mut dot = PathBuilder::new();
                        dot.push_circle(x, y, radius);
                        paint.set_color_rgba8(255, 41, 67, (strength * alpha) as u8);
                        if let Some(dot) = dot.finish() {
                            image.fill_path(&dot, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
                        }
                    }
                    continue;
                }
                AnnotationEvent::Clear => continue,
            }
            if let Some(path) = path.finish() {
                image.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        Some(image)
    }
}

#[cfg(target_os = "windows")]
#[path = "annotation_windows.rs"]
mod windows;

#[cfg(target_os = "macos")]
#[path = "annotation_macos.rs"]
pub(crate) mod macos;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_render_without_opaque_background_and_laser_fades() {
        let now = Instant::now();
        let mark = Mark {
            monitor: MonitorInfo { id: 7, name: String::new(), is_primary: true, left: -32, top: 0,
                width: 64, height: 64, dpi_scale: 1.0 },
            event: AnnotationEvent::Laser { display: 7, point: [32768, 32768] },
            time: now,
        };
        let opacity = |now| {
            let frame = Snapshot { marks: vec![mark.clone()], now };
            let image = frame.render(7, 64, 64).unwrap();
            assert_eq!(image.data()[3], 0);
            assert!(frame.render(8, 64, 64).is_none());
            image.data()[(31 * 64 + 31) * 4 + 3]
        };
        assert!(opacity(now) > opacity(now + Duration::from_millis(450)));
        assert!(!mark.active(now + LASER_LIFETIME));
    }
}
