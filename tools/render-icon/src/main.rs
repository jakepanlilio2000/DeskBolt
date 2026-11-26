use std::io;
use std::path::PathBuf;

use resvg::{tiny_skia, usvg};

const ICON: &[u8] = include_bytes!("../../../assets/icon.svg");
const SIZES: [u32; 2] = [256, 512];

fn render(tree: &usvg::Tree, size: u32) -> tiny_skia::Pixmap {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("valid icon dimensions");
    let transform = tiny_skia::Transform::from_scale(
        size as f32 / tree.size().width(),
        size as f32 / tree.size().height(),
    );
    resvg::render(tree, transform, &mut pixmap.as_mut());
    pixmap
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os().nth(1).map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Provide an output directory."))?;
    let tree = usvg::Tree::from_data(ICON, &usvg::Options::default())?;
    std::fs::create_dir_all(&output)?;
    for size in SIZES {
        render(&tree, size).save_png(output.join(format!("deskbolt-{size}.png")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_pngs_decode_at_the_requested_size_and_have_visible_pixels() {
        let tree = usvg::Tree::from_data(ICON, &usvg::Options::default()).unwrap();
        for size in SIZES {
            let png = render(&tree, size).encode_png().unwrap();
            let decoded = tiny_skia::Pixmap::decode_png(&png).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (size, size));
            assert!(decoded.pixels().iter().any(|pixel| pixel.alpha() == 255));
            assert_eq!(decoded.pixel(0, 0).unwrap().alpha(), 0);
        }
    }
}
