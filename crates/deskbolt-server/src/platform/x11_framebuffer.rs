use deskbolt_proto::{CursorSnapshot, MonitorInfo};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use super::{CaptureOutput, RawCapturedFrame, ScreenCapturer, diff_bgra_dirty_rects};

pub(crate) struct X11Framebuffer {
    connection: RustConnection,
    root: u32,
    monitor: MonitorInfo,
    previous: Vec<u8>,
    cursor: Option<super::x11_cursor::X11Cursor>,
}

impl X11Framebuffer {
    pub(crate) fn open() -> Result<Self, String> {
        let (connection, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let layout = connection.setup();
        let screen = &layout.roots[screen];
        let root = screen.root;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.visual_id == screen.root_visual);
        // shortcut: accept packed BGRA roots used by common headless X servers, add other visuals when needed.
        if layout.image_byte_order != ImageOrder::LSB_FIRST
            || !layout
                .pixmap_formats
                .iter()
                .any(|format| format.depth == screen.root_depth && format.bits_per_pixel == 32)
            || !visual.is_some_and(|v| {
                (v.red_mask, v.green_mask, v.blue_mask) == (0xff0000, 0xff00, 0xff)
            })
        {
            return Err("The graphical session has an unsupported framebuffer layout.".into());
        }
        let (width, height) = size(&connection, root)?;
        let monitor = MonitorInfo {
            id: 0,
            name: "Virtual Display".into(),
            is_primary: true,
            left: 0,
            top: 0,
            width: width.into(),
            height: height.into(),
            dpi_scale: 1.0,
        };
        Ok(Self {
            connection,
            root,
            monitor,
            previous: Vec::new(),
            cursor: super::x11_cursor::X11Cursor::open(),
        })
    }

    fn sample(&mut self, force_full: bool) -> Result<CaptureOutput, String> {
        let (width, height) = size(&self.connection, self.root)?;
        let image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                width,
                height,
                u32::MAX,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let mut bgra = image.data;
        if bgra.len() != usize::from(width) * usize::from(height) * 4 {
            return Err("The virtual display returned an incomplete frame.".into());
        }
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        let (width, height) = (u32::from(width), u32::from(height));
        let full = force_full
            || (width, height) != (self.monitor.width, self.monitor.height)
            || self.previous.len() != bgra.len();
        let dirty_rects = if full {
            Vec::new()
        } else {
            diff_bgra_dirty_rects(&self.previous, &bgra, width, height, 64)
        };
        if !full && dirty_rects.is_empty() {
            return Ok(CaptureOutput::Timeout);
        }
        self.previous.clone_from(&bgra);
        (self.monitor.width, self.monitor.height) = (width, height);
        Ok(CaptureOutput::Frame(RawCapturedFrame {
            monitor_id: 0,
            width,
            height,
            bgra,
            move_rects: Vec::new(),
            dirty_rects,
            force_full: full,
        }))
    }
}

fn size(connection: &RustConnection, root: u32) -> Result<(u16, u16), String> {
    let geometry = connection
        .get_geometry(root)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    if !(1..=8192).contains(&geometry.width) || !(1..=8192).contains(&geometry.height) {
        return Err("The virtual display dimensions are unsupported.".into());
    }
    Ok((geometry.width, geometry.height))
}

impl ScreenCapturer for X11Framebuffer {
    fn monitors(&self) -> Vec<MonitorInfo> {
        vec![self.monitor.clone()]
    }
    fn set_monitor(&mut self, _: u8) {}
    fn capture_next(&mut self, force_full: bool, _: u32) -> CaptureOutput {
        self.sample(force_full)
            .unwrap_or_else(CaptureOutput::Reacquire)
    }
    fn capture_cursor(&mut self) -> Option<CursorSnapshot> {
        self.cursor.as_mut()?.sample(&self.monitor)
    }
}
