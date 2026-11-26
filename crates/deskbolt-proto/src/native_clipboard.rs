use std::borrow::Cow;
use std::io::Cursor;
use std::path::{Component, Path};

use arboard::{Clipboard, ImageData};
use png::{BitDepth, ColorType, Decoder, Encoder, Limits, Transformations};

use crate::{ClipboardMsg, ClipboardSync, MAX_CLIPBOARD_BYTES, MAX_DECOMPRESSED_PAYLOAD};

pub struct NativeClipboard(Clipboard);

impl NativeClipboard {
    pub fn open() -> Option<Self> {
        Clipboard::new().ok().map(Self)
    }

    pub fn text(&mut self, limit: usize) -> Option<String> {
        self.0.get_text().ok().filter(|text| text.len() <= limit.min(MAX_CLIPBOARD_BYTES))
    }

    pub fn read(&mut self, limit: usize) -> Option<ClipboardMsg> {
        let limit = limit.min(MAX_CLIPBOARD_BYTES);
        if let Ok(paths) = self.0.get().file_list() {
            let paths: Vec<_> = paths
                .iter()
                .filter_map(|p| p.to_str())
                .filter(|p| local_path(p))
                .map(str::to_owned)
                .collect();
            let list = ClipboardMsg::FileDropList(paths);
            if (1..=limit).contains(&list.payload_bytes()) {
                return Some(list);
            }
        }
        if let Ok(html) = self.0.get().html()
            && html.len() <= limit
        {
            return Some(ClipboardMsg::Html(html));
        }
        if let Some(text) = self.text(limit) {
            return Some(ClipboardMsg::Utf8Text(text));
        }
        let png = encode_png(self.0.get_image().ok()?)?;
        (png.len() <= limit).then_some(ClipboardMsg::PngImage(png))
    }

    pub fn write(&mut self, message: &ClipboardSync, limit: usize) -> bool {
        if !message.within_limit(limit) {
            return false;
        }
        match message {
            ClipboardSync::SetText { utf8_text, .. } => self.0.set_text(utf8_text),
            ClipboardSync::Clear { .. } => self.0.clear(),
            ClipboardSync::SetData { data, .. } => match data {
                ClipboardMsg::Utf8Text(text) => self.0.set_text(text),
                ClipboardMsg::Html(html) => self.0.set_html(html, None),
                ClipboardMsg::PngImage(png) => {
                    let Some(image) = decode_png(png) else { return false };
                    self.0.set_image(image)
                }
                ClipboardMsg::FileDropList(paths) => {
                    if paths.is_empty() || !paths.iter().all(|p| local_path(p)) {
                        return false;
                    }
                    self.0.set().file_list(paths)
                }
            },
        }
        .is_ok()
    }
}

fn local_path(path: &str) -> bool {
    let valid_prefix = |component| match component {
        Component::Prefix(prefix) => matches!(prefix.kind(), std::path::Prefix::Disk(_)),
        Component::ParentDir => false,
        _ => true,
    };
    !path.chars().any(char::is_control)
        && !path.starts_with("//")
        && !path.starts_with("\\\\")
        && Path::new(path).is_absolute()
        && Path::new(path).components().all(valid_prefix)
}

fn encode_png(image: ImageData<'_>) -> Option<Vec<u8>> {
    let len = image.width.checked_mul(image.height)?.checked_mul(4)?;
    if len > MAX_DECOMPRESSED_PAYLOAD || len != image.bytes.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, image.width.try_into().ok()?, image.height.try_into().ok()?);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_color(ColorType::Rgba);
    encoder.write_header().ok()?.write_image_data(&image.bytes).ok()?;
    Some(out)
}

fn decode_png(bytes: &[u8]) -> Option<ImageData<'static>> {
    let mut decoder = Decoder::new(Cursor::new(bytes));
    decoder.set_limits(Limits { bytes: MAX_DECOMPRESSED_PAYLOAD });
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    let width = info.width as usize;
    let height = info.height as usize;
    let rgba_len = width.checked_mul(height)?.checked_mul(4)?;
    if rgba_len > MAX_DECOMPRESSED_PAYLOAD {
        return None;
    }
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut pixels).ok()?;
    pixels.truncate(frame.buffer_size());
    let bytes = match frame.color_type {
        ColorType::Rgba => pixels,
        ColorType::Rgb => pixels.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect(),
        ColorType::GrayscaleAlpha => pixels.as_chunks::<2>().0.iter().flat_map(|&[grey, alpha]| [grey, grey, grey, alpha]).collect(),
        ColorType::Grayscale => pixels.iter().flat_map(|&p| [p, p, p, 255]).collect(),
        ColorType::Indexed => return None,
    };
    (bytes.len() == rgba_len).then_some(ImageData { width, height, bytes: Cow::Owned(bytes) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_png_roundtrips_and_rejects_invalid_images() {
        let image = ImageData { width: 2, height: 1, bytes: Cow::Borrowed(&[0, 20, 40, 255, 4, 3, 2, 1]) };
        let png = encode_png(image.clone()).unwrap();
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height, decoded.bytes), (image.width, image.height, image.bytes));
        assert!(decode_png(&png[..20]).is_none());
        assert!(encode_png(ImageData { width: usize::MAX, height: 2, bytes: Cow::Borrowed(&[]) }).is_none());
    }

    #[test]
    fn clipboard_file_lists_require_local_absolute_paths() {
        assert!(!local_path("//host/share"));
        assert!(!local_path(r"\\host\share"));
        assert!(!local_path("file.txt"));
        assert!(!local_path("file\0.txt"));
        #[cfg(target_os = "windows")]
        {
            assert!(local_path(r"C:\Users\User\Desktop\file.txt"));
            assert!(!local_path(r"C:\Users\..\secret.txt"));
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert!(local_path("/home/user/Desktop/file.txt"));
            assert!(!local_path("/home/../secret.txt"));
        }
    }
}
