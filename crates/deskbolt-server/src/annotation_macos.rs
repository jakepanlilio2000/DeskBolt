use std::collections::HashMap;

use objc2::{AnyThread, MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSBackingStoreType, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSImage,
    NSImageScaling, NSImageView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{NSPoint, NSRect, NSSize};

#[derive(Default)]
pub(crate) struct Overlay {
    last: u64,
    windows: HashMap<u8, Window>,
}

struct Window {
    window: Retained<NSWindow>,
    view: Retained<NSImageView>,
}

impl Drop for Window {
    fn drop(&mut self) {
        self.window.orderOut(None);
    }
}

impl Overlay {
    pub(crate) fn refresh(&mut self) {
        let Some(scene) = super::snapshot(&mut self.last) else { return };
        let Some(mtm) = MainThreadMarker::new() else { return };
        let displays = scene.displays();
        self.windows.retain(|id, _| displays.iter().any(|monitor| monitor.id == *id));
        let primary_height = CGDisplayBounds(CGMainDisplayID()).size.height;
        for monitor in displays {
            let width = f64::from(monitor.width);
            let height = f64::from(monitor.height);
            let frame = NSRect::new(
                NSPoint::new(f64::from(monitor.left), primary_height - f64::from(monitor.top) - height),
                NSSize::new(width, height),
            );
            let Some(bitmap) = scene.render(monitor.id, monitor.width, monitor.height) else { continue };
            let pixels = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(), std::ptr::null_mut(), monitor.width as isize, monitor.height as isize,
                    8, 4, true, false, NSDeviceRGBColorSpace, monitor.width as isize * 4, 32,
                )
            };
            let Some(pixels) = pixels else { continue };
            let data = pixels.bitmapData();
            if data.is_null() { continue; }
            unsafe { std::ptr::copy_nonoverlapping(bitmap.data().as_ptr(), data, bitmap.data().len()); }
            let image = NSImage::initWithSize(NSImage::alloc(), frame.size);
            image.addRepresentation(&pixels);
            let entry = self.windows.entry(monitor.id).or_insert_with(|| Window::new(frame, mtm));
            entry.window.setFrame_display(frame, false);
            entry.view.setImage(Some(&image));
            entry.window.orderFrontRegardless();
        }
    }
}

impl Window {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Self {
        let window = unsafe {
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(), frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false,
            );
            window.setReleasedWhenClosed(false);
            window
        };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setIgnoresMouseEvents(true);
        window.setLevel(25);
        window.setCollectionBehavior(NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary);
        let view = NSImageView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
        window.setContentView(Some(&view));
        Self { window, view }
    }
}
