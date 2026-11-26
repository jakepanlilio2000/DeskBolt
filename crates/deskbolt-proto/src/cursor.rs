use crate::VideoMsg;

pub const MAX_CURSOR_SIDE: u16 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}

impl CursorShape {
    pub fn decode(width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, zstd: &[u8]) -> Option<Self> {
        let len = cursor_byte_len(width, height, hotspot_x, hotspot_y)?;
        let rgba = zstd::bulk::decompress(zstd, len).ok()?;
        (rgba.len() == len).then_some(Self { width, height, hotspot_x, hotspot_y, rgba })
    }
}

fn cursor_byte_len(width: u16, height: u16, hotspot_x: u16, hotspot_y: u16) -> Option<usize> {
    (width <= MAX_CURSOR_SIDE && height <= MAX_CURSOR_SIDE && hotspot_x < width && hotspot_y < height)
        .then_some(usize::from(width) * usize::from(height) * 4)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub x: i16,
    pub y: i16,
    pub shape: CursorShape,
}

#[derive(Default)]
pub struct CursorDelta {
    previous: Option<(u8, CursorSnapshot)>,
}

impl CursorDelta {
    pub fn update(&mut self, monitor_id: u8, cursor: Option<CursorSnapshot>, refresh: bool) -> Option<VideoMsg> {
        let previous = self.previous.as_ref().filter(|(id, _)| *id == monitor_id).map(|(_, cursor)| cursor);
        if !refresh && cursor.as_ref() == previous && (cursor.is_some() || self.previous.is_none()) {
            return None;
        }
        let message = if let Some(cursor) = &cursor {
            let CursorShape { width, height, hotspot_x, hotspot_y, rgba } = &cursor.shape;
            if cursor_byte_len(*width, *height, *hotspot_x, *hotspot_y)? != rgba.len() {
                return None;
            }
            let shape_changed = refresh || previous.is_none_or(|p| p.shape != cursor.shape);
            let zstd_rgba_shape = if shape_changed {
                Some(zstd::bulk::compress(rgba, 1).ok()?)
            } else {
                None
            };
            VideoMsg::CursorUpdate {
                monitor_id, visible: true, x: cursor.x, y: cursor.y,
                hotspot_x: *hotspot_x, hotspot_y: *hotspot_y, width: *width, height: *height, zstd_rgba_shape,
            }
        } else {
            VideoMsg::CursorUpdate {
                monitor_id, visible: false, x: 0, y: 0, hotspot_x: 0, hotspot_y: 0,
                width: 0, height: 0, zstd_rgba_shape: None,
            }
        };
        self.previous = cursor.map(|cursor| (monitor_id, cursor));
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelId, FLAG_NONE, pack_message, unpack_message};

    #[test]
    fn serializes_shape_movement_refresh_and_hide_deltas() {
        let mut delta = CursorDelta::default();
        let shape = CursorShape { width: 2, height: 1, hotspot_x: 1, hotspot_y: 0, rgba: vec![0, 0, 0, 255, 255, 0, 0, 255] };
        let mut cursor = CursorSnapshot { x: 8, y: 9, shape: shape.clone() };
        let first = delta.update(2, Some(cursor.clone()), false).unwrap();
        let frame = pack_message(ChannelId::VideoTiles, FLAG_NONE, &first, None).unwrap();
        assert_eq!(unpack_message::<VideoMsg>(&frame).unwrap(), first);
        let VideoMsg::CursorUpdate { visible: true, monitor_id: 2, zstd_rgba_shape: Some(encoded), .. } = first else {
            panic!("first update omitted the cursor shape")
        };
        assert_eq!(CursorShape::decode(2, 1, 1, 0, &encoded), Some(shape));
        assert!(CursorShape::decode(257, 1, 0, 0, &encoded).is_none());
        assert!(CursorShape::decode(1, 1, 0, 0, &encoded).is_none());
        assert!(delta.update(2, Some(cursor.clone()), false).is_none());
        cursor.x = 10;
        assert!(matches!(delta.update(2, Some(cursor.clone()), false),
            Some(VideoMsg::CursorUpdate { x: 10, zstd_rgba_shape: None, .. })));
        assert!(matches!(delta.update(2, Some(cursor), true),
            Some(VideoMsg::CursorUpdate { zstd_rgba_shape: Some(_), .. })));
        assert!(matches!(delta.update(2, None, false), Some(VideoMsg::CursorUpdate { visible: false, .. })));
        assert!(delta.update(2, None, false).is_none());
    }
}
