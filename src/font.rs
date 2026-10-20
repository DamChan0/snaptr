//! Font loading (via `fc-match`) and glyph rasterization onto `x11rb::image::Image` buffers.

use ab_glyph::{Font as FontTrait, ScaleFont};
use x11rb::image::{Image, PixelLayout};

pub struct Font {
    inner: ab_glyph::FontVec,
}

impl Font {
    /// Resolve and load a font for `lang` (e.g. "ko", "eng") via fontconfig.
    pub fn load(lang: &str) -> Result<Font, String> {
        let query = format!("sans-serif:lang={lang}");
        let out = std::process::Command::new("fc-match")
            .args(["-f", "%{file}|%{index}", &query])
            .output()
            .map_err(|e| format!("fc-match not found: {e}"))?;
        if !out.status.success() {
            return Err("fc-match failed to resolve a font".into());
        }
        let s = String::from_utf8_lossy(&out.stdout);
        let (path, index_str) = s.split_once('|').ok_or("unexpected fc-match output")?;
        let index: u32 = index_str.trim().parse().unwrap_or(0);
        let data = std::fs::read(path).map_err(|e| format!("reading font {path}: {e}"))?;
        let inner = ab_glyph::FontVec::try_from_vec_and_index(data, index)
            .map_err(|e| format!("parsing font {path}: {e:?}"))?;
        Ok(Font { inner })
    }

    fn scaled(&self, px: f32) -> ab_glyph::PxScaleFont<&ab_glyph::FontVec> {
        self.inner.as_scaled(px)
    }

    /// Advance width of a single character at the given pixel size; usable as the
    /// `char_width` closure for `logic::wrap_text`.
    pub fn char_width(&self, px: f32, c: char) -> f32 {
        let scaled = self.scaled(px);
        scaled.h_advance(scaled.glyph_id(c))
    }

    /// Line height per the design spec: 1.45 x font px for every font size, chrome and
    /// translation text alike (overrides the font's natural ascent/descent/line-gap metric).
    pub fn line_height(&self, px: f32) -> f32 {
        px * 1.45
    }

    pub fn ascent(&self, px: f32) -> f32 {
        self.scaled(px).ascent()
    }

    /// Measure the pixel width of a single line of text at the given size.
    pub fn text_width(&self, px: f32, line: &str) -> f32 {
        line.chars().map(|c| self.char_width(px, c)).sum()
    }

    /// Draw left-aligned `lines` of text into `image` starting at `(x0, y0)` (top-left of
    /// the text block), in `color` (r, g, b each 0-255). Pixels outside the image bounds
    /// are silently clipped.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_lines(
        &self,
        image: &mut Image,
        layout: PixelLayout,
        px: f32,
        lines: &[String],
        x0: i32,
        y0: i32,
        line_height: f32,
        color: (u8, u8, u8),
    ) {
        let scaled = self.scaled(px);
        let ascent = scaled.ascent();
        let pixel = layout.encode(byte_to_component(color));
        let w = image.width() as i32;
        let h = image.height() as i32;

        for (i, line) in lines.iter().enumerate() {
            let mut pen_x = x0 as f32;
            let pen_y = y0 as f32 + ascent + i as f32 * line_height;
            for c in line.chars() {
                let gid = scaled.glyph_id(c);
                let glyph = gid.with_scale_and_position(px, ab_glyph::point(pen_x, pen_y));
                if let Some(outlined) = self.inner.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    outlined.draw(|gx, gy, coverage| {
                        if coverage <= 0.0 {
                            return;
                        }
                        let px_ = bounds.min.x as i32 + gx as i32;
                        let py_ = bounds.min.y as i32 + gy as i32;
                        if px_ < 0 || py_ < 0 || px_ >= w || py_ >= h {
                            return;
                        }
                        let (x, y) = (px_ as u16, py_ as u16);
                        if coverage >= 0.999 {
                            image.put_pixel(x, y, pixel);
                        } else {
                            let bg = layout.decode(image.get_pixel(x, y));
                            let fg = byte_to_component(color);
                            let blended = (
                                lerp16(bg.0, fg.0, coverage),
                                lerp16(bg.1, fg.1, coverage),
                                lerp16(bg.2, fg.2, coverage),
                            );
                            image.put_pixel(x, y, layout.encode(blended));
                        }
                    });
                }
                pen_x += scaled.h_advance(gid);
            }
        }
    }

    /// Draw left-aligned `lines` of text into a packed RGB (3 bytes/px, row-major) buffer
    /// of size `w x h`, starting at canvas-local `(x0, y0)` (top-left of the text block),
    /// in `color`. Pixels outside the buffer bounds are silently clipped.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_lines_rgb(
        &self,
        buf: &mut [u8],
        w: i32,
        h: i32,
        px: f32,
        lines: &[String],
        x0: i32,
        y0: i32,
        line_height: f32,
        color: (u8, u8, u8),
    ) {
        let scaled = self.scaled(px);
        let ascent = scaled.ascent();

        for (i, line) in lines.iter().enumerate() {
            let mut pen_x = x0 as f32;
            let pen_y = y0 as f32 + ascent + i as f32 * line_height;
            for c in line.chars() {
                let gid = scaled.glyph_id(c);
                let glyph = gid.with_scale_and_position(px, ab_glyph::point(pen_x, pen_y));
                if let Some(outlined) = self.inner.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    outlined.draw(|gx, gy, coverage| {
                        if coverage <= 0.0 {
                            return;
                        }
                        let px_ = bounds.min.x as i32 + gx as i32;
                        let py_ = bounds.min.y as i32 + gy as i32;
                        if px_ < 0 || py_ < 0 || px_ >= w || py_ >= h {
                            return;
                        }
                        let idx = ((py_ * w + px_) * 3) as usize;
                        if coverage >= 0.999 {
                            buf[idx] = color.0;
                            buf[idx + 1] = color.1;
                            buf[idx + 2] = color.2;
                        } else {
                            buf[idx] = lerp8(buf[idx], color.0, coverage);
                            buf[idx + 1] = lerp8(buf[idx + 1], color.1, coverage);
                            buf[idx + 2] = lerp8(buf[idx + 2], color.2, coverage);
                        }
                    });
                }
                pen_x += scaled.h_advance(gid);
            }
        }
    }
}

fn byte_to_component(c: (u8, u8, u8)) -> (u16, u16, u16) {
    let up = |v: u8| (v as u16) << 8 | v as u16;
    (up(c.0), up(c.1), up(c.2))
}

fn lerp16(a: u16, b: u16, t: f32) -> u16 {
    (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 65535.0) as u16
}

fn lerp8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
}
