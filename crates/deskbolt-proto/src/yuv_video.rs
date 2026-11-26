pub(crate) struct Yuv420<'a> {
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stored_height: usize,
    pub stride: usize,
    pub planar: bool,
}

impl Yuv420<'_> {
    pub fn write_rgba(&self, rgba: &mut Vec<u8>) -> bool {
        let Self {
            pixels,
            width,
            height,
            stored_height,
            stride,
            planar,
        } = *self;
        if width < 2
            || height < 2
            || width > stride
            || height > stored_height
            || !width.is_multiple_of(2)
            || !stride.is_multiple_of(2)
            || stored_height > 8192
            || stride > 16384
        {
            return false;
        }
        let uv = stride * stored_height;
        let stored_chroma_height = stored_height.div_ceil(2);
        if pixels.len() < uv + stride * stored_chroma_height {
            return false;
        }
        let chroma_stride = stride / 2;
        let v_start = uv + chroma_stride * stored_chroma_height;
        rgba.resize(width * height * 4, 255);
        for (row, output) in rgba.chunks_exact_mut(width * 4).enumerate() {
            for (column, pixel) in output.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let y = (i32::from(pixels[row * stride + column]) - 16).max(0) * 298;
                let (u, v) = if planar {
                    let index = row / 2 * chroma_stride + column / 2;
                    (pixels[uv + index], pixels[v_start + index])
                } else {
                    let index = uv + row / 2 * stride + (column & !1);
                    (pixels[index], pixels[index + 1])
                };
                let u = i32::from(u) - 128;
                let v = i32::from(v) - 128;
                let byte = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
                *pixel = [
                    byte(y + 409 * v),
                    byte(y - 100 * u - 208 * v),
                    byte(y + 516 * u),
                    255,
                ];
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_padded_planar_and_interleaved_surfaces() {
        let mut rgba = Vec::new();
        for planar in [false, true] {
            let mut surface = Yuv420 {
                pixels: &[235, 16, 0, 0, 16, 235, 0, 0, 128, 128, 128, 0],
                width: 2,
                height: 2,
                stride: 4,
                stored_height: 2,
                planar,
            };
            assert!(surface.write_rgba(&mut rgba));
            assert_eq!(&rgba[..8], &[255, 255, 255, 255, 0, 0, 0, 255]);
            surface.pixels = &surface.pixels[..9];
            assert!(!surface.write_rgba(&mut rgba));
        }
    }
}
