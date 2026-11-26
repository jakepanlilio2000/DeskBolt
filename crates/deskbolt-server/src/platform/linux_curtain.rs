use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

enum Curtain {
    X11(Box<x11rb::rust_connection::RustConnection>),
    Wayland {
        stop: mpsc::Sender<()>,
        worker: std::thread::JoinHandle<()>,
    },
}

static CURTAIN: Mutex<Option<Curtain>> = Mutex::new(None);

pub(super) fn set(enabled: bool) -> Result<(), String> {
    let mut current = CURTAIN.lock().unwrap();
    if !enabled {
        match current.take() {
            Some(Curtain::X11(connection)) => drop(connection),
            Some(Curtain::Wayland { stop, worker }) => {
                let _ = stop.send(());
                let _ = worker.join();
            }
            None => {}
        }
        return Ok(());
    }
    match current.as_ref() {
        Some(Curtain::Wayland { worker, .. }) if worker.is_finished() => {}
        Some(_) => return Ok(()),
        None => {}
    }
    *current = Some(if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        open_wayland()?
    } else {
        Curtain::X11(Box::new(open_x11().map_err(|error| error.to_string())?))
    });
    Ok(())
}

fn open_x11() -> Result<x11rb::rust_connection::RustConnection, Box<dyn std::error::Error>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::shape::SK;
    use x11rb::protocol::xfixes::ConnectionExt as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};

    let (connection, _) = x11rb::connect(None)?;
    connection.xfixes_query_version(2, 0)?.reply()?;
    let input = connection.generate_id()?;
    connection.xfixes_create_region(input, &[])?.check()?;
    for screen in &connection.setup().roots {
        let geometry = connection.get_geometry(screen.root)?.reply()?;
        let window = connection.generate_id()?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                0,
                0,
                geometry.width,
                geometry.height,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .background_pixel(screen.black_pixel),
            )?
            .check()?;
        connection
            .xfixes_set_window_shape_region(window, SK::INPUT, 0, 0, input)?
            .check()?;
        connection.map_window(window)?.check()?;
    }
    connection.xfixes_destroy_region(input)?.check()?;
    connection.flush()?;
    Ok(connection)
}

fn open_wayland() -> Result<Curtain, String> {
    let (ready, reported) = mpsc::sync_channel(1);
    let (stop, stopping) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        if let Err(error) = run_wayland(&ready, &stopping) {
            let _ = ready.try_send(Err(error.to_string()));
        }
    });
    reported
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "The display did not respond.")??;
    Ok(Curtain::Wayland { stop, worker })
}

fn run_wayland(
    ready: &mpsc::SyncSender<Result<(), String>>,
    stop: &mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<DisplayState>(&connection)?;
    let qh = queue.handle();
    let mut state = DisplayState {
        compositor: globals.bind(&qh, 1..=4, ())?,
        layer_shell: globals
            .bind(&qh, 1..=4, ())
            .map_err(|_| "This desktop does not offer screen blanking.")?,
        shm: globals.bind(&qh, 1..=1, ())?,
        outputs: HashMap::new(),
        failure: None,
    };
    for output in globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
    {
        state.add_output(globals.registry(), &qh, output.name, output.version);
    }
    queue.roundtrip(&mut state)?;
    let mut announced = false;
    loop {
        if !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return Ok(());
        }
        if let Some(failure) = state.failure.take() {
            return Err(failure.into());
        }
        if state.outputs.is_empty() {
            return Err("No active screen can be blanked.".into());
        }
        queue.flush()?;
        if !announced && state.outputs.values().all(|output| output.buffer.is_some()) {
            ready.send(Ok(()))?;
            announced = true;
        }
        if let Some(read) = queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: read.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll borrows one stack-allocated descriptor during this call.
            if unsafe { libc::poll(&mut fd, 1, 100) } > 0 {
                read.read()?;
            }
        }
        queue.dispatch_pending(&mut state)?;
    }
}

struct Output {
    surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    buffer: Option<wl_buffer::WlBuffer>,
    output: wl_output::WlOutput,
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.layer.destroy();
        self.surface.destroy();
        use wayland_client::Proxy as _;
        if self.output.version() >= 3 {
            self.output.release();
        }
    }
}

struct DisplayState {
    compositor: wl_compositor::WlCompositor,
    layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    shm: wl_shm::WlShm,
    outputs: HashMap<u32, Output>,
    failure: Option<String>,
}

impl DisplayState {
    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        qh: &QueueHandle<Self>,
        name: u32,
        version: u32,
    ) {
        use zwlr_layer_shell_v1::Layer;
        use zwlr_layer_surface_v1::Anchor;
        let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
        let surface = self.compositor.create_surface(qh, ());
        let transparent_input = self.compositor.create_region(qh, ());
        surface.set_input_region(Some(&transparent_input));
        transparent_input.destroy();
        let layer = self.layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            Layer::Overlay,
            "deskbolt-curtain".into(),
            qh,
            name,
        );
        layer.set_anchor(Anchor::Top | Anchor::Right | Anchor::Bottom | Anchor::Left);
        layer.set_exclusive_zone(-1);
        layer.set_size(0, 0);
        surface.commit();
        self.outputs.insert(
            name,
            Output {
                surface,
                layer,
                buffer: None,
                output,
            },
        );
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for DisplayState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                state.add_output(registry, qh, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for DisplayState {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                let Some(output) = state.outputs.get_mut(name) else {
                    return;
                };
                let Some((stride, size)) = buffer_layout(width, height) else {
                    state.failure = Some("The desktop supplied an unsupported screen size.".into());
                    return;
                };
                // SAFETY: memfd_create returns a newly owned anonymous file descriptor.
                let fd =
                    unsafe { libc::memfd_create(c"deskbolt-curtain".as_ptr(), libc::MFD_CLOEXEC) };
                if fd < 0 {
                    state.failure = Some(std::io::Error::last_os_error().to_string());
                    return;
                }
                // SAFETY: The successful descriptor is owned by this file.
                let file = unsafe { File::from_raw_fd(fd) };
                if let Err(error) = file.set_len(size as u64) {
                    state.failure = Some(error.to_string());
                    return;
                }
                let pool = state.shm.create_pool(file.as_fd(), size, qh, ());
                let buffer = pool.create_buffer(
                    0,
                    width as i32,
                    height as i32,
                    stride,
                    wl_shm::Format::Xrgb8888,
                    qh,
                    (),
                );
                pool.destroy();
                layer.ack_configure(serial);
                output.surface.attach(Some(&buffer), 0, 0);
                output.surface.damage(0, 0, width as i32, height as i32);
                output.surface.commit();
                if let Some(old) = output.buffer.replace(buffer) {
                    old.destroy();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.outputs.remove(name);
            }
            _ => {}
        }
    }
}

fn buffer_layout(width: u32, height: u32) -> Option<(i32, i32)> {
    let stride = width.checked_mul(4)?;
    let size = stride.checked_mul(height)?;
    (size > 0 && size <= 256 * 1024 * 1024).then_some((stride as i32, size as i32))
}

delegate_noop!(DisplayState: ignore wl_compositor::WlCompositor);
delegate_noop!(DisplayState: ignore wl_region::WlRegion);
delegate_noop!(DisplayState: ignore wl_surface::WlSurface);
delegate_noop!(DisplayState: ignore wl_output::WlOutput);
delegate_noop!(DisplayState: ignore wl_shm::WlShm);
delegate_noop!(DisplayState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(DisplayState: ignore wl_buffer::WlBuffer);
delegate_noop!(DisplayState: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_empty_and_overflowing_curtain_buffers() {
        assert_eq!(super::buffer_layout(1920, 1080), Some((7680, 8_294_400)));
        assert_eq!(super::buffer_layout(1920, 0), None);
        assert_eq!(super::buffer_layout(u32::MAX, 2), None);
    }
}
