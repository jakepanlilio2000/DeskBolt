use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, Ui, Vec2};

pub struct VideoQualityStats {
    samples: VecDeque<u32>,
    source_seq: Option<u64>,
    received: VecDeque<(Instant, usize)>,
    rendered: VecDeque<Instant>,
    last_render_id: u64,
    codec: &'static str,
    decode_ms: u16,
}

impl Default for VideoQualityStats {
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(60),
            source_seq: None,
            received: VecDeque::new(),
            rendered: VecDeque::new(),
            last_render_id: 0,
            codec: "—",
            decode_ms: 0,
        }
    }
}

impl VideoQualityStats {
    pub fn decoded(&mut self, sequence: u64, codec: &'static str, decode_ms: u16) {
        if self.samples.len() == 60 {
            self.samples.pop_front();
        }
        let gap = self.source_seq.map_or(0, |previous| {
            if sequence > previous {
                (sequence - previous - 1).min(u64::from(u32::MAX)) as u32
            } else {
                self.samples.clear();
                0
            }
        });
        self.samples.push_back(gap);
        self.source_seq = Some(sequence);
        self.codec = codec;
        self.decode_ms = decode_ms;
    }

    pub fn record_bytes(&mut self, bytes: usize, now: Instant) {
        // Group reads into 50 ms buckets so high packet rates use bounded storage.
        match self.received.back_mut() {
            Some((time, counter)) if now.duration_since(*time) < Duration::from_millis(50) => {
                *counter = counter.saturating_add(bytes);
            }
            _ => self.received.push_back((now, bytes)),
        }
        self.expire(now);
    }

    pub fn record_render(&mut self, id: u64, now: Instant) {
        if id != self.last_render_id {
            self.last_render_id = id;
            self.rendered.push_back(now);
            self.expire(now);
        }
    }

    fn expire(&mut self, now: Instant) {
        let window = Duration::from_secs(1);
        while self
            .received
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) >= window)
        {
            self.received.pop_front();
        }
        while self
            .rendered
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            self.rendered.pop_front();
        }
    }

    pub fn paint(&mut self, ui: &Ui, canvas: Rect, rtt: Option<u64>) {
        if canvas.width() < 218.0 || canvas.height() < 105.0 {
            return;
        }
        self.expire(Instant::now());
        let bitrate = self
            .received
            .iter()
            .map(|(_, bytes)| *bytes as f64)
            .sum::<f64>()
            * 8.0
            / 1_000_000.0;
        let origin = canvas.min + Vec2::splat(8.0);
        let panel = Rect::from_min_size(origin, Vec2::new(202.0, 95.0));
        let painter = ui.painter_at(canvas);
        painter.rect_filled(panel, 6.0, Color32::from_black_alpha(185));
        let rtt = rtt.map_or_else(|| "—".to_owned(), |ms| format!("{ms} ms"));
        let status = format!(
            "{}\nRTT {rtt}  ·  {} FPS\n{bitrate:.2} Mbps  ·  Decode {} ms",
            self.codec,
            self.rendered.len(),
            self.decode_ms
        );
        painter.text(
            origin + Vec2::new(8.0, 6.0),
            Align2::LEFT_TOP,
            status,
            FontId::monospace(11.0),
            Color32::WHITE,
        );
        painter.text(
            origin + Vec2::new(8.0, 54.0),
            Align2::LEFT_TOP,
            "Frame drops",
            FontId::monospace(10.0),
            Color32::LIGHT_GRAY,
        );
        let high = self.samples.iter().copied().max().unwrap_or(1).max(1) as f32;
        let graph = panel.shrink2(Vec2::new(8.0, 5.0));
        let dots: Vec<_> = self
            .samples
            .iter()
            .enumerate()
            .map(|(index, drops)| {
                let x =
                    graph.right() - (self.samples.len() - index - 1) as f32 * graph.width() / 59.0;
                Pos2::new(x, graph.bottom() - *drops as f32 * 20.0 / high)
            })
            .collect();
        if dots.len() >= 2 {
            painter.add(egui::Shape::line(
                dots,
                Stroke::new(1.2, Color32::from_rgb(122, 218, 239)),
            ));
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_window_counts_gaps_and_expires_rates() {
        let mut stats = VideoQualityStats::default();
        let start = Instant::now();
        stats.record_bytes(125_000, start);
        stats.record_bytes(125_000, start + Duration::from_millis(25));
        stats.record_render(1, start + Duration::from_millis(30));
        stats.record_render(1, start + Duration::from_millis(40));
        assert_eq!(stats.received.front().unwrap().1, 250_000);
        assert_eq!(stats.rendered.len(), 1);
        stats.expire(start + Duration::from_secs(2));
        assert!(stats.received.is_empty());
        assert!(stats.rendered.is_empty());
        for frame in 0..65 {
            stats.decoded(frame * 2 + 1, "H.264 SW", 8);
        }
        assert_eq!(stats.samples.len(), 60);
        assert!(stats.samples.iter().all(|&gap| gap == 1));
        stats.decoded(1, "Zstd 4:4:4", 3);
        assert_eq!(stats.samples.as_slices().0, &[0]);
    }
}
