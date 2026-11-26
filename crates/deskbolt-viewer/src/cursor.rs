use deskbolt_proto::{CursorShape, VideoMsg};
use egui::{Color32, ColorImage, Pos2, Rect, TextureHandle, TextureOptions, Ui, Vec2};

#[derive(Default)]
pub struct CursorOverlay {
    screen_position: Option<(u8, i16, i16)>,
    shape: Option<CursorShape>,
    texture: Option<TextureHandle>,
}

impl CursorOverlay {
    pub fn update(&mut self, update: VideoMsg) {
        let VideoMsg::CursorUpdate {
            monitor_id, visible, x, y, hotspot_x, hotspot_y, width, height, zstd_rgba_shape,
        } = update else { return };
        if let Some(compressed) = zstd_rgba_shape {
            self.shape = CursorShape::decode(width, height, hotspot_x, hotspot_y, &compressed);
            self.texture = None;
        }
        let correct_shape = self.shape.as_ref().is_some_and(|shape| {
            (shape.width, shape.height, shape.hotspot_x, shape.hotspot_y) == (width, height, hotspot_x, hotspot_y)
        });
        self.screen_position = (visible && correct_shape).then_some((monitor_id, x, y));
    }

    pub fn paint(&mut self, ui: &Ui, display: Rect, screen: (u8, u32, u32), local_pointer: Option<Pos2>) -> bool {
        let (monitor_id, width, height) = screen;
        let Some((id, x, y)) = self.screen_position else { return false };
        let Some(shape) = self.shape.as_ref() else { return false };
        if id != monitor_id || width == 0 || height == 0 {
            return false;
        }
        let texture = self.texture.get_or_insert_with(|| {
            let image = ColorImage::from_rgba_unmultiplied(
                [usize::from(shape.width), usize::from(shape.height)], &shape.rgba,
            );
            ui.ctx().load_texture("session cursor", image, TextureOptions::NEAREST)
        });
        let scale = display.size() / Vec2::new(width as f32, height as f32);
        let point = local_pointer.unwrap_or_else(|| display.min + Vec2::new(f32::from(x), f32::from(y)) * scale);
        let top_left = point - Vec2::new(f32::from(shape.hotspot_x), f32::from(shape.hotspot_y)) * scale;
        let rect = Rect::from_min_size(top_left, Vec2::new(f32::from(shape.width), f32::from(shape.height)) * scale);
        ui.painter_at(display).image(texture.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        true
    }
}
