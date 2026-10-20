//! Frozen-screen capture in physical pixels, virtual-desktop coordinates.

use crate::logic::Rect;

/// Packed RGB (3 bytes/px, row-major) of screen area `r`, including all windows on it.
pub fn capture(r: Rect) -> Result<Vec<u8>, String> {
    if r.w <= 0 || r.h <= 0 {
        return Err(format!("capture: empty rect {r:?}"));
    }
    platform::capture(r)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Rect;
    use x11rb::image::{Image, ImageOrder, PixelLayout};
    use x11rb::image::{Image, PixelLayout};

    /// GetImage on the root window returns what is on screen, children included.
    pub fn capture(r: Rect) -> Result<Vec<u8>, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("connecting to X server: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let visual = screen
            .allowed_depths
            .iter()
            .find_map(|d| d.visuals.iter().find(|v| v.visual_id == screen.root_visual).copied())
            .ok_or("root visual not found")?;
        let layout = PixelLayout::from_visual_type(visual).map_err(|e| format!("unsupported root visual: {e:?}"))?;
        let (img, _) = Image::get(&conn, screen.root, r.x as i16, r.y as i16, r.w as u16, r.h as u16)
        let mut out = Vec::with_capacity((r.w * r.h * 3) as usize);
        let bgrx = img.byte_order() == ImageOrder::LsbFirst
            && img.data().len() >= r.w as usize * r.h as usize * 4
            && layout.encode((0xffff, 0, 0)) == 0x00ff_0000
            && layout.encode((0, 0, 0xffff)) == 0x0000_00ff;
        if bgrx {
            // Common 24/32-bit TrueColor: copy bytes instead of decoding pixel by pixel.
            let stride = img.data().len() / usize::from(img.height());
            for row in img.data().chunks_exact(stride) {
                for p in row[..r.w as usize * 4].chunks_exact(4) {
                    out.extend_from_slice(&[p[2], p[1], p[0]]);
                }
            }
            return Ok(out);
        }
        for y in 0..img.height() {
            for x in 0..img.width() {
                let (red, green, blue) = layout.decode(img.get_pixel(x, y));
                out.extend_from_slice(&[(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8]);
            }
        }
        Ok(out)
    }
}

#[cfg(windows)]
mod platform {
    use super::Rect;

    pub fn capture(_r: Rect) -> Result<Vec<u8>, String> {
        Err("capture: Windows not implemented yet".into())
    }
}
