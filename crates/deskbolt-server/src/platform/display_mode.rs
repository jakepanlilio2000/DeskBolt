use crate::display_modes::DisplayMode;
use deskbolt_proto::MonitorInfo;

#[cfg(target_os = "windows")]
mod native {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CDS_FULLSCREEN, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
        DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
        ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
    };
    use windows::core::PCWSTR;

    fn device(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn query(name: &[u16], index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..DEVMODEW::default()
        };
        unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), index, &mut mode).as_bool() }
            .then_some(mode)
    }

    fn describe(name: &str, mode: DEVMODEW) -> DisplayMode {
        DisplayMode {
            output: name.into(),
            id: 0,
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            hz: mode.dmDisplayFrequency,
        }
    }

    pub(crate) fn modes(monitor: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String> {
        let name = device(&monitor.name);
        let original = current(&monitor.name)?;
        let modes = (0..4096)
            .map_while(|index| query(&name, ENUM_DISPLAY_SETTINGS_MODE(index)))
            .filter(|mode| mode.dmBitsPerPel == 32)
            .map(|mode| describe(&monitor.name, mode))
            .collect();
        Ok((original, modes))
    }

    pub(crate) fn current(output: &str) -> Result<DisplayMode, String> {
        query(&device(output), ENUM_CURRENT_SETTINGS)
            .map(|mode| describe(output, mode))
            .ok_or_else(|| "The display is unavailable.".into())
    }

    pub(crate) fn apply(mode: &DisplayMode) -> Result<(), String> {
        let name = device(&mode.output);
        let mut config =
            query(&name, ENUM_CURRENT_SETTINGS).ok_or("The display is unavailable.")?;
        config.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        (
            config.dmPelsWidth,
            config.dmPelsHeight,
            config.dmDisplayFrequency,
        ) = (mode.width, mode.height, mode.hz);
        let status = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name.as_ptr()),
                Some(&config),
                None,
                CDS_FULLSCREEN,
                None,
            )
        };
        if status == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(format!(
                "The display rejected the temporary mode ({}).",
                status.0
            ))
        }
    }
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::{
        ConnectionExt, GetCrtcInfoReply, GetScreenResourcesCurrentReply, ModeInfo, Rotation,
        SetConfig,
    };
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    struct Displays {
        conn: RustConnection,
        root: u32,
        resources: GetScreenResourcesCurrentReply,
    }

    struct Output {
        name: String,
        crtc: u32,
        geometry: GetCrtcInfoReply,
        modes: Vec<u32>,
    }

    fn error(error: impl std::fmt::Display) -> String {
        format!("Display mode unavailable: {error}")
    }

    impl Displays {
        fn open() -> Result<Self, String> {
            if super::super::wayland_capture::is_wayland() {
                return Err("This desktop does not expose display mode changes.".into());
            }
            let (conn, screen) = x11rb::connect(None).map_err(error)?;
            let root = conn.setup().roots[screen].root;
            let resources = conn
                .randr_get_screen_resources_current(root)
                .map_err(error)?
                .reply()
                .map_err(error)?;
            Ok(Self {
                conn,
                root,
                resources,
            })
        }

        fn outputs(&self) -> Vec<Output> {
            self.resources
                .outputs
                .iter()
                .filter_map(|&output| {
                    let info = self
                        .conn
                        .randr_get_output_info(output, self.resources.config_timestamp)
                        .ok()?
                        .reply()
                        .ok()?;
                    if info.crtc == 0 {
                        return None;
                    }
                    let geometry = self
                        .conn
                        .randr_get_crtc_info(info.crtc, self.resources.config_timestamp)
                        .ok()?
                        .reply()
                        .ok()?;
                    Some(Output {
                        name: String::from_utf8(info.name).ok()?,
                        crtc: info.crtc,
                        geometry,
                        modes: info.modes,
                    })
                })
                .collect()
        }
    }

    impl Output {
        fn describe(&self, mode: &ModeInfo) -> DisplayMode {
            let rotation = u16::from(self.geometry.rotation);
            let rotated = rotation & u16::from(Rotation::ROTATE90 | Rotation::ROTATE270) != 0;
            let (width, height) = if rotated {
                (mode.height, mode.width)
            } else {
                (mode.width, mode.height)
            };
            let clocks = u64::from(mode.htotal) * u64::from(mode.vtotal);
            let hz = if clocks == 0 {
                0
            } else {
                ((u64::from(mode.dot_clock) + clocks / 2) / clocks) as u32
            };
            DisplayMode {
                output: self.name.clone(),
                id: u64::from(mode.id),
                width: width.into(),
                height: height.into(),
                hz,
            }
        }

        fn current(&self, displays: &Displays) -> Result<DisplayMode, String> {
            displays
                .resources
                .modes
                .iter()
                .find(|mode| mode.id == self.geometry.mode)
                .map(|info| self.describe(info))
                .ok_or_else(|| "The current display mode is unavailable.".into())
        }
    }

    pub(crate) fn modes(monitor: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String> {
        let displays = Displays::open()?;
        let output = displays
            .outputs()
            .into_iter()
            .find(|out| {
                i32::from(out.geometry.x) == monitor.left
                    && i32::from(out.geometry.y) == monitor.top
                    && u32::from(out.geometry.width) == monitor.width
                    && u32::from(out.geometry.height) == monitor.height
            })
            .ok_or("The display is unavailable.")?;
        let geometry = displays
            .conn
            .get_geometry(displays.root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let modes = displays
            .resources
            .modes
            .iter()
            .filter(|m| output.modes.contains(&m.id))
            .map(|mode| output.describe(mode))
            .filter(|mode| {
                i64::from(output.geometry.x) + i64::from(mode.width) <= i64::from(geometry.width)
                    && i64::from(output.geometry.y) + i64::from(mode.height)
                        <= i64::from(geometry.height)
            })
            .collect();
        Ok((output.current(&displays)?, modes))
    }

    pub(crate) fn current(name: &str) -> Result<DisplayMode, String> {
        let displays = Displays::open()?;
        displays
            .outputs()
            .into_iter()
            .find(|out| out.name == name)
            .ok_or("The display is unavailable.")?
            .current(&displays)
    }

    pub(crate) fn apply(mode: &DisplayMode) -> Result<(), String> {
        let displays = Displays::open()?;
        let output = displays
            .outputs()
            .into_iter()
            .find(|out| out.name == mode.output)
            .ok_or("The display is unavailable.")?;
        let id = u32::try_from(mode.id).map_err(error)?;
        if !output.modes.contains(&id) {
            return Err("The display no longer supports this mode.".into());
        }
        let g = output.geometry;
        let result = displays
            .conn
            .randr_set_crtc_config(
                output.crtc,
                0,
                displays.resources.config_timestamp,
                g.x,
                g.y,
                id,
                g.rotation,
                &g.outputs,
            )
            .map_err(error)?
            .reply()
            .map_err(error)?;
        if result.status == SetConfig::SUCCESS {
            Ok(())
        } else {
            Err("The display rejected the temporary mode.".into())
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    type Ref = *const c_void;

    #[repr(C)]
    struct Bounds {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayBounds(display: u32) -> Bounds;
        fn CGDisplayCopyDisplayMode(display: u32) -> Ref;
        fn CGDisplayCopyAllDisplayModes(display: u32, options: Ref) -> Ref;
        fn CGDisplayModeGetIODisplayModeID(mode: Ref) -> i32;
        fn CGDisplayModeGetPixelWidth(mode: Ref) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: Ref) -> usize;
        fn CGDisplayModeGetRefreshRate(mode: Ref) -> f64;
        fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
        fn CGConfigureDisplayWithDisplayMode(
            config: *mut c_void,
            display: u32,
            mode: Ref,
            options: Ref,
        ) -> i32;
        fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
        fn CGCancelDisplayConfiguration(config: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: Ref);
        fn CFArrayGetCount(value: Ref) -> isize;
        fn CFArrayGetValueAtIndex(value: Ref, index: isize) -> Ref;
    }

    struct Owned(Ref);

    impl Owned {
        fn retain(value: Ref) -> Result<Self, String> {
            if value.is_null() {
                Err("The display is unavailable.".into())
            } else {
                Ok(Self(value))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    fn check(status: i32) -> Result<(), String> {
        if status == 0 {
            Ok(())
        } else {
            Err(format!("Could not change display mode ({status})."))
        }
    }

    fn describe(display: u32, mode: Ref) -> DisplayMode {
        unsafe {
            DisplayMode {
                output: display.to_string(),
                id: CGDisplayModeGetIODisplayModeID(mode) as u32 as u64,
                width: CGDisplayModeGetPixelWidth(mode) as u32,
                height: CGDisplayModeGetPixelHeight(mode) as u32,
                hz: CGDisplayModeGetRefreshRate(mode).round() as u32,
            }
        }
    }

    fn display_id(output: &str) -> Result<u32, String> {
        output
            .parse()
            .map_err(|_| "The display identifier is invalid.".into())
    }

    pub(crate) fn modes(monitor: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String> {
        let mut active = [0; 32];
        let mut count = 0;
        check(unsafe {
            CGGetActiveDisplayList(active.len() as u32, active.as_mut_ptr(), &mut count)
        })?;
        let display = active
            .into_iter()
            .take(count as usize)
            .find(|&id| {
                let bounds = unsafe { CGDisplayBounds(id) };
                bounds.x.round() as i32 == monitor.left && bounds.y.round() as i32 == monitor.top
            })
            .ok_or("The display is unavailable.")?;
        let list = Owned::retain(unsafe { CGDisplayCopyAllDisplayModes(display, null()) })?;
        let count = unsafe { CFArrayGetCount(list.0) };
        let modes = (0..count)
            .map(|index| describe(display, unsafe { CFArrayGetValueAtIndex(list.0, index) }))
            .collect();
        Ok((current(&display.to_string())?, modes))
    }

    pub(crate) fn current(output: &str) -> Result<DisplayMode, String> {
        let display = display_id(output)?;
        let mode = Owned::retain(unsafe { CGDisplayCopyDisplayMode(display) })?;
        Ok(describe(display, mode.0))
    }

    pub(crate) fn apply(mode: &DisplayMode) -> Result<(), String> {
        let display = display_id(&mode.output)?;
        let list = Owned::retain(unsafe { CGDisplayCopyAllDisplayModes(display, null()) })?;
        let count = unsafe { CFArrayGetCount(list.0) };
        let target = (0..count)
            .map(|index| unsafe { CFArrayGetValueAtIndex(list.0, index) })
            .find(|&target| describe(display, target) == *mode)
            .ok_or("The display no longer supports this mode.")?;
        let mut config = null_mut();
        check(unsafe { CGBeginDisplayConfiguration(&mut config) })?;
        let status = unsafe { CGConfigureDisplayWithDisplayMode(config, display, target, null()) };
        if status != 0 {
            unsafe { CGCancelDisplayConfiguration(config) };
            return check(status);
        }
        // App-only changes are also restored automatically when the agent exits.
        check(unsafe { CGCompleteDisplayConfiguration(config, 0) })
    }
}

#[cfg(target_os = "android")]
mod native {
    use super::*;
    const UNAVAILABLE: &str = "Changing display modes is unavailable on this host.";

    pub(crate) fn modes(_: &MonitorInfo) -> Result<(DisplayMode, Vec<DisplayMode>), String> {
        Err(UNAVAILABLE.into())
    }
    pub(crate) fn current(_: &str) -> Result<DisplayMode, String> {
        Err(UNAVAILABLE.into())
    }
    pub(crate) fn apply(_: &DisplayMode) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
}

pub(crate) use native::{apply, current, modes};
