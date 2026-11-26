use std::mem::size_of;

use deskbolt_proto::{CursorShape, CursorSnapshot, MAX_CURSOR_SIDE, MonitorInfo};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HDC, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};

#[derive(Default)]
pub struct WindowsCursor {
    shape: Option<(usize, CursorShape)>,
}

impl WindowsCursor {
    pub fn sample(&mut self, monitor: &MonitorInfo) -> Option<CursorSnapshot> {
        let mut cursor = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        // SAFETY: The structure is initialized with its required size.
        unsafe { GetCursorInfo(&mut cursor).ok()?; }
        if cursor.flags.0 & CURSOR_SHOWING.0 == 0 || cursor.flags.0 & CURSOR_SUPPRESSED.0 != 0 {
            return None;
        }
        let x = cursor.ptScreenPos.x.checked_sub(monitor.left)?;
        let y = cursor.ptScreenPos.y.checked_sub(monitor.top)?;
        if x < 0 || y < 0 || x as u32 >= monitor.width || y as u32 >= monitor.height {
            return None;
        }
        let handle = cursor.hCursor.0 as usize;
        if self.shape.as_ref().is_none_or(|(current, _)| *current != handle) {
            self.shape = read_shape(HICON(cursor.hCursor.0)).map(|shape| (handle, shape));
        }
        let shape = self.shape.as_ref()?.1.clone();
        Some(CursorSnapshot { x: x.try_into().ok()?, y: y.try_into().ok()?, shape })
    }
}

fn read_shape(cursor: HICON) -> Option<CursorShape> {
    let mut icon = ICONINFO::default();
    // SAFETY: The cursor is borrowed. GetIconInfo returns two owned bitmap handles.
    unsafe { GetIconInfo(cursor, &mut icon).ok()?; }
    let dc = unsafe { GetDC(None) };
    let result = if dc.is_invalid() { None } else {
        read_shape_bitmaps(dc, &icon)
    };
    // SAFETY: Release the screen DC and delete the returned bitmap handles, never the cursor.
    unsafe {
        if !dc.is_invalid() { let _ = ReleaseDC(None, dc); }
        if !icon.hbmColor.is_invalid() { let _ = DeleteObject(icon.hbmColor.into()); }
        if !icon.hbmMask.is_invalid() { let _ = DeleteObject(icon.hbmMask.into()); }
    }
    result
}

fn read_shape_bitmaps(dc: HDC, icon: &ICONINFO) -> Option<CursorShape> {
    let mono = icon.hbmColor.is_invalid();
    let source = if mono { icon.hbmMask } else { icon.hbmColor };
    let mut bitmap = BITMAP::default();
    // SAFETY: Output points to a correctly sized BITMAP record.
    let copied = unsafe { GetObjectW(source.into(), size_of::<BITMAP>() as i32, Some((&mut bitmap as *mut BITMAP).cast())) };
    if copied == 0 {
        return None;
    }
    let width = u16::try_from(bitmap.bmWidth).ok()?;
    let mut height = u16::try_from(bitmap.bmHeight).ok()?;
    if mono {
        if !height.is_multiple_of(2) { return None; }
        height /= 2;
    }
    if width == 0 || height == 0 || width > MAX_CURSOR_SIDE || height > MAX_CURSOR_SIDE {
        return None;
    }
    let hotspot_x = u16::try_from(icon.xHotspot).ok()?;
    let hotspot_y = u16::try_from(icon.yHotspot).ok()?;
    if hotspot_x >= width || hotspot_y >= height {
        return None;
    }
    let rgba = if mono {
        let bits = bitmap_bgra(dc, icon.hbmMask, width, height * 2)?;
        monochrome_pixels(&bits)
    } else {
        let color = bitmap_bgra(dc, icon.hbmColor, width, height)?;
        let has_alpha = color.chunks_exact(4).any(|pixel| pixel[3] != 0);
        let mask = (!has_alpha).then(|| bitmap_bgra(dc, icon.hbmMask, width, height)).flatten();
        color.chunks_exact(4).enumerate().flat_map(|(i, pixel)| {
            let alpha = if has_alpha { pixel[3] } else if mask.as_ref().is_some_and(|m| m[i * 4] != 0) { 0 } else { 255 };
            [pixel[2], pixel[1], pixel[0], alpha]
        }).collect()
    };
    Some(CursorShape { width, height, hotspot_x, hotspot_y, rgba })
}

fn bitmap_bgra(dc: HDC, source: HBITMAP, width: u16, height: u16) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: i32::from(width),
            biHeight: -i32::from(height),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = vec![0u8; usize::from(width) * usize::from(height) * 4];
    // SAFETY: The vector holds all scan lines requested by the initialized bitmap descriptor.
    let rows = unsafe {
        GetDIBits(dc, source, 0, u32::from(height), Some(bytes.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS)
    };
    (rows == i32::from(height)).then_some(bytes)
}

fn monochrome_pixels(bits: &[u8]) -> Vec<u8> {
    let (and, xor) = bits.split_at(bits.len() / 2);
    and.chunks_exact(4).zip(xor.chunks_exact(4)).flat_map(|(and, xor)| {
        // shortcut: Draw the invert mask in white, add exact background inversion with a compositing mask.
        let shade = if xor[0] == 0 { 0 } else { 255 };
        let alpha = if and[0] != 0 && xor[0] == 0 { 0 } else { 255 };
        [shade, shade, shade, alpha]
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_mask_maps_opaque_and_transparent_pixels() {
        let bits = [0, 0, 0, 0, 255, 255, 255, 0, 0, 0, 0, 0, 255, 255, 255, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 0, 255, 255, 255, 0];
        assert_eq!(monochrome_pixels(&bits), [0, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255]);
    }
}
