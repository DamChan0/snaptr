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
    use x11rb::connection::Connection;
    use x11rb::image::{Image, ImageOrder, PixelLayout};

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
            .map_err(|e| format!("capturing screen: {e}"))?;
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
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP,
        HDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
    };

    /// `GetDC(None)` releases through `ReleaseDC`, unlike a memory DC's `DeleteDC`.
    struct ScreenDc(HDC);
    impl Drop for ScreenDc {
        fn drop(&mut self) {
            unsafe {
                ReleaseDC(None, self.0);
            }
        }
    }

    struct MemDc(HDC);
    impl Drop for MemDc {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }

    struct Bitmap(HBITMAP);
    impl Drop for Bitmap {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }

    /// BitBlt the desktop into a compatible bitmap, then read it back as top-down 32-bit BGRA
    /// and repack to RGB. `r` is in physical virtual-desktop pixels; `CAPTUREBLT` makes layered
    /// (WS_EX_LAYERED) windows show up in the copy the way they do on screen.
    pub fn capture(r: Rect) -> Result<Vec<u8>, String> {
        unsafe {
            let screen = ScreenDc(GetDC(None));
            if screen.0.is_invalid() {
                return Err("capture: GetDC failed".into());
            }
            let mem = MemDc(CreateCompatibleDC(Some(screen.0)));
            if mem.0.is_invalid() {
                return Err("capture: CreateCompatibleDC failed".into());
            }
            let bitmap = Bitmap(CreateCompatibleBitmap(screen.0, r.w, r.h));
            if bitmap.0.is_invalid() {
                return Err("capture: CreateCompatibleBitmap failed".into());
            }
            // Deselect back to the DC's original bitmap right away so `bitmap` and `mem` can be
            // torn down independently of each other on every exit path below.
            let original = SelectObject(mem.0, bitmap.0.into());
            let blt = BitBlt(mem.0, 0, 0, r.w, r.h, Some(screen.0), r.x, r.y, SRCCOPY | CAPTUREBLT);
            SelectObject(mem.0, original);
            blt.map_err(|e| format!("capture: BitBlt failed: {e}"))?;

            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: r.w,
                    biHeight: -r.h, // negative: top-down rows, matching our row-major output
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bgra = vec![0u8; r.w as usize * r.h as usize * 4];
            let lines =
                GetDIBits(mem.0, bitmap.0, 0, r.h as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
            if lines == 0 {
                return Err("capture: GetDIBits failed".into());
            }

            let mut out = Vec::with_capacity(r.w as usize * r.h as usize * 3);
            for px in bgra.chunks_exact(4) {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            Ok(out)
        }
    }
}
