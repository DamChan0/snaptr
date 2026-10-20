//! Pure client-side compositing: rounded rects, strokes, shadows, chips and translation
//! boxes. No windowing calls here; operates on an in-memory RGB canvas. Fixed design tokens
//! live here as `pub const`s; user-configurable colors arrive as a `Theme`.

use crate::font::Font;
use crate::logic::Rect;

pub type Rgb = (u8, u8, u8);

/// User-configurable colors (from `Config`). Secondary text is derived from `text`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// Highlights, selection outline, hover/copied borders.
    pub accent: Rgb,
    /// Translation box and chip background.
    pub surface: Rgb,
    /// Translation text.
    pub text: Rgb,
}

impl Theme {
    /// `text` pulled 35% toward `surface`: status chips and failed translations.
    pub fn text_secondary(&self) -> Rgb {
        let mix = |t: u8, s: u8| lerp8(t, s, 0.35);
        (mix(self.text.0, self.surface.0), mix(self.text.1, self.surface.1), mix(self.text.2, self.surface.2))
    }
}

// ---- design tokens (design.md "Tokens") ----

pub const SURFACE_ALPHA: f32 = 0.97;
pub const SURFACE_BORDER: Rgb = (0xFF, 0xFF, 0xFF);
pub const SURFACE_BORDER_ALPHA: f32 = 0.08;

/// Radius for translation boxes and chips.
pub const RADIUS_BOX: f32 = 8.0;
/// Radius for highlights over original text blocks.
pub const RADIUS_HIGHLIGHT: f32 = 4.0;

pub const SHADOW_OFFSET_Y: i32 = 6;
pub const SHADOW_BLUR: f32 = 18.0;
pub const SHADOW_ALPHA: f32 = 0.38;

pub const SPACE_8: i32 = 8;
pub const SPACE_12: i32 = 12;
pub const SPACE_16: i32 = 16;
pub const SPACE_24: i32 = 24;

/// Gap between a translation box and its original block (design.md item 5).
pub const GAP: i32 = SPACE_8;

const HIGHLIGHT_INFLATE: i32 = 3;
const HIGHLIGHT_FILL_ALPHA: f32 = 0.22;
const HIGHLIGHT_FILL_ALPHA_HOVER: f32 = 0.34;
const HIGHLIGHT_STROKE_ALPHA: f32 = 0.70;
const HIGHLIGHT_STROKE_ALPHA_HOVER: f32 = 1.0;

const CHIP_PAD_X: i32 = SPACE_12;
const CHIP_PAD_Y: i32 = 6;

const BORDER_WIDTH: f32 = 1.0;

/// Extra pixels around an element's rect that its painting touches (shadow spread). The
/// caller fetches background for `inflate(rect, PAINT_MARGIN)`. Derived from the fixed
/// shadow params above: 3-pass box blur spread (~3 * blur/3) plus the y offset plus a
/// 1px AA feather.
pub const PAINT_MARGIN: i32 = SHADOW_BLUR as i32 + SHADOW_OFFSET_Y + 2;

/// Packed RGB, 3 bytes/px, row-major. Canvas origin = screen point `origin`.
pub struct Canvas {
    pub origin: (i32, i32),
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
}

impl Canvas {
    pub fn new(origin: (i32, i32), w: i32, h: i32, px: Vec<u8>) -> Canvas {
        Canvas { origin, w, h, px }
    }

    fn to_local(&self, x: i32, y: i32) -> (i32, i32) {
        (x - self.origin.0, y - self.origin.1)
    }

    fn blend(&mut self, x: i32, y: i32, c: Rgb, alpha: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || alpha <= 0.0 {
            return;
        }
        let a = alpha.min(1.0);
        let idx = ((y * self.w + x) * 3) as usize;
        self.px[idx] = lerp8(self.px[idx], c.0, a);
        self.px[idx + 1] = lerp8(self.px[idx + 1], c.1, a);
        self.px[idx + 2] = lerp8(self.px[idx + 2], c.2, a);
    }

    /// Rounded-rect fill with a 1px antialiased feather at the corners/edges.
    pub fn fill_rounded(&mut self, r: Rect, radius: f32, c: Rgb, alpha: f32) {
        let (lx, ly) = self.to_local(r.x, r.y);
        let (cx, cy, hw, hh, rad) = rounded_geometry(lx, ly, r.w, r.h, radius);
        let x0 = (lx - 1).max(0);
        let y0 = (ly - 1).max(0);
        let x1 = (lx + r.w + 1).min(self.w);
        let y1 = (ly + r.h + 1).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = rounded_sdf(x as f32 + 0.5, y as f32 + 0.5, cx, cy, hw, hh, rad);
                let cov = coverage_from_sdf(d);
                if cov > 0.0 {
                    self.blend(x, y, c, alpha * cov);
                }
            }
        }
    }

    /// Rounded-rect stroke of `width` px, drawn inward from the outer edge, AA on both
    /// the outer and inner boundary.
    pub fn stroke_rounded(&mut self, r: Rect, radius: f32, width: f32, c: Rgb, alpha: f32) {
        let (lx, ly) = self.to_local(r.x, r.y);
        let (cx, cy, hw, hh, rad) = rounded_geometry(lx, ly, r.w, r.h, radius);
        let inner_rad = (rad - width).max(0.0);
        let x0 = (lx - 1).max(0);
        let y0 = (ly - 1).max(0);
        let x1 = (lx + r.w + 1).min(self.w);
        let y1 = (ly + r.h + 1).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let d_outer = rounded_sdf(px, py, cx, cy, hw, hh, rad);
                let d_inner = rounded_sdf(px, py, cx, cy, hw - width, hh - width, inner_rad);
                let cov = (coverage_from_sdf(d_outer) - coverage_from_sdf(d_inner)).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, alpha * cov);
                }
            }
        }
    }

    /// Shadow: a rounded-rect alpha mask, offset down by `offset_y`, blurred by a 3-pass
    /// box blur approximating a Gaussian of the given `blur` size, composited as black.
    /// Operates over the whole canvas (sized by the caller to rect + PAINT_MARGIN), so an
    /// out-of-canvas `r` just paints nothing without panicking.
    pub fn shadow(&mut self, r: Rect, radius: f32, offset_y: i32, blur: f32, alpha: f32) {
        let (w, h) = (self.w, self.h);
        if w <= 0 || h <= 0 {
            return;
        }
        let (lx, ly) = self.to_local(r.x, r.y + offset_y);
        let (cx, cy, hw, hh, rad) = rounded_geometry(lx, ly, r.w, r.h, radius);

        let mut mask = vec![0f32; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let d = rounded_sdf(x as f32 + 0.5, y as f32 + 0.5, cx, cy, hw, hh, rad);
                mask[(y * w + x) as usize] = coverage_from_sdf(d);
            }
        }

        let box_radius = ((blur / 3.0).round() as i32).max(1);
        for _ in 0..3 {
            mask = box_blur_axis(&mask, w, h, box_radius, true);
            mask = box_blur_axis(&mask, w, h, box_radius, false);
        }

        for y in 0..h {
            for x in 0..w {
                let m = mask[(y * w + x) as usize];
                if m > 0.0 {
                    self.blend(x, y, (0, 0, 0), alpha * m);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(&mut self, font: &Font, px: f32, lines: &[String], x: i32, y: i32, line_h: f32, c: Rgb) {
        let (lx, ly) = self.to_local(x, y);
        font.draw_lines_rgb(&mut self.px, self.w, self.h, px, lines, lx, ly, line_h, c);
    }
}

fn rounded_geometry(lx: i32, ly: i32, w: i32, h: i32, radius: f32) -> (f32, f32, f32, f32, f32) {
    let hw = w as f32 / 2.0;
    let hh = h as f32 / 2.0;
    let cx = lx as f32 + hw;
    let cy = ly as f32 + hh;
    let rad = radius.min(hw).min(hh).max(0.0);
    (cx, cy, hw, hh, rad)
}

/// Signed distance from `(px, py)` to a rounded rect centered at `(cx, cy)` with half
/// extents `(hw, hh)` and corner radius `radius`. Negative inside, 0 at the edge.
fn rounded_sdf(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, radius: f32) -> f32 {
    let qx = (px - cx).abs() - (hw - radius);
    let qy = (py - cy).abs() - (hh - radius);
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - radius
}

/// 1px feather centered on the true edge (d = 0): full coverage at d <= -0.5, none at
/// d >= 0.5.
fn coverage_from_sdf(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

fn box_blur_axis(buf: &[f32], w: i32, h: i32, radius: i32, horizontal: bool) -> Vec<f32> {
    let mut out = vec![0f32; buf.len()];
    let count = (2 * radius + 1) as f32;
    if horizontal {
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let mut sum = 0f32;
                for dx in -radius..=radius {
                    let xx = (x + dx).clamp(0, w - 1);
                    sum += buf[(row + xx) as usize];
                }
                out[(row + x) as usize] = sum / count;
            }
        }
    } else {
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0f32;
                for dy in -radius..=radius {
                    let yy = (y + dy).clamp(0, h - 1);
                    sum += buf[(yy * w + x) as usize];
                }
                out[(y * w + x) as usize] = sum / count;
            }
        }
    }
    out
}

fn lerp8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
}

/// Translation box: shadow, surface fill (97%), 1px border (white 8%, accent 100% on
/// hover), text in `theme.text` or secondary (failed translation) at `box_padding`.
#[allow(clippy::too_many_arguments)]
pub fn translation_box(
    cv: &mut Canvas,
    theme: &Theme,
    r: Rect,
    font: &Font,
    font_px: f32,
    lines: &[String],
    secondary: bool,
    hover: bool,
) {
    cv.shadow(r, RADIUS_BOX, SHADOW_OFFSET_Y, SHADOW_BLUR, SHADOW_ALPHA);
    cv.fill_rounded(r, RADIUS_BOX, theme.surface, SURFACE_ALPHA);
    if hover {
        cv.stroke_rounded(r, RADIUS_BOX, BORDER_WIDTH, theme.accent, 1.0);
    } else {
        cv.stroke_rounded(r, RADIUS_BOX, BORDER_WIDTH, SURFACE_BORDER, SURFACE_BORDER_ALPHA);
    }
    let (pad_x, pad_y) = box_padding(font_px);
    let color = if secondary { theme.text_secondary() } else { theme.text };
    cv.text(font, font_px, lines, r.x + pad_x, r.y + pad_y, font.line_height(font_px), color);
}

/// Highlight over an original text block: accent 22%/70% fill+stroke (34%/100% hover),
/// radius 4, inflated by 3px.
pub fn highlight(cv: &mut Canvas, theme: &Theme, block: Rect, hover: bool) {
    let r = highlight_bounds(block);
    let (fill_alpha, stroke_alpha) = if hover {
        (HIGHLIGHT_FILL_ALPHA_HOVER, HIGHLIGHT_STROKE_ALPHA_HOVER)
    } else {
        (HIGHLIGHT_FILL_ALPHA, HIGHLIGHT_STROKE_ALPHA)
    };
    cv.fill_rounded(r, RADIUS_HIGHLIGHT, theme.accent, fill_alpha);
    cv.stroke_rounded(r, RADIUS_HIGHLIGHT, BORDER_WIDTH, theme.accent, stroke_alpha);
}

/// `block` inflated by the highlight's 3px margin (for hit-test/background fetch).
pub fn highlight_bounds(block: Rect) -> Rect {
    Rect {
        x: block.x - HIGHLIGHT_INFLATE,
        y: block.y - HIGHLIGHT_INFLATE,
        w: block.w + 2 * HIGHLIGHT_INFLATE,
        h: block.h + 2 * HIGHLIGHT_INFLATE,
    }
}

/// Size of a pill chip holding a single line of `text` at `px`.
pub fn chip_size(font: &Font, px: f32, text: &str) -> (i32, i32) {
    let w = font.text_width(px, text).ceil() as i32 + 2 * CHIP_PAD_X;
    let h = font.line_height(px).ceil() as i32 + 2 * CHIP_PAD_Y;
    (w, h)
}

/// Surface pill, radius 8, padding x12/y6. `accent_border` adds a 1px accent border and
/// switches the text to `theme.text`; otherwise text is secondary with no border.
#[allow(clippy::too_many_arguments)]
pub fn chip(cv: &mut Canvas, theme: &Theme, r: Rect, font: &Font, px: f32, text: &str, accent_border: bool) {
    cv.fill_rounded(r, RADIUS_BOX, theme.surface, SURFACE_ALPHA);
    let color = if accent_border {
        cv.stroke_rounded(r, RADIUS_BOX, BORDER_WIDTH, theme.accent, 1.0);
        theme.text
    } else {
        theme.text_secondary()
    };
    let lines = [text.to_string()];
    cv.text(font, px, &lines, r.x + CHIP_PAD_X, r.y + CHIP_PAD_Y, font.line_height(px), color);
}

/// Padding inside a translation box: x = max(14, round(font*0.6)), y = round(font*0.45).
pub fn box_padding(font_px: f32) -> (i32, i32) {
    let x = ((font_px * 0.6).round() as i32).max(14);
    let y = (font_px * 0.45).round() as i32;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(cv: &Canvas, x: i32, y: i32) -> Rgb {
        let idx = ((y * cv.w + x) * 3) as usize;
        (cv.px[idx], cv.px[idx + 1], cv.px[idx + 2])
    }

    #[test]
    fn fill_rounded_leaves_corner_untouched_fills_center() {
        let mut cv = Canvas::new((0, 0), 20, 20, vec![255u8; 20 * 20 * 3]);
        cv.fill_rounded(Rect { x: 0, y: 0, w: 20, h: 20 }, 4.0, (0, 0, 0), 1.0);
        // The extreme corner pixel sits outside the radius-4 arc: must stay background.
        assert_eq!(pixel(&cv, 0, 0), (255, 255, 255));
        // The center is well inside the rounded rect: fully painted.
        assert_eq!(pixel(&cv, 10, 10), (0, 0, 0));
    }

    #[test]
    fn fill_clips_off_canvas_rect_in_screen_coords() {
        let mut cv = Canvas::new((100, 100), 10, 10, vec![200u8; 10 * 10 * 3]);
        // Rect mostly off the canvas's top-left; only its bottom-right 4x4 corner overlaps.
        cv.fill_rounded(Rect { x: 94, y: 94, w: 10, h: 10 }, 0.0, (0, 0, 0), 1.0);
        assert_eq!(pixel(&cv, 0, 0), (0, 0, 0));
        assert_eq!(pixel(&cv, 2, 2), (0, 0, 0));
        // Outside the overlap, the original background is untouched.
        assert_eq!(pixel(&cv, 6, 6), (200, 200, 200));
    }

    #[test]
    fn shadow_appears_below_offset_not_above() {
        let mut cv = Canvas::new((0, 0), 50, 50, vec![255u8; 50 * 50 * 3]);
        let r = Rect { x: 10, y: 10, w: 20, h: 20 };
        cv.shadow(r, 4.0, 10, 4.0, 0.9);
        // Above the un-shifted rect's top edge, far from the offset+blurred shadow.
        let above = pixel(&cv, 20, 5);
        assert!(above.0 > 250, "expected untouched pixel above shadow, got {above:?}");
        // Inside the shadow after the y-offset shift: darkened.
        let below = pixel(&cv, 20, 30);
        assert!(below.0 < 50, "expected darkened pixel below shadow, got {below:?}");
    }
}
