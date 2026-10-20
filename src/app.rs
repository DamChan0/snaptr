//! X11 overlay: freeze the screen, let the user drag (or click, or `--full`) to pick a
//! region, run OCR + translation on it, and render clickable highlight/translation boxes
//! on top of the frozen screenshot until the user exits.
//!
//! Two server-side copies of the frozen screen exist: CLEAR and DIM (CLEAR darkened with one
//! XRender FillRectangles). The window background is DIM; the selection shows CLEAR. Every
//! painted element is composited client-side over pixels fetched from those pixmaps (never
//! from the window, so repaints never accumulate) and PutImage'd back.

use crate::font::Font;
use crate::io;
use crate::logic::{self, OcrBlock, Rect};
use crate::paint::{self, Canvas, Rgb, ACCENT, GAP, PAINT_MARGIN, SPACE_16, SPACE_24, SPACE_8};

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::image::{Image, ImageOrder, PixelLayout};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::render::{self, ConnectionExt as _, CreatePictureAux, PictOp};
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor, EventMask, Gcontext, GrabMode,
    GrabStatus, Keycode, ModMask, Pixmap, Rectangle, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const DIM_ALPHA: f32 = 0.45;
const OUTLINE_DARK: Rgb = (0x0C, 0x0C, 0x0E);
const MIN_FONT_PX: i32 = 13;
const MAX_FONT_PX: i32 = 56;
const MIN_BOX_W: i32 = 240;
const CLICK_THRESHOLD: i32 = 5;
const COPIED_FOR: Duration = Duration::from_millis(1200);
const FADE_FRAMES: u32 = 7;
const FADE_FRAME: Duration = Duration::from_millis(20);
const FADE_FROM: f32 = 0.3;

const XK_ESCAPE: u32 = 0xff1b;
const XK_RETURN: u32 = 0xff0d;
const XK_KP_ENTER: u32 = 0xff8d;
const XK_C: u32 = 0x0063;
const CURSOR_CROSSHAIR: u16 = 34;
const CURSOR_HAND: u16 = 60;

const HINT_TEXT: &str = "드래그해서 영역 선택 · 클릭하면 화면 전체 · Esc 닫기";
const READING_TEXT: &str = "글자 읽는 중";
const EMPTY_TEXT: &str = "글자를 찾지 못했어요 · 다시 드래그하거나 Esc";
const ALL_FAILED_TEXT: &str = "번역 실패 · 네트워크를 확인하세요";
const FAILED_LINE: &str = "번역 실패";
const COPIED_TEXT: &str = "복사됨";
const COPIED_ALL_TEXT: &str = "복사됨 · 전체";

/// Entry point: run one capture-OCR-translate-interact cycle (repeatable by re-dragging).
pub fn run(full: bool, to: &str, ocr_langs: &str) -> Result<(), String> {
    let font = Font::load(to).map_err(|e| format!("loading font for '{to}': {e}"))?;
    let mut ui = Ui::new(font)?;
    let result = ui.session(full, to, ocr_langs);
    ui.cleanup();
    result
}

enum Kind {
    Highlight,
    Box { lines: Vec<String>, font_px: f32, failed: bool },
    Chip { text: String, accent: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Content,
    Hint,
    SizeReadout,
    Status,
    Copied,
}

struct Element {
    kind: Kind,
    role: Role,
    /// Block rect for highlights, box/chip rect otherwise (screen coordinates).
    rect: Rect,
    copy: Option<String>,
    expires: Option<Instant>,
}

impl Element {
    fn chip(role: Role, rect: Rect, text: &str, accent: bool) -> Element {
        Element {
            kind: Kind::Chip { text: text.to_string(), accent },
            role,
            rect,
            copy: None,
            expires: None,
        }
    }

    fn hit_rect(&self) -> Rect {
        match self.kind {
            Kind::Highlight => paint::highlight_bounds(self.rect),
            _ => self.rect,
        }
    }

    /// Screen area this element's painting touches.
    fn bounds(&self) -> Rect {
        match self.kind {
            Kind::Box { .. } => inflate(self.rect, PAINT_MARGIN),
            _ => inflate(self.hit_rect(), 2),
        }
    }

    fn paint(&self, cv: &mut Canvas, font: &Font, chip_px: f32, hover: bool) {
        match &self.kind {
            Kind::Highlight => paint::highlight(cv, self.rect, hover),
            Kind::Box { lines, font_px, failed } => {
                paint::translation_box(cv, self.rect, font, *font_px, lines, *failed, hover)
            }
            Kind::Chip { text, accent } => paint::chip(cv, self.rect, font, chip_px, text, *accent),
        }
    }
}

struct Keys {
    escape: Option<Keycode>,
    enter: [Option<Keycode>; 2],
    c: Option<Keycode>,
}

enum Selection {
    Region(Rect, Rect),
    Cancelled,
}

enum AfterResult {
    Exit,
    Redrag((i32, i32)),
}

struct Ui {
    conn: RustConnection,
    win: Window,
    gc: Gcontext,
    line_gc: Gcontext,
    layout: PixelLayout,
    depth: u8,
    clear: Pixmap,
    dim: Pixmap,
    root: Rect,
    monitors: Vec<Rect>,
    font: Font,
    chip_px: f32,
    keys: Keys,
    crosshair: Cursor,
    hand: Cursor,
    /// Area currently shown undimmed (from CLEAR).
    sel: Option<Rect>,
    /// Draw the selection outline around `sel` (off for whole-monitor selections).
    outline: bool,
    elements: Vec<Element>,
    hover: Option<usize>,
}

impl Ui {
    fn new(font: Font) -> Result<Ui, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("connecting to X server: {e}"))?;
        let screen = conn.setup().roots[screen_num].clone();
        let root = screen.root;
        let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
        let root_rect = Rect { x: 0, y: 0, w: i32::from(w), h: i32::from(h) };
        let visual = screen
            .allowed_depths
            .iter()
            .find_map(|d| d.visuals.iter().find(|v| v.visual_id == screen.root_visual).copied())
            .ok_or("root visual not found")?;
        let layout = PixelLayout::from_visual_type(visual).map_err(|e| format!("unsupported root visual: {e:?}"))?;

        let id = |conn: &RustConnection| conn.generate_id().map_err(|e| format!("allocating X id: {e}"));
        let gc = id(&conn)?;
        conn.create_gc(gc, root, &CreateGCAux::new().graphics_exposures(0)).map_err(|e| e.to_string())?;
        let clear = id(&conn)?;
        let dim = id(&conn)?;
        for pm in [clear, dim] {
            conn.create_pixmap(screen.root_depth, pm, root, w, h).map_err(|e| e.to_string())?;
            conn.copy_area(root, pm, gc, 0, 0, 0, 0, w, h).map_err(|e| e.to_string())?;
        }
        dim_pixmap(&conn, screen_num, screen.root_visual, dim, w, h);

        let crosshair = glyph_cursor(&conn, CURSOR_CROSSHAIR)?;
        let hand = glyph_cursor(&conn, CURSOR_HAND)?;

        let win = id(&conn)?;
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixmap(dim)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(crosshair);
        conn.create_window(screen.root_depth, win, root, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, 0, &aux)
            .map_err(|e| format!("creating overlay window: {e}"))?;
        conn.map_window(win).map_err(|e| e.to_string())?;

        let line_gc = id(&conn)?;
        conn.create_gc(line_gc, win, &CreateGCAux::new().graphics_exposures(0).line_width(1))
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        grab_input(&conn, win, crosshair);
        let keys = Keys {
            escape: keycode_for(&conn, XK_ESCAPE),
            enter: [keycode_for(&conn, XK_RETURN), keycode_for(&conn, XK_KP_ENTER)],
            c: keycode_for(&conn, XK_C),
        };
        let chip_px = 14.0 * xft_dpi(&conn, root) / 96.0;
        let monitors = query_monitors(&conn, root, root_rect);

        Ok(Ui {
            conn,
            win,
            gc,
            line_gc,
            layout,
            depth: screen.root_depth,
            clear,
            dim,
            root: root_rect,
            monitors,
            font,
            chip_px,
            keys,
            crosshair,
            hand,
            sel: None,
            outline: false,
            elements: Vec::new(),
            hover: None,
        })
    }

    fn session(&mut self, full: bool, to: &str, ocr_langs: &str) -> Result<(), String> {
        let mut start = None;
        let mut first = true;
        loop {
            let (sel, monitor) = match self.select(full && first, first, start)? {
                Selection::Cancelled => return Ok(()),
                Selection::Region(sel, monitor) => (sel, monitor),
            };
            first = false;
            self.process(sel, monitor, to, ocr_langs);
            match self.interact(sel, monitor)? {
                AfterResult::Exit => return Ok(()),
                AfterResult::Redrag(p) => {
                    self.reset();
                    start = Some(p);
                }
            }
        }
    }

    // ---- selection -----------------------------------------------------------------

    fn select(&mut self, full: bool, show_hint: bool, mut start: Option<(i32, i32)>) -> Result<Selection, String> {
        if full {
            let (x, y) = self.pointer();
            let m = self.monitor_at(x, y);
            return Ok(Selection::Region(m, m));
        }
        if show_hint {
            let (x, y) = self.pointer();
            let m = self.monitor_at(x, y);
            let (w, h) = paint::chip_size(&self.font, self.chip_px, HINT_TEXT);
            let r = Rect { x: m.x + (m.w - w) / 2, y: m.bottom() - SPACE_24 - h, w, h };
            self.add(Element::chip(Role::Hint, r, HINT_TEXT, false));
            self.flush();
        }
        loop {
            let event = self.conn.wait_for_event().map_err(|e| format!("waiting for event: {e}"))?;
            match event {
                Event::Expose(e) if e.count == 0 => self.redraw_all(),
                Event::KeyPress(e) if Some(e.detail) == self.keys.escape => return Ok(Selection::Cancelled),
                Event::KeyPress(e) if start.is_none() && self.keys.enter.contains(&Some(e.detail)) => {
                    self.remove(Role::Hint);
                    let (x, y) = self.pointer();
                    let m = self.monitor_at(x, y);
                    return Ok(Selection::Region(m, m));
                }
                Event::ButtonPress(e) if e.detail == 3 => return Ok(Selection::Cancelled),
                Event::ButtonPress(e) if e.detail == 1 && start.is_none() => {
                    self.remove(Role::Hint);
                    start = Some((i32::from(e.event_x), i32::from(e.event_y)));
                    self.flush();
                }
                Event::MotionNotify(e) => {
                    if let Some(s) = start {
                        let r = rect_from_points(s, (i32::from(e.event_x), i32::from(e.event_y)));
                        self.set_selection(r, true);
                        let text = format!("{} × {}", r.w, r.h);
                        let m = self.monitor_at(r.x + r.w / 2, r.y + r.h / 2);
                        let chip = self.anchored_chip(Role::SizeReadout, r, m, &text, false);
                        self.replace(chip);
                        self.flush();
                    }
                }
                Event::ButtonRelease(e) if e.detail == 1 => {
                    if let Some(s) = start {
                        self.remove(Role::SizeReadout);
                        let end = (i32::from(e.event_x), i32::from(e.event_y));
                        if (end.0 - s.0).abs() < CLICK_THRESHOLD && (end.1 - s.1).abs() < CLICK_THRESHOLD {
                            let m = self.monitor_at(end.0, end.1);
                            return Ok(Selection::Region(m, m));
                        }
                        let r = rect_from_points(s, end).intersect(self.root);
                        let m = self.monitor_at(r.x + r.w / 2, r.y + r.h / 2);
                        return Ok(Selection::Region(r, m));
                    }
                }
                _ => {}
            }
        }
    }

    /// Show `r` undimmed (restoring the previously shown area to dim) and outline it.
    fn set_selection(&mut self, r: Rect, outline: bool) {
        let old = self.sel.replace(r);
        if let Some(o) = old {
            let o = inflate(o, 2);
            for part in subtract(o, r) {
                self.copy(self.dim, part);
            }
            // Elements under the erased outline band need repainting.
            let damaged: Vec<Rect> =
                self.elements.iter().map(Element::bounds).filter(|b| b.overlaps(&o)).collect();
            for b in damaged {
                self.draw_region(b);
            }
        }
        match old {
            Some(o) => {
                for part in subtract(r, o) {
                    self.copy(self.clear, part);
                }
            }
            None => self.copy(self.clear, r),
        }
        self.outline = outline;
        self.draw_outline();
    }

    fn draw_outline(&self) {
        let Some(r) = self.sel.filter(|_| self.outline) else { return };
        let line = |gc_color: Rgb, inset: i32| {
            let _ = self.conn.change_gc(
                self.line_gc,
                &x11rb::protocol::xproto::ChangeGCAux::new().foreground(encode_rgb(self.layout, gc_color)),
            );
            let o = inflate(r, inset);
            let _ = self.conn.poly_rectangle(
                self.win,
                self.line_gc,
                &[Rectangle { x: o.x as i16, y: o.y as i16, width: (o.w - 1).max(0) as u16, height: (o.h - 1).max(0) as u16 }],
            );
        };
        line(OUTLINE_DARK, 2);
        line(ACCENT, 1);
    }

    // ---- OCR + translation ---------------------------------------------------------

    fn process(&mut self, sel: Rect, monitor: Rect, to: &str, ocr_langs: &str) {
        let whole_monitor = sel == monitor;
        self.set_selection(sel, !whole_monitor);
        let status = self.anchored_chip(Role::Status, sel, monitor, READING_TEXT, false);
        self.replace(status);
        self.flush();

        let rgb = self.fetch_rgb(self.clear, sel);
        let blocks = match io::ocr(&rgb, sel.w as u32, sel.h as u32, ocr_langs) {
            Ok(b) => b,
            Err(e) => {
                let text = if e.starts_with("tesseract가") { e } else { format!("글자 인식 실패 · {e}") };
                let chip = self.anchored_chip(Role::Status, sel, monitor, &text, true);
                self.replace(chip);
                self.flush();
                return;
            }
        };
        if blocks.is_empty() {
            let chip = self.anchored_chip(Role::Status, sel, monitor, EMPTY_TEXT, true);
            self.replace(chip);
            self.flush();
            return;
        }

        let text = format!("번역 중 · {}개 블록", blocks.len());
        let chip = self.anchored_chip(Role::Status, sel, monitor, &text, false);
        self.replace(chip);
        self.flush();

        let translations = translate_all(&blocks, to);
        let all_failed = translations.iter().all(Result::is_err);
        self.remove(Role::Status);
        let new = self.layout_blocks(sel, &blocks, translations);
        let first_new = self.elements.len();
        self.elements.extend(new);
        if all_failed {
            let chip = self.anchored_chip(Role::Status, sel, monitor, ALL_FAILED_TEXT, true);
            self.elements.push(chip);
        }
        self.reveal(first_new);
    }

    fn layout_blocks(&self, sel: Rect, blocks: &[OcrBlock], translations: Vec<Result<String, String>>) -> Vec<Element> {
        let screen_rects: Vec<Rect> = blocks
            .iter()
            .map(|b| Rect { x: sel.x + b.rect.x, y: sel.y + b.rect.y, w: b.rect.w, h: b.rect.h })
            .collect();
        let mut highlights = Vec::new();
        let mut boxes: Vec<Element> = Vec::new();
        for ((i, block), translation) in blocks.iter().enumerate().zip(translations) {
            let block_rect = screen_rects[i];
            highlights.push(Element {
                kind: Kind::Highlight,
                role: Role::Content,
                rect: block_rect,
                copy: Some(block.text.clone()),
                expires: None,
            });

            let monitor = self.monitor_at(block_rect.x + block_rect.w / 2, block_rect.y + block_rect.h / 2);
            let box_w = block_rect.w.clamp(MIN_BOX_W, (monitor.w - 2 * SPACE_16).max(MIN_BOX_W));
            let preferred = ((block.avg_line_height * 1.05).round() as i32).clamp(MIN_FONT_PX, MAX_FONT_PX);
            let failed = translation.is_err();
            let text = translation.unwrap_or_else(|_| block.text.clone());
            let font = &self.font;
            let wrap = |px: f32| {
                let (pad_x, _) = paint::box_padding(px);
                let mut lines = if failed { vec![FAILED_LINE.to_string()] } else { Vec::new() };
                lines.extend(logic::wrap_text(&text, (box_w - 2 * pad_x) as f32, |c| font.char_width(px, c)));
                lines
            };
            let height_for_font = |fp: i32| {
                let px = fp as f32;
                let (_, pad_y) = paint::box_padding(px);
                (wrap(px).len() as f32 * font.line_height(px)).ceil() as i32 + 2 * pad_y
            };
            let mut avoid: Vec<Rect> = screen_rects
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| paint::highlight_bounds(*r))
                .collect();
            avoid.extend(boxes.iter().map(|b| b.rect));
            let placed = logic::place_box(
                paint::highlight_bounds(block_rect),
                monitor,
                box_w,
                preferred,
                MIN_FONT_PX,
                GAP,
                &avoid,
                height_for_font,
            );
            let px = placed.font_px as f32;
            boxes.push(Element {
                kind: Kind::Box { lines: wrap(px), font_px: px, failed },
                role: Role::Content,
                rect: placed.rect,
                copy: Some(if failed { block.text.clone() } else { text.clone() }),
                expires: None,
            });
        }
        highlights.extend(boxes);
        highlights
    }

    /// Fade the elements from index `first` in: one short exponential ease-out from 30%.
    fn reveal(&mut self, first: usize) {
        let regions: Vec<Rect> = self.elements[first..]
            .iter()
            .filter_map(|e| e.bounds().intersect_opt(self.root))
            .collect();
        let frames: Vec<(Canvas, Canvas)> = regions
            .iter()
            .map(|r| {
                let bg = self.background(*r);
                let mut done = Canvas::new(bg.origin, bg.w, bg.h, bg.px.clone());
                self.paint_elements(&mut done, *r);
                (bg, done)
            })
            .collect();
        for frame in 1..=FADE_FRAMES {
            let t = frame as f32 / FADE_FRAMES as f32;
            let eased = if frame == FADE_FRAMES { 1.0 } else { 1.0 - 2f32.powf(-10.0 * t) };
            let a = FADE_FROM + (1.0 - FADE_FROM) * eased;
            for (bg, done) in &frames {
                let px: Vec<u8> = bg.px.iter().zip(&done.px).map(|(&b, &d)| lerp8(b, d, a)).collect();
                self.put(&Canvas::new(bg.origin, bg.w, bg.h, px));
            }
            self.draw_outline();
            self.flush();
            if frame < FADE_FRAMES {
                std::thread::sleep(FADE_FRAME);
            }
        }
    }

    // ---- result interaction --------------------------------------------------------

    fn interact(&mut self, sel: Rect, monitor: Rect) -> Result<AfterResult, String> {
        loop {
            self.expire_elements();
            let deadline = self.elements.iter().filter_map(|e| e.expires).min();
            let event = match deadline {
                None => self.conn.wait_for_event().map_err(|e| format!("waiting for event: {e}"))?,
                Some(d) => match self.conn.poll_for_event().map_err(|e| format!("polling event: {e}"))? {
                    Some(ev) => ev,
                    None => {
                        let wait = d.saturating_duration_since(Instant::now()).min(Duration::from_millis(16));
                        std::thread::sleep(wait);
                        continue;
                    }
                },
            };
            match event {
                Event::Expose(e) if e.count == 0 => self.redraw_all(),
                Event::KeyPress(e) if Some(e.detail) == self.keys.escape => return Ok(AfterResult::Exit),
                Event::KeyPress(e) if Some(e.detail) == self.keys.c && u16::from(e.state) & u16::from(ModMask::CONTROL) != 0 => {
                    let all: Vec<&str> = self
                        .elements
                        .iter()
                        .filter(|el| matches!(el.kind, Kind::Box { .. }))
                        .filter_map(|el| el.copy.as_deref())
                        .collect();
                    if !all.is_empty() && io::copy_to_clipboard(&all.join("\n\n")).is_ok() {
                        let mut chip = self.anchored_chip(Role::Copied, sel, monitor, COPIED_ALL_TEXT, true);
                        chip.expires = Some(Instant::now() + COPIED_FOR);
                        self.replace(chip);
                        self.flush();
                    }
                }
                Event::ButtonPress(e) if e.detail == 3 => return Ok(AfterResult::Exit),
                Event::ButtonPress(e) if e.detail == 1 => {
                    let (x, y) = (i32::from(e.event_x), i32::from(e.event_y));
                    match self.target_at(x, y) {
                        Some(i) => self.copy_element(i),
                        None => return Ok(AfterResult::Redrag((x, y))),
                    }
                }
                Event::MotionNotify(e) => self.set_hover(self.target_at(i32::from(e.event_x), i32::from(e.event_y))),
                _ => {}
            }
        }
    }

    /// Topmost copyable element under the point.
    fn target_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.elements.len())
            .rev()
            .find(|&i| self.elements[i].copy.is_some() && self.elements[i].hit_rect().contains_point(x, y))
    }

    fn set_hover(&mut self, new: Option<usize>) {
        if new == self.hover {
            return;
        }
        let old = std::mem::replace(&mut self.hover, new);
        for i in [old, new].into_iter().flatten() {
            self.draw_region(self.elements[i].bounds());
        }
        let cursor = if new.is_some() { self.hand } else { self.crosshair };
        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
        let _ = self.conn.change_active_pointer_grab(cursor, x11rb::CURRENT_TIME, mask);
        self.draw_outline();
        self.flush();
    }

    fn copy_element(&mut self, i: usize) {
        let Some(text) = self.elements[i].copy.clone() else { return };
        if io::copy_to_clipboard(&text).is_err() {
            return;
        }
        let target = self.elements[i].hit_rect();
        let (w, h) = paint::chip_size(&self.font, self.chip_px, COPIED_TEXT);
        let y = match self.elements[i].kind {
            // Badge straddling the box's top edge.
            Kind::Box { .. } => target.y - h / 2,
            _ => target.y - h - 4,
        };
        let x = target.right() - w - SPACE_8;
        let mut chip = Element::chip(Role::Copied, Rect { x, y, w, h }.clamp_into(self.root), COPIED_TEXT, true);
        chip.expires = Some(Instant::now() + COPIED_FOR);
        self.replace(chip);
        self.flush();
    }

    fn expire_elements(&mut self) {
        let now = Instant::now();
        if !self.elements.iter().any(|e| e.expires.is_some_and(|t| t <= now)) {
            return;
        }
        let gone: Vec<Rect> = self
            .elements
            .iter()
            .filter(|e| e.expires.is_some_and(|t| t <= now))
            .map(Element::bounds)
            .collect();
        self.elements.retain(|e| e.expires.is_none_or(|t| t > now));
        self.hover = None;
        for b in gone {
            self.draw_region(b);
        }
        self.draw_outline();
        self.flush();
    }

    fn reset(&mut self) {
        self.elements.clear();
        self.hover = None;
        self.sel = None;
        self.outline = false;
        let _ = self.conn.clear_area(false, self.win, 0, 0, 0, 0);
        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
        let _ = self.conn.change_active_pointer_grab(self.crosshair, x11rb::CURRENT_TIME, mask);
        self.flush();
    }

    // ---- element bookkeeping -------------------------------------------------------

    /// Chip placed 8 px below `anchor`'s bottom-left, flipped above if there is no room,
    /// inside the bottom-left corner if neither fits (whole-monitor selection).
    fn anchored_chip(&self, role: Role, anchor: Rect, monitor: Rect, text: &str, accent: bool) -> Element {
        let (w, h) = paint::chip_size(&self.font, self.chip_px, text);
        let y = if anchor.bottom() + GAP + h <= monitor.bottom() {
            anchor.bottom() + GAP
        } else if anchor.y - GAP - h >= monitor.y {
            anchor.y - GAP - h
        } else {
            anchor.bottom() - SPACE_24 - h
        };
        let x = if anchor == monitor { anchor.x + SPACE_24 } else { anchor.x };
        let r = Rect { x, y, w, h }.clamp_into(monitor);
        Element::chip(role, r, text, accent)
    }

    fn add(&mut self, e: Element) {
        let b = e.bounds();
        self.elements.push(e);
        self.draw_region(b);
    }

    fn remove(&mut self, role: Role) {
        let gone: Vec<Rect> = self.elements.iter().filter(|e| e.role == role).map(Element::bounds).collect();
        if gone.is_empty() {
            return;
        }
        self.elements.retain(|e| e.role != role);
        self.hover = None;
        for b in gone {
            self.draw_region(b);
        }
        self.draw_outline();
    }

    fn replace(&mut self, e: Element) {
        self.remove(e.role);
        self.add(e);
        self.draw_outline();
    }

    fn redraw_all(&mut self) {
        if let Some(s) = self.sel {
            self.copy(self.clear, s);
        }
        let all: Vec<Rect> = self.elements.iter().map(Element::bounds).collect();
        for b in all {
            self.draw_region(b);
        }
        self.draw_outline();
        self.flush();
    }

    // ---- pixels --------------------------------------------------------------------

    fn draw_region(&self, r: Rect) {
        let Some(r) = r.intersect_opt(self.root) else { return };
        let mut cv = self.background(r);
        self.paint_elements(&mut cv, r);
        self.put(&cv);
    }

    fn paint_elements(&self, cv: &mut Canvas, r: Rect) {
        for (i, e) in self.elements.iter().enumerate() {
            if e.bounds().overlaps(&r) {
                e.paint(cv, &self.font, self.chip_px, self.hover == Some(i));
            }
        }
    }

    /// Frozen-screen pixels as currently shown: DIM, with CLEAR inside the selection.
    fn background(&self, r: Rect) -> Canvas {
        let mut px = self.fetch_rgb(self.dim, r);
        if let Some(inner) = self.sel.and_then(|s| s.intersect_opt(r)) {
            let clear = self.fetch_rgb(self.clear, inner);
            let row = (inner.w * 3) as usize;
            for y in 0..inner.h {
                let dst = (((inner.y - r.y + y) * r.w + (inner.x - r.x)) * 3) as usize;
                let src = (y * inner.w * 3) as usize;
                px[dst..dst + row].copy_from_slice(&clear[src..src + row]);
            }
        }
        Canvas::new((r.x, r.y), r.w, r.h, px)
    }

    fn fetch_rgb(&self, pm: Pixmap, r: Rect) -> Vec<u8> {
        let n = (r.w * r.h * 3) as usize;
        let Ok((img, _)) = Image::get(&self.conn, pm, r.x as i16, r.y as i16, r.w as u16, r.h as u16) else {
            return vec![0; n];
        };
        let mut out = Vec::with_capacity(n);
        if self.is_bgrx(&img) {
            let stride = img.data().len() / usize::from(img.height());
            for row in img.data().chunks_exact(stride) {
                for p in row[..r.w as usize * 4].chunks_exact(4) {
                    out.extend_from_slice(&[p[2], p[1], p[0]]);
                }
            }
        } else {
            for y in 0..img.height() {
                for x in 0..img.width() {
                    let (red, green, blue) = self.layout.decode(img.get_pixel(x, y));
                    out.extend_from_slice(&[(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8]);
                }
            }
        }
        out
    }

    fn put(&self, cv: &Canvas) {
        let (w, h) = (cv.w as u16, cv.h as u16);
        let Ok(mut img) = Image::allocate_native(w, h, self.depth, self.conn.setup()) else { return };
        if self.is_bgrx(&img) {
            let stride = img.data().len() / usize::from(h);
            let data = img.data_mut();
            for (y, src) in cv.px.chunks_exact(cv.w as usize * 3).enumerate() {
                let row = &mut data[y * stride..];
                for (x, p) in src.chunks_exact(3).enumerate() {
                    row[x * 4..x * 4 + 4].copy_from_slice(&[p[2], p[1], p[0], 0xff]);
                }
            }
        } else {
            for (i, p) in cv.px.chunks_exact(3).enumerate() {
                let (x, y) = ((i % cv.w as usize) as u16, (i / cv.w as usize) as u16);
                img.put_pixel(x, y, encode_rgb(self.layout, (p[0], p[1], p[2])));
            }
        }
        let _ = img.put(&self.conn, self.win, self.gc, cv.origin.0 as i16, cv.origin.1 as i16);
    }

    /// Common 32-bpp little-endian xRGB visual: lets pixels be copied as bytes.
    fn is_bgrx(&self, img: &Image) -> bool {
        img.byte_order() == ImageOrder::LsbFirst
            && img.data().len() >= usize::from(img.width()) * usize::from(img.height()) * 4
            && self.layout.encode((0xffff, 0, 0)) == 0x00ff_0000
            && self.layout.encode((0, 0, 0xffff)) == 0x0000_00ff
    }

    fn copy(&self, src: Pixmap, r: Rect) {
        let Some(r) = r.intersect_opt(self.root) else { return };
        let _ = self.conn.copy_area(src, self.win, self.gc, r.x as i16, r.y as i16, r.x as i16, r.y as i16, r.w as u16, r.h as u16);
    }

    fn pointer(&self) -> (i32, i32) {
        self.conn
            .query_pointer(self.win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or((0, 0), |r| (i32::from(r.root_x), i32::from(r.root_y)))
    }

    fn monitor_at(&self, x: i32, y: i32) -> Rect {
        self.monitors.iter().find(|m| m.contains_point(x, y)).copied().unwrap_or(self.root)
    }

    fn flush(&self) {
        let _ = self.conn.flush();
    }

    fn cleanup(&self) {
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.free_pixmap(self.clear);
        let _ = self.conn.free_pixmap(self.dim);
        self.flush();
    }
}

impl Rect {
    fn intersect_opt(self, o: Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        (r > x && b > y).then_some(Rect { x, y, w: r - x, h: b - y })
    }

    fn intersect(self, o: Rect) -> Rect {
        self.intersect_opt(o).unwrap_or(Rect { x: o.x, y: o.y, w: 1, h: 1 })
    }

    fn clamp_into(self, o: Rect) -> Rect {
        Rect {
            x: self.x.clamp(o.x, (o.right() - self.w).max(o.x)),
            y: self.y.clamp(o.y, (o.bottom() - self.h).max(o.y)),
            ..self
        }
    }
}

fn inflate(r: Rect, d: i32) -> Rect {
    Rect { x: r.x - d, y: r.y - d, w: r.w + 2 * d, h: r.h + 2 * d }
}

/// `a` minus `b` as up to four non-overlapping rects.
fn subtract(a: Rect, b: Rect) -> Vec<Rect> {
    let Some(i) = a.intersect_opt(b) else { return vec![a] };
    let parts = [
        Rect { x: a.x, y: a.y, w: a.w, h: i.y - a.y },
        Rect { x: a.x, y: i.bottom(), w: a.w, h: a.bottom() - i.bottom() },
        Rect { x: a.x, y: i.y, w: i.x - a.x, h: i.h },
        Rect { x: i.right(), y: i.y, w: a.right() - i.right(), h: i.h },
    ];
    parts.into_iter().filter(|r| r.w > 0 && r.h > 0).collect()
}

fn rect_from_points(a: (i32, i32), b: (i32, i32)) -> Rect {
    Rect { x: a.0.min(b.0), y: a.1.min(b.1), w: (a.0 - b.0).abs().max(1), h: (a.1 - b.1).abs().max(1) }
}

fn lerp8(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
}

fn encode_rgb(layout: PixelLayout, (r, g, b): Rgb) -> u32 {
    let up = |v: u8| (u16::from(v) << 8) | u16::from(v);
    layout.encode((up(r), up(g), up(b)))
}

/// Darken `pm` in place with XRender. Without RENDER the overlay just stays undimmed.
fn dim_pixmap(conn: &RustConnection, screen_num: usize, visual: u32, pm: Pixmap, w: u16, h: u16) {
    let Ok(Ok(formats)) = conn.render_query_pict_formats().map(|c| c.reply()) else { return };
    let Some(format) = formats.screens.get(screen_num).and_then(|s| {
        s.depths.iter().flat_map(|d| &d.visuals).find(|v| v.visual == visual).map(|v| v.format)
    }) else {
        return;
    };
    let Ok(pic) = conn.generate_id() else { return };
    if conn.render_create_picture(pic, pm, format, &CreatePictureAux::new()).is_err() {
        return;
    }
    let color = render::Color { red: 0, green: 0, blue: 0, alpha: (DIM_ALPHA * 65535.0) as u16 };
    let _ = conn.render_fill_rectangles(PictOp::OVER, pic, color, &[Rectangle { x: 0, y: 0, width: w, height: h }]);
    let _ = conn.render_free_picture(pic);
}

fn glyph_cursor(conn: &RustConnection, glyph: u16) -> Result<Cursor, String> {
    let font = conn.generate_id().map_err(|e| e.to_string())?;
    conn.open_font(font, b"cursor").map_err(|e| format!("opening cursor font: {e}"))?;
    let cursor = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_glyph_cursor(cursor, font, font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff)
        .map_err(|e| format!("creating cursor: {e}"))?;
    let _ = conn.close_font(font);
    Ok(cursor)
}

fn grab_input(conn: &RustConnection, win: Window, cursor: Cursor) {
    let deadline = Instant::now() + Duration::from_millis(200);
    let (mut pointer_ok, mut keyboard_ok) = (false, false);
    while Instant::now() < deadline && !(pointer_ok && keyboard_ok) {
        if !pointer_ok {
            let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
            pointer_ok = conn
                .grab_pointer(false, win, mask, GrabMode::ASYNC, GrabMode::ASYNC, win, cursor, x11rb::CURRENT_TIME)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.status == GrabStatus::SUCCESS);
        }
        if !keyboard_ok {
            keyboard_ok = conn
                .grab_keyboard(false, win, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.status == GrabStatus::SUCCESS);
        }
        if !(pointer_ok && keyboard_ok) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn keycode_for(conn: &RustConnection, keysym: u32) -> Option<Keycode> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let reply = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let per = usize::from(reply.keysyms_per_keycode);
    if per == 0 {
        return None;
    }
    reply.keysyms.chunks(per).position(|syms| syms.contains(&keysym)).map(|i| min + i as u8)
}

/// `Xft.dpi` from the root RESOURCE_MANAGER property (96 when unset).
fn xft_dpi(conn: &RustConnection, root: Window) -> f32 {
    use x11rb::protocol::xproto::AtomEnum;
    conn.get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| {
            String::from_utf8_lossy(&r.value)
                .lines()
                .find_map(|l| l.strip_prefix("Xft.dpi:").and_then(|v| v.trim().parse::<f32>().ok()))
        })
        .filter(|d| *d > 0.0)
        .unwrap_or(96.0)
}

fn query_monitors(conn: &RustConnection, root: Window, fallback: Rect) -> Vec<Rect> {
    let monitors: Vec<Rect> = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|reply| {
            reply
                .monitors
                .iter()
                .map(|m| Rect { x: i32::from(m.x), y: i32::from(m.y), w: i32::from(m.width), h: i32::from(m.height) })
                .collect()
        })
        .unwrap_or_default();
    if monitors.is_empty() { vec![fallback] } else { monitors }
}

fn translate_all(blocks: &[OcrBlock], to: &str) -> Vec<Result<String, String>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = blocks.iter().map(|b| scope.spawn(|| io::translate(&b.text, to))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("translation thread panicked".to_string())))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_covers_difference_exactly() {
        let a = Rect { x: 0, y: 0, w: 10, h: 10 };
        let b = Rect { x: 3, y: 4, w: 4, h: 2 };
        let parts = subtract(a, b);
        let area: i32 = parts.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, 100 - 8);
        assert!(parts.iter().all(|p| !p.overlaps(&b)));
        assert_eq!(subtract(a, Rect { x: 20, y: 20, w: 2, h: 2 }), vec![a]);
        assert!(subtract(b, a).is_empty());
    }
}
