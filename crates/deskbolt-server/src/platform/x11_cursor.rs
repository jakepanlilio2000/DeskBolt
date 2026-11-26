use deskbolt_proto::{CursorShape, CursorSnapshot, MAX_CURSOR_SIDE, MonitorInfo};
use x11rb::protocol::xfixes::ConnectionExt;
use x11rb::rust_connection::RustConnection;

use super::desktop_common::unpremultiply;

pub struct X11Cursor {
    connection: RustConnection,
    shape: Option<(u32, CursorShape)>,
}

impl X11Cursor {
    pub fn open() -> Option<Self> {
        if super::wayland_capture::is_wayland() {
            return None;
        }
        let (connection, _) = RustConnection::connect(None).ok()?;
        connection.xfixes_query_version(2, 0).ok()?.reply().ok()?;
        Some(Self {
            connection,
            shape: None,
        })
    }

    pub fn sample(&mut self, monitor: &MonitorInfo) -> Option<CursorSnapshot> {
        let cursor = self
            .connection
            .xfixes_get_cursor_image()
            .ok()?
            .reply()
            .ok()?;
        let x = i32::from(cursor.x).checked_sub(monitor.left)?;
        let y = i32::from(cursor.y).checked_sub(monitor.top)?;
        if x < 0 || y < 0 || x as u32 >= monitor.width || y as u32 >= monitor.height {
            return None;
        }
        if self
            .shape
            .as_ref()
            .is_none_or(|(serial, _)| *serial != cursor.cursor_serial)
        {
            let (width, height) = (cursor.width, cursor.height);
            if width > MAX_CURSOR_SIDE
                || height > MAX_CURSOR_SIDE
                || cursor.xhot >= width
                || cursor.yhot >= height
                || cursor.cursor_image.len() != usize::from(width) * usize::from(height)
            {
                return None;
            }
            let rgba = cursor
                .cursor_image
                .into_iter()
                .flat_map(|argb| {
                    let [a, r, g, b] = argb.to_be_bytes();
                    let mut pixel = [r, g, b, a];
                    unpremultiply(&mut pixel);
                    pixel
                })
                .collect();
            self.shape = Some((
                cursor.cursor_serial,
                CursorShape {
                    width,
                    height,
                    hotspot_x: cursor.xhot,
                    hotspot_y: cursor.yhot,
                    rgba,
                },
            ));
        }
        Some(CursorSnapshot {
            x: x.try_into().ok()?,
            y: y.try_into().ok()?,
            shape: self.shape.as_ref()?.1.clone(),
        })
    }
}
