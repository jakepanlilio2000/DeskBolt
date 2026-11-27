use std::collections::HashMap;
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{self, ConnectionExt as _, CreateWindowAux, ImageFormat, ImageOrder, Rectangle, WindowClass};

static WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

pub(super) fn start() -> Result<(), String> {
    let mut worker = WORKER.lock().unwrap();
    if worker.as_ref().is_some_and(|thread| !thread.is_finished()) {
        return Ok(());
    }
    let (ready, recv) = mpsc::sync_channel(1);
    let handle = thread::Builder::new().name("desktop-annotation".into()).spawn(move || {
        if let Err(error) = run(&ready) {
            let _ = ready.try_send(Err(error.to_string()));
        }
    }).map_err(|error| error.to_string())?;
    recv.recv_timeout(Duration::from_secs(1)).map_err(|_| "The X11 desktop did not respond.")??;
    *worker = Some(handle);
    Ok(())
}

fn run(ready: &mpsc::SyncSender<Result<(), String>>) -> Result<(), Box<dyn std::error::Error>> {
    let (connection, index) = x11rb::connect(None)?;
    connection.shape_query_version()?.reply()?;
    let root = &connection.setup().roots[index];
    let format = connection.setup().pixmap_formats.iter().find(|f| f.depth == root.root_depth);
    let visual = root.allowed_depths.iter().flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == root.root_visual);
    if !format.is_some_and(|format| format.bits_per_pixel == 32)
        || !visual.is_some_and(|visual| visual.red_mask == 0x00ff_0000
            && visual.green_mask == 0x0000_ff00 && visual.blue_mask == 0x0000_00ff)
    {
        return Err("This X11 screen does not offer a compatible overlay format.".into());
    }
    let ordered = connection.setup().image_byte_order == ImageOrder::LSB_FIRST;
    let empty = xproto::ClipOrdering::UNSORTED;
    let gc = connection.generate_id()?;
    connection.create_gc(gc, root.root, &xproto::CreateGCAux::new())?.check()?;
    ready.send(Ok(()))?;
    let mut last = 0;
    let mut windows = HashMap::<u8, u32>::new();
    let mut idle = 0;
    loop {
        if let Some(scene) = crate::annotations::snapshot(&mut last) {
            let displays = scene.displays();
            windows.retain(|id, window| {
                let keep = displays.iter().any(|monitor| monitor.id == *id);
                if !keep {
                    let _ = connection.destroy_window(*window);
                }
                keep
            });
            for monitor in displays {
                let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                    i16::try_from(monitor.left), i16::try_from(monitor.top),
                    u16::try_from(monitor.width), u16::try_from(monitor.height),
                ) else { continue };
                let Some(mut image) = scene.render(monitor.id, u32::from(w), u32::from(h)) else { continue };
                let window = match windows.entry(monitor.id) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let window = connection.generate_id()?;
                        connection.create_window(root.root_depth, window, root.root, x, y, w, h, 0,
                            WindowClass::INPUT_OUTPUT, root.root_visual,
                            &CreateWindowAux::new().override_redirect(1))?.check()?;
                        connection.shape_rectangles(SO::SET, SK::INPUT, empty, window, 0, 0, &[])?.check()?;
                        connection.shape_rectangles(SO::SET, SK::BOUNDING, empty, window, 0, 0, &[])?.check()?;
                        connection.map_window(window)?.check()?;
                        *entry.insert(window)
                    }
                };
                let stride = usize::from(w) * 4;
                let runs = opaque_runs(image.data(), stride);
                for pixel in image.data_mut().as_chunks_mut::<4>().0 {
                    if ordered { pixel.swap(0, 2); } else { pixel.rotate_right(1); }
                }
                let rows = ((connection.maximum_request_bytes() - 32) / stride).max(1);
                connection.shape_rectangles(SO::SET, SK::BOUNDING, empty, window, 0, 0, &[])?;
                for part in runs.chunks(2_048) {
                    connection.shape_rectangles(SO::UNION, SK::BOUNDING, empty, window, 0, 0, part)?;
                }
                for (chunk, pixels) in image.data().chunks(rows * stride).enumerate() {
                    connection.put_image(ImageFormat::Z_PIXMAP, window, gc, w, (pixels.len() / stride) as u16, 0,
                        (chunk * rows) as i16, 0, root.root_depth, pixels)?;
                }
            }
            connection.flush()?;
        }
        idle = if windows.is_empty() { idle + 1 } else { 0 };
        if idle > 200 {
            return Ok(());
        }
        while connection.poll_for_event()?.is_some() {}
        thread::sleep(Duration::from_millis(40));
    }
}

fn opaque_runs(bytes: &[u8], stride: usize) -> Vec<Rectangle> {
    let mut spans = Vec::new();
    for (y, row) in bytes.chunks_exact(stride).enumerate() {
        let mut start = None;
        for (x, alpha) in row.iter().skip(3).step_by(4).copied().chain([0]).enumerate() {
            match (start, alpha > 16) {
                (None, true) => start = Some(x),
                (Some(from), false) => {
                    spans.push(Rectangle { x: from as i16, y: y as i16, width: (x - from) as u16, height: 1 });
                    start = None;
                }
                _ => {}
            }
        }
    }
    spans
}
