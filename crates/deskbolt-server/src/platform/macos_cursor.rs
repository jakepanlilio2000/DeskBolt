use deskbolt_proto::{CursorShape, CursorSnapshot, MAX_CURSOR_SIDE, MonitorInfo};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGEvent, CGImageAlphaInfo, CGImageByteOrderInfo,
};

use super::desktop_common::unpremultiply;

pub fn sample(monitor: &MonitorInfo, frame: (u32, u32)) -> Option<CursorSnapshot> {
    if monitor.width == 0 || monitor.height == 0 || frame.0 == 0 || frame.1 == 0 {
        return None;
    }
    let event = CGEvent::new(None)?;
    let location = CGEvent::location(Some(&event));
    let x = location.x - f64::from(monitor.left);
    let y = location.y - f64::from(monitor.top);
    if x < 0.0 || y < 0.0 || x >= f64::from(monitor.width) || y >= f64::from(monitor.height) {
        return None;
    }
    let scale_x = f64::from(frame.0) / f64::from(monitor.width);
    let scale_y = f64::from(frame.1) / f64::from(monitor.height);
    autoreleasepool(|_| {
        #[allow(deprecated)]
        let cursor = NSCursor::currentSystemCursor()?;
        let ns_image = cursor.image();
        let size = ns_image.size();
        let hotspot = cursor.hotSpot();
        let width = (size.width * scale_x).round() as u16;
        let height = (size.height * scale_y).round() as u16;
        if width == 0 || height == 0 || width > MAX_CURSOR_SIDE || height > MAX_CURSOR_SIDE {
            return None;
        }
        let hotspot_x = ((hotspot.x * scale_x).round() as u16).min(width - 1);
        let hotspot_y = ((hotspot.y * scale_y).round() as u16).min(height - 1);
        // SAFETY: A null rectangle and optional drawing hints are supported by the image API.
        let image = unsafe {
            ns_image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None)
        }?;
        let color = CGColorSpace::new_device_rgb()?;
        let bytes_per_row = usize::from(width) * 4;
        let mut rgba = vec![0; bytes_per_row * usize::from(height)];
        // SAFETY: The bitmap context is dropped before the backing pixel buffer is used.
        let context = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                usize::from(width),
                usize::from(height),
                8,
                bytes_per_row,
                Some(&color),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }?;
        CGContext::draw_image(
            Some(&context),
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: f64::from(width),
                    height: f64::from(height),
                },
            },
            Some(&image),
        );
        drop(context);
        for pixel in rgba.as_chunks_mut::<4>().0 {
            unpremultiply(pixel);
        }
        Some(CursorSnapshot {
            x: (x * scale_x).round() as i16,
            y: (y * scale_y).round() as i16,
            shape: CursorShape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            },
        })
    })
}
