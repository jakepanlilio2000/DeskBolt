#[cfg(target_os = "windows")]
mod native {
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};

    pub(crate) struct HeadlessOutput;

    impl HeadlessOutput {
        pub fn open() -> Result<Self, String> {
            for _ in 0..3 {
                let (mut path_count, mut mode_count) = (0, 0);
                let status = unsafe {
                    GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count)
                };
                if status != ERROR_SUCCESS {
                    return Err(format!("Could not query virtual displays ({}).", status.0));
                }
                if path_count == 0 || path_count > 512 || mode_count > 2048 {
                    break;
                }
                let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
                let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
                let status = unsafe {
                    QueryDisplayConfig(
                        QDC_ALL_PATHS,
                        &mut path_count,
                        paths.as_mut_ptr(),
                        &mut mode_count,
                        modes.as_mut_ptr(),
                        None,
                    )
                };
                if status == ERROR_INSUFFICIENT_BUFFER {
                    continue;
                }
                if status != ERROR_SUCCESS {
                    return Err(format!(
                        "Could not find virtual display paths ({}).",
                        status.0
                    ));
                }
                // Never replace an existing desktop configuration if a physical display arrived during discovery.
                let paths = &paths[..path_count as usize];
                if paths.iter().any(|path| path.flags & 1 != 0) {
                    return Ok(Self);
                }
                for mut path in paths.iter().copied().filter(virtual_target) {
                    path.flags = 1;
                    path.sourceInfo.Anonymous.modeInfoIdx = u32::MAX;
                    path.targetInfo.Anonymous.modeInfoIdx = u32::MAX;
                    path.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL::default();
                    path.targetInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_UNSPECIFIED;
                    let flags = SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
                    let requested = [path];
                    if unsafe { SetDisplayConfig(Some(&requested), None, flags | SDC_VALIDATE) }
                        == 0
                        && unsafe { SetDisplayConfig(Some(&requested), None, flags | SDC_APPLY) }
                            == 0
                    {
                        return Ok(Self);
                    }
                }
                break;
            }
            Err("No virtual display adapter is available.".into())
        }
    }

    fn virtual_target(path: &DISPLAYCONFIG_PATH_INFO) -> bool {
        path.targetInfo.targetAvailable.as_bool()
            && path.targetInfo.outputTechnology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn selects_only_available_virtual_targets() {
            let mut path = DISPLAYCONFIG_PATH_INFO::default();
            assert!(!virtual_target(&path));
            path.targetInfo.outputTechnology = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL;
            assert!(!virtual_target(&path));
            path.targetInfo.targetAvailable = true.into();
            assert!(virtual_target(&path));
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use objc2::rc::{Allocated, Retained, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{msg_send, sel};
    use objc2_core_foundation::{CGPoint, CGSize};
    use objc2_foundation::{NSArray, NSString};

    unsafe extern "C" {
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *const AnyObject;
    }

    pub(crate) struct HeadlessOutput {
        _display: Retained<AnyObject>,
    }

    const UNAVAILABLE: &str = "The desktop does not support a virtual display.";

    fn verify(class: &AnyClass, methods: &[Sel]) -> Result<(), String> {
        if methods
            .iter()
            .all(|&method| class.instance_method(method).is_some())
        {
            Ok(())
        } else {
            Err(UNAVAILABLE.into())
        }
    }

    impl HeadlessOutput {
        pub fn open() -> Result<Self, String> {
            autoreleasepool(|_| Self::create())
        }

        fn create() -> Result<Self, String> {
            let descriptor_type =
                AnyClass::get(c"CGVirtualDisplayDescriptor").ok_or(UNAVAILABLE)?;
            let settings_type = AnyClass::get(c"CGVirtualDisplaySettings").ok_or(UNAVAILABLE)?;
            let mode_type = AnyClass::get(c"CGVirtualDisplayMode").ok_or(UNAVAILABLE)?;
            let display_type = AnyClass::get(c"CGVirtualDisplay").ok_or(UNAVAILABLE)?;
            verify(
                descriptor_type,
                &[
                    sel!(setName:),
                    sel!(setQueue:),
                    sel!(setVendorID:),
                    sel!(setProductID:),
                    sel!(setMaxPixelsWide:),
                    sel!(setMaxPixelsHigh:),
                    sel!(setSizeInMillimeters:),
                    sel!(setRedPrimary:),
                    sel!(setGreenPrimary:),
                    sel!(setBluePrimary:),
                    sel!(setWhitePoint:),
                ],
            )?;
            verify(settings_type, &[sel!(setModes:), sel!(setHiDPI:)])?;
            verify(mode_type, &[sel!(initWithWidth:height:refreshRate:)])?;
            verify(
                display_type,
                &[sel!(initWithDescriptor:), sel!(applySettings:)],
            )?;
            // Resolve the platform classes dynamically because the display provider is optional.
            unsafe {
                let queue = dispatch_get_global_queue(0, 0)
                    .as_ref()
                    .ok_or(UNAVAILABLE)?;
                let descriptor: Option<Retained<AnyObject>> = msg_send![descriptor_type, new];
                let descriptor = descriptor.ok_or(UNAVAILABLE)?;
                let name = NSString::from_str("Virtual Display");
                let _: () = msg_send![&*descriptor, setName: &*name];
                let _: () = msg_send![&*descriptor, setQueue: queue];
                let _: () = msg_send![&*descriptor, setVendorID: 0x5f1du32];
                let _: () = msg_send![&*descriptor, setProductID: 1u32];
                let serial = std::process::id().max(1);
                if descriptor_type
                    .instance_method(sel!(setSerialNumber:))
                    .is_some()
                {
                    let _: () = msg_send![&*descriptor, setSerialNumber: serial];
                } else if descriptor_type
                    .instance_method(sel!(setSerialNum:))
                    .is_some()
                {
                    let _: () = msg_send![&*descriptor, setSerialNum: serial];
                }
                let _: () = msg_send![&*descriptor, setMaxPixelsWide: 1920u32];
                let _: () = msg_send![&*descriptor, setMaxPixelsHigh: 1080u32];
                let _: () =
                    msg_send![&*descriptor, setSizeInMillimeters: CGSize::new(480.0, 270.0)];
                let _: () = msg_send![&*descriptor, setRedPrimary: CGPoint::new(0.64, 0.33)];
                let _: () = msg_send![&*descriptor, setGreenPrimary: CGPoint::new(0.30, 0.60)];
                let _: () = msg_send![&*descriptor, setBluePrimary: CGPoint::new(0.15, 0.06)];
                let _: () = msg_send![&*descriptor, setWhitePoint: CGPoint::new(0.3127, 0.329)];
                let allocation: Allocated<AnyObject> = msg_send![display_type, alloc];
                let display: Option<Retained<AnyObject>> =
                    msg_send![allocation, initWithDescriptor: &*descriptor];
                let display = display.ok_or(UNAVAILABLE)?;
                let settings: Option<Retained<AnyObject>> = msg_send![settings_type, new];
                let settings = settings.ok_or(UNAVAILABLE)?;
                let allocation: Allocated<AnyObject> = msg_send![mode_type, alloc];
                let mode: Option<Retained<AnyObject>> = msg_send![allocation, initWithWidth: 1920u32, height: 1080u32, refreshRate: 60.0f64];
                let modes = NSArray::from_retained_slice(&[mode.ok_or(UNAVAILABLE)?]);
                let _: () = msg_send![&*settings, setModes: &*modes];
                let _: () = msg_send![&*settings, setHiDPI: 0u32];
                let activated: Bool = msg_send![&*display, applySettings: &*settings];
                if !activated.as_bool() {
                    return Err(UNAVAILABLE.into());
                }
                Ok(Self { _display: display })
            }
        }
    }
}

pub(crate) use native::HeadlessOutput;
