use deskbolt_proto::{CURSOR_INVERT_PIXEL, CursorShape, VideoMsg};
use egui::{Color32, ColorImage, Pos2, Rect, TextureHandle, TextureOptions, Ui, Vec2};

#[derive(Default)]
pub struct CursorOverlay {
    screen_position: Option<(u8, i16, i16)>,
    shape: Option<CursorShape>,
    texture: Option<TextureHandle>,
    contains_invert: bool,
    composite_position: Option<(u64, i32, i32)>,
}

impl CursorOverlay {
    pub fn update(&mut self, update: VideoMsg) {
        let VideoMsg::CursorUpdate {
            monitor_id, visible, x, y, hotspot_x, hotspot_y, width, height, zstd_rgba_shape,
        } = update else { return };
        if let Some(compressed) = zstd_rgba_shape {
            self.shape = CursorShape::decode(width, height, hotspot_x, hotspot_y, &compressed);
            self.texture = None;
            self.composite_position = None;
            self.contains_invert = self.shape.as_ref().is_some_and(|shape| {
                shape.rgba.as_chunks::<4>().0.contains(&CURSOR_INVERT_PIXEL)
            });
        }
        let correct_shape = self.shape.as_ref().is_some_and(|shape| {
            (shape.width, shape.height, shape.hotspot_x, shape.hotspot_y) == (width, height, hotspot_x, hotspot_y)
        });
        self.screen_position = (visible && correct_shape).then_some((monitor_id, x, y));
    }

    pub fn paint(&mut self, ui: &Ui, display: Rect, monitor_id: u8, frame: &crate::session::DecodedFrame, local_pointer: Option<Pos2>) -> bool {
        let (width, height) = (frame.width, frame.height);
        let Some((id, x, y)) = self.screen_position else { return false };
        let Some(shape) = self.shape.as_ref() else { return false };
        if id != monitor_id || width == 0 || height == 0 {
            return false;
        }
        let scale = display.size() / Vec2::new(width as f32, height as f32);
        let point = local_pointer.unwrap_or_else(|| display.min + Vec2::new(f32::from(x), f32::from(y)) * scale);
        let top_left = point - Vec2::new(f32::from(shape.hotspot_x), f32::from(shape.hotspot_y)) * scale;
        let source = (top_left - display.min) / scale;
        let pos = (source.x.round() as i32, source.y.round() as i32);
        let stamp = (frame.seq, pos.0, pos.1);
        if self.texture.is_none() || (self.contains_invert && self.composite_position != Some(stamp)) {
            let pixels = self.contains_invert.then(|| composite_invert(shape, frame, pos));
            let image = ColorImage::from_rgba_unmultiplied(
                [usize::from(shape.width), usize::from(shape.height)], pixels.as_deref().unwrap_or(&shape.rgba),
            );
            if let Some(texture) = self.texture.as_mut() {
                texture.set(image, TextureOptions::NEAREST);
            } else {
                self.texture = Some(ui.ctx().load_texture("session cursor", image, TextureOptions::NEAREST));
            }
            self.composite_position = Some(stamp);
        }
        let rect = Rect::from_min_size(top_left, Vec2::new(f32::from(shape.width), f32::from(shape.height)) * scale);
        ui.painter_at(display).image(self.texture.as_ref().unwrap().id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        true
    }
}

fn composite_invert(shape: &CursorShape, frame: &crate::session::DecodedFrame, pos: (i32, i32)) -> Vec<u8> {
    let mut rgba = shape.rgba.clone();
    let width = usize::from(shape.width);
    for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        if *pixel != CURSOR_INVERT_PIXEL { continue; }
        let x = pos.0 + (index % width) as i32;
        let y = pos.1 + (index / width) as i32;
        if x < 0 || y < 0 || x as u32 >= frame.width || y as u32 >= frame.height {
            *pixel = [0; 4];
            continue;
        }
        let start = (y as usize * frame.width as usize + x as usize) * 4;
        *pixel = frame.rgba.get(start..start + 4).map_or([0; 4], |background| {
            [255 - background[0], 255 - background[1], 255 - background[2], 255]
        });
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inversion_uses_the_underlying_frame_and_clips_at_the_edge() {
        let shape = CursorShape { width: 2, height: 1, hotspot_x: 0, hotspot_y: 0, rgba: CURSOR_INVERT_PIXEL.repeat(2) };
        let frame = crate::session::DecodedFrame { width: 1, height: 1, seq: 1, rgba: vec![10, 20, 30, 255] };
        assert_eq!(composite_invert(&shape, &frame, (-1, 0)), [0, 0, 0, 0, 245, 235, 225, 255]);
    }
}
