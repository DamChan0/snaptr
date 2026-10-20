//! X11 overlay: freeze the screen, let the user drag (or click, or `--full`) to pick a
//! region, run OCR + translation on it, and render clickable highlight/translation boxes
//! on top of the frozen screenshot until the user copies something or exits.

use crate::font::Font;
use crate::io;
use crate::logic::{self, OcrBlock, PlacedBox, Rect};

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::image::{Image, PixelLayout};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, Keycode, Rectangle, Screen, Window, WindowClass,
};
use x11rb::protocol::Event;

const MARGIN: i32 = 16;
const GAP: i32 = 8;
const PADDING: i32 = 8;
const MIN_FONT_PX: i32 = 12;
const MIN_BOX_W: i32 = 240;
const CLICK_THRESHOLD: i32 = 5;
const HIGHLIGHT_ALPHA: f32 = 0.35;
const SEL_BORDER_PX: i32 = 2;
const XK_ESCAPE: u32 = 0xff1b;

/// Entry point: run one full capture-OCR-translate-interact-exit cycle.
pub fn run(full: bool, to: &str, ocr_langs: &str) -> Result<(), String> {
    // Resolved once, up front: if this fails there is nothing to draw with anyway.
    let font = Font::load(to).map_err(|e| format!("loading font for '{to}': {e}"))?;

    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| format!("connecting to X server: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let root_depth = screen.root_depth;
    let root_w = screen.width_in_pixels;
    let root_h = screen.height_in_pixels;
    let root_rect = Rect { x: 0, y: 0, w: i32::from(root_w), h: i32::from(root_h) };
    let layout = pixel_layout_for(screen)?;

    let monitors = query_monitors(&conn, root, root_rect);

    // Freeze the screen into a server-side pixmap before anything else can change it.
    let freeze_gc = conn.generate_id().map_err(|e| format!("allocating gc id: {e}"))?;
    conn.create_gc(freeze_gc, root, &CreateGCAux::new().graphics_exposures(0))
        .map_err(|e| format!("creating freeze gc: {e}"))?;
    let pixmap = conn.generate_id().map_err(|e| format!("allocating pixmap id: {e}"))?;
    conn.create_pixmap(root_depth, pixmap, root, root_w, root_h)
        .map_err(|e| format!("creating freeze pixmap: {e}"))?;
    conn.copy_area(root, pixmap, freeze_gc, 0, 0, 0, 0, root_w, root_h)
        .map_err(|e| format!("copying root to freeze pixmap: {e}"))?;
    let _ = conn.free_gc(freeze_gc);

    let cursor = create_crosshair_cursor(&conn)?;

    let win = conn.generate_id().map_err(|e| format!("allocating window id: {e}"))?;
    let win_aux = CreateWindowAux::new()
        .override_redirect(1)
        .background_pixmap(pixmap)
        .event_mask(
            EventMask::EXPOSURE
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION
                | EventMask::KEY_PRESS,
        )
        .cursor(cursor);
    conn.create_window(
        root_depth,
        win,
        root,
        0,
        0,
        root_w,
        root_h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &win_aux,
    )
    .map_err(|e| format!("creating overlay window: {e}"))?;
    conn.map_window(win).map_err(|e| format!("mapping overlay window: {e}"))?;
    conn.flush().map_err(|e| format!("flushing: {e}"))?;

    grab_input(&conn, win, cursor);

    let escape_kc = find_escape_keycode(&conn)?;

    let draw_gc = conn.generate_id().map_err(|e| format!("allocating gc id: {e}"))?;
    conn.create_gc(
        draw_gc,
        win,
        &CreateGCAux::new()
            .graphics_exposures(0)
            .foreground(encode_rgb(layout, (0, 255, 255)))
            .line_width(SEL_BORDER_PX as u32),
    )
    .map_err(|e| format!("creating draw gc: {e}"))?;

    let put_gc = conn.generate_id().map_err(|e| format!("allocating gc id: {e}"))?;
    conn.create_gc(put_gc, win, &CreateGCAux::new().graphics_exposures(0))
        .map_err(|e| format!("creating put gc: {e}"))?;

    let (sel, monitor) = match select_region(
        &conn,
        win,
        draw_gc,
        &monitors,
        root_rect,
        full,
        escape_kc,
    )? {
        SelectOutcome::Cancelled => {
            cleanup(&conn, win);
            return Ok(());
        }
        SelectOutcome::Selected(sel, monitor) => (sel, monitor),
    };

    let label_text = "Translating…";
    let label_px = 16.0;
    let label_w = (font.text_width(label_px, label_text) as i32 + 2 * PADDING).max(80);
    let label_h = (font.ascent(label_px).ceil() as i32) + 2 * PADDING;
    let label = build_text_panel(&conn, root_depth, layout, &font, &[label_text.to_string()], label_px, label_w, label_h, PADDING);
    let label_x = sel.x.clamp(monitor.x, (monitor.right() - label_w).max(monitor.x));
    let label_y = if sel.bottom() + label_h + GAP <= monitor.bottom() {
        sel.bottom() + GAP
    } else {
        (sel.y - label_h - GAP).max(monitor.y)
    };
    let _ = label.put(&conn, win, put_gc, label_x as i16, label_y as i16);
    let _ = conn.flush();

    let (capture, _visual): (Image<'static>, _) = Image::get(&conn, pixmap, sel.x as i16, sel.y as i16, sel.w as u16, sel.h as u16)
        .map_err(|e| format!("capturing selection: {e}"))?;
    let rgb = capture_to_rgb(&capture, layout);
    let ocr_result = io::ocr(&rgb, sel.w as u32, sel.h as u32, ocr_langs);

    // Erase the "Translating…" label patch before painting the real result.
    let _ = conn.copy_area(pixmap, win, put_gc, label_x as i16, label_y as i16, label_x as i16, label_y as i16, label_w as u16, label_h as u16);

    let mut overlays: Vec<(i32, i32, Image<'static>)> = Vec::new();
    let mut translation_boxes: Vec<ClickTarget> = Vec::new();
    let mut original_blocks: Vec<ClickTarget> = Vec::new();

    match ocr_result {
        Err(e) => {
            overlays.push(build_message_panel(&conn, root_depth, layout, &font, &format!("OCR failed: {e}"), monitor));
        }
        Ok(blocks) if blocks.is_empty() => {
            overlays.push(build_message_panel(&conn, root_depth, layout, &font, "No text found", monitor));
        }
        Ok(blocks) => {
            render_blocks(
                &conn,
                root_depth,
                layout,
                &font,
                &capture,
                sel,
                &monitors,
                monitor,
                to,
                blocks,
                &mut overlays,
                &mut translation_boxes,
                &mut original_blocks,
            );
        }
    }

    for (x, y, img) in &overlays {
        let _ = img.put(&conn, win, put_gc, *x as i16, *y as i16);
    }
    let _ = conn.flush();

    interact(
        &conn,
        win,
        draw_gc,
        layout,
        escape_kc,
        &overlays,
        put_gc,
        &translation_boxes,
        &original_blocks,
    )?;

    cleanup(&conn, win);
    Ok(())
}

/// One clickable region: clicking it copies `text` to the clipboard.
struct ClickTarget {
    rect: Rect,
    text: String,
}

#[allow(clippy::too_many_arguments)]
fn render_blocks(
    conn: &impl Connection,
    depth: u8,
    layout: PixelLayout,
    font: &Font,
    capture: &Image,
    sel: Rect,
    monitors: &[Rect],
    fallback_monitor: Rect,
    to: &str,
    blocks: Vec<OcrBlock>,
    overlays: &mut Vec<(i32, i32, Image<'static>)>,
    translation_boxes: &mut Vec<ClickTarget>,
    original_blocks: &mut Vec<ClickTarget>,
) {
    let translations = translate_all(&blocks, to);
    let block_screen_rects: Vec<Rect> = blocks
        .iter()
        .map(|b| Rect { x: sel.x + b.rect.x, y: sel.y + b.rect.y, w: b.rect.w, h: b.rect.h })
        .collect();

    let mut placed_rects: Vec<Rect> = Vec::new();

    for (i, block) in blocks.iter().enumerate() {
        let block_screen = block_screen_rects[i];
        let highlight = make_highlight(capture, layout, block.rect, depth);
        overlays.push((sel.x + block.rect.x, sel.y + block.rect.y, highlight));
        original_blocks.push(ClickTarget { rect: block_screen, text: block.text.clone() });

        let bm = monitor_for_rect(block_screen, monitors, fallback_monitor);
        let box_w = block_screen.w.clamp(MIN_BOX_W, (bm.w - 2 * MARGIN).max(MIN_BOX_W));
        let preferred_font = ((block.avg_line_height * 0.9) as i32).clamp(14, 48);

        let mut avoid: Vec<Rect> = block_screen_rects
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, r)| *r)
            .collect();
        avoid.extend(placed_rects.iter().copied());

        let (text, is_error) = match &translations[i] {
            Ok(t) => (t.clone(), false),
            Err(e) => (format!("translation failed: {e}"), true),
        };

        let height_for_font = |fp: i32| -> i32 {
            let px = fp as f32;
            let avail = (box_w - 2 * PADDING) as f32;
            let lines = logic::wrap_text(&text, avail, |c| font.char_width(px, c));
            (lines.len() as f32 * font.line_height(px)).ceil() as i32 + 2 * PADDING
        };

        let placed: PlacedBox = logic::place_box(
            block_screen,
            bm,
            box_w,
            preferred_font,
            MIN_FONT_PX,
            GAP,
            &avoid,
            height_for_font,
        );
        placed_rects.push(placed.rect);

        let px = placed.font_px as f32;
        let avail = (box_w - 2 * PADDING) as f32;
        let lines = logic::wrap_text(&text, avail, |c| font.char_width(px, c));
        let panel = build_text_panel(conn, depth, layout, font, &lines, px, placed.rect.w, placed.rect.h, PADDING);
        overlays.push((placed.rect.x, placed.rect.y, panel));
        if !is_error {
            translation_boxes.push(ClickTarget { rect: placed.rect, text });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn interact(
    conn: &impl Connection,
    win: Window,
    draw_gc: x11rb::protocol::xproto::Gcontext,
    layout: PixelLayout,
    escape_kc: Keycode,
    overlays: &[(i32, i32, Image<'static>)],
    put_gc: x11rb::protocol::xproto::Gcontext,
    translation_boxes: &[ClickTarget],
    original_blocks: &[ClickTarget],
) -> Result<(), String> {
    loop {
        let event = conn.wait_for_event().map_err(|e| format!("waiting for event: {e}"))?;
        match event {
            Event::Expose(e) if e.count == 0 => {
                for (x, y, img) in overlays {
                    let _ = img.put(conn, win, put_gc, *x as i16, *y as i16);
                }
                let _ = conn.flush();
            }
            Event::KeyPress(e) if e.detail == escape_kc => {
                return Ok(());
            }
            Event::ButtonPress(e) => {
                if e.detail == 3 {
                    return Ok(());
                }
                if e.detail == 1 {
                    let (x, y) = (i32::from(e.event_x), i32::from(e.event_y));
                    let hit = translation_boxes
                        .iter()
                        .find(|t| t.rect.contains_point(x, y))
                        .or_else(|| original_blocks.iter().find(|t| t.rect.contains_point(x, y)));
                    if let Some(target) = hit {
                        let _ = io::copy_to_clipboard(&target.text);
                        flash_green(conn, win, draw_gc, layout, target.rect);
                    }
                }
            }
            Event::Error(_) => {}
            _ => {}
        }
    }
}

fn flash_green(conn: &impl Connection, win: Window, gc: x11rb::protocol::xproto::Gcontext, layout: PixelLayout, rect: Rect) {
    let _ = conn.change_gc(gc, &ChangeGCAux::new().foreground(encode_rgb(layout, (0, 220, 0))));
    let _ = conn.poly_rectangle(
        win,
        gc,
        &[Rectangle { x: rect.x as i16, y: rect.y as i16, width: rect.w as u16, height: rect.h as u16 }],
    );
    let _ = conn.flush();
}

enum SelectOutcome {
    Selected(Rect, Rect),
    Cancelled,
}

fn select_region(
    conn: &impl Connection,
    win: Window,
    draw_gc: x11rb::protocol::xproto::Gcontext,
    monitors: &[Rect],
    root_rect: Rect,
    full: bool,
    escape_kc: Keycode,
) -> Result<SelectOutcome, String> {
    if full {
        let reply = conn
            .query_pointer(root_rect_window(conn, win))
            .map_err(|e| format!("querying pointer: {e}"))?
            .reply()
            .map_err(|e| format!("querying pointer: {e}"))?;
        let m = monitor_at(monitors, i32::from(reply.root_x), i32::from(reply.root_y), root_rect);
        return Ok(SelectOutcome::Selected(m, m));
    }

    let mut start: Option<(i32, i32)> = None;
    let mut last_drawn: Option<Rect> = None;

    loop {
        let event = conn.wait_for_event().map_err(|e| format!("waiting for event: {e}"))?;
        match event {
            Event::ButtonPress(e) => {
                if e.detail == 3 {
                    return Ok(SelectOutcome::Cancelled);
                }
                if e.detail == 1 && start.is_none() {
                    start = Some((i32::from(e.event_x), i32::from(e.event_y)));
                }
            }
            Event::MotionNotify(e) => {
                if let Some(s) = start {
                    let r = rect_from_points(s, (i32::from(e.event_x), i32::from(e.event_y)));
                    if let Some(old) = last_drawn {
                        clear_rect_border(conn, win, old);
                    }
                    draw_rect_border(conn, win, draw_gc, r);
                    last_drawn = Some(r);
                    let _ = conn.flush();
                }
            }
            Event::ButtonRelease(e) if e.detail == 1 => {
                if let Some(s) = start {
                    let end = (i32::from(e.event_x), i32::from(e.event_y));
                    if let Some(old) = last_drawn {
                        clear_rect_border(conn, win, old);
                        let _ = conn.flush();
                    }
                    let dx = (end.0 - s.0).abs();
                    let dy = (end.1 - s.1).abs();
                    if dx < CLICK_THRESHOLD && dy < CLICK_THRESHOLD {
                        let m = monitor_at(monitors, end.0, end.1, root_rect);
                        return Ok(SelectOutcome::Selected(m, m));
                    }
                    let r = rect_from_points(s, end);
                    let m = monitor_at(monitors, r.x + r.w / 2, r.y + r.h / 2, root_rect);
                    return Ok(SelectOutcome::Selected(r, m));
                }
            }
            Event::KeyPress(e) if e.detail == escape_kc => {
                return Ok(SelectOutcome::Cancelled);
            }
            Event::Error(_) => {}
            _ => {}
        }
    }
}

/// `query_pointer` needs a window id purely to know which screen to ask about; our
/// overlay window (which covers the whole root) works just as well as the root window.
fn root_rect_window(_conn: &impl Connection, win: Window) -> Window {
    win
}

fn draw_rect_border(conn: &impl Connection, win: Window, gc: x11rb::protocol::xproto::Gcontext, r: Rect) {
    let _ = conn.poly_rectangle(
        win,
        gc,
        &[Rectangle { x: r.x as i16, y: r.y as i16, width: r.w as u16, height: r.h as u16 }],
    );
}

fn clear_rect_border(conn: &impl Connection, win: Window, r: Rect) {
    let pad = SEL_BORDER_PX + 1;
    let bands = [
        (r.x - pad, r.y - pad, r.w + 2 * pad, 2 * pad),
        (r.x - pad, r.y + r.h - pad, r.w + 2 * pad, 2 * pad),
        (r.x - pad, r.y - pad, 2 * pad, r.h + 2 * pad),
        (r.x + r.w - pad, r.y - pad, 2 * pad, r.h + 2 * pad),
    ];
    for (x, y, w, h) in bands {
        let _ = conn.clear_area(false, win, x as i16, y as i16, w.max(0) as u16, h.max(0) as u16);
    }
}

fn rect_from_points(a: (i32, i32), b: (i32, i32)) -> Rect {
    let x = a.0.min(b.0);
    let y = a.1.min(b.1);
    let w = (a.0 - b.0).abs().max(1);
    let h = (a.1 - b.1).abs().max(1);
    Rect { x, y, w, h }
}

fn monitor_at(monitors: &[Rect], x: i32, y: i32, fallback: Rect) -> Rect {
    monitors.iter().find(|m| m.contains_point(x, y)).copied().unwrap_or(fallback)
}

fn monitor_for_rect(rect: Rect, monitors: &[Rect], fallback: Rect) -> Rect {
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    monitor_at(monitors, cx, cy, fallback)
}

fn query_monitors(conn: &impl Connection, root: Window, fallback: Rect) -> Vec<Rect> {
    let monitors = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| {
            reply
                .monitors
                .iter()
                .map(|m| Rect { x: i32::from(m.x), y: i32::from(m.y), w: i32::from(m.width), h: i32::from(m.height) })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if monitors.is_empty() { vec![fallback] } else { monitors }
}

fn grab_input(conn: &impl Connection, win: Window, cursor: u32) {
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut pointer_ok = false;
    let mut keyboard_ok = false;
    while Instant::now() < deadline && !(pointer_ok && keyboard_ok) {
        if !pointer_ok {
            if let Ok(cookie) = conn.grab_pointer(
                false,
                win,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                win,
                cursor,
                x11rb::CURRENT_TIME,
            ) {
                if let Ok(reply) = cookie.reply() {
                    pointer_ok = reply.status == GrabStatus::SUCCESS;
                }
            }
        }
        if !keyboard_ok {
            if let Ok(cookie) =
                conn.grab_keyboard(false, win, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
            {
                if let Ok(reply) = cookie.reply() {
                    keyboard_ok = reply.status == GrabStatus::SUCCESS;
                }
            }
        }
        if !(pointer_ok && keyboard_ok) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn create_crosshair_cursor(conn: &impl Connection) -> Result<u32, String> {
    let cursor_font = conn.generate_id().map_err(|e| format!("allocating font id: {e}"))?;
    conn.open_font(cursor_font, b"cursor").map_err(|e| format!("opening cursor font: {e}"))?;
    let cursor = conn.generate_id().map_err(|e| format!("allocating cursor id: {e}"))?;
    conn.create_glyph_cursor(cursor, cursor_font, cursor_font, 34, 35, 0, 0, 0, 0xffff, 0xffff, 0xffff)
        .map_err(|e| format!("creating crosshair cursor: {e}"))?;
    let _ = conn.close_font(cursor_font);
    Ok(cursor)
}

fn find_escape_keycode(conn: &impl Connection) -> Result<Keycode, String> {
    let setup = conn.setup();
    let min_kc = setup.min_keycode;
    let max_kc = setup.max_keycode;
    let count = max_kc - min_kc + 1;
    let reply = conn
        .get_keyboard_mapping(min_kc, count)
        .map_err(|e| format!("getting keyboard mapping: {e}"))?
        .reply()
        .map_err(|e| format!("getting keyboard mapping: {e}"))?;
    let per = reply.keysyms_per_keycode as usize;
    if per == 0 {
        return Err("keyboard mapping reported zero keysyms per keycode".to_string());
    }
    for i in 0..count as usize {
        if reply.keysyms[i * per] == XK_ESCAPE {
            return Ok(min_kc + i as u8);
        }
    }
    Err("Escape key not found in keyboard mapping".to_string())
}

fn cleanup(conn: &impl Connection, win: Window) {
    let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
    let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
    let _ = conn.destroy_window(win);
    let _ = conn.flush();
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

fn pixel_layout_for(screen: &Screen) -> Result<PixelLayout, String> {
    let visual = screen
        .allowed_depths
        .iter()
        .find_map(|d| d.visuals.iter().find(|v| v.visual_id == screen.root_visual).copied())
        .ok_or_else(|| "root visual not found in allowed depths".to_string())?;
    PixelLayout::from_visual_type(visual).map_err(|e| format!("unsupported root visual: {e:?}"))
}

fn expand8(v: u8) -> u16 {
    (u16::from(v) << 8) | u16::from(v)
}

fn encode_rgb(layout: PixelLayout, (r, g, b): (u8, u8, u8)) -> u32 {
    layout.encode((expand8(r), expand8(g), expand8(b)))
}

fn blend16(bg: (u16, u16, u16), fg: (u16, u16, u16), alpha: f32) -> (u16, u16, u16) {
    let lerp = |a: u16, b: u16| (f32::from(a) + (f32::from(b) - f32::from(a)) * alpha).round().clamp(0.0, 65535.0) as u16;
    (lerp(bg.0, fg.0), lerp(bg.1, fg.1), lerp(bg.2, fg.2))
}

fn capture_to_rgb(image: &Image, layout: PixelLayout) -> Vec<u8> {
    let w = image.width();
    let h = image.height();
    let mut out = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = layout.decode(image.get_pixel(x, y));
            out.push((r >> 8) as u8);
            out.push((g >> 8) as u8);
            out.push((b >> 8) as u8);
        }
    }
    out
}

fn make_highlight(capture: &Image, layout: PixelLayout, rel: Rect, depth: u8) -> Image<'static> {
    let w = rel.w.max(1) as u16;
    let h = rel.h.max(1) as u16;
    let setup_scanline_pad = capture.scanline_pad();
    let mut img = Image::allocate(w, h, setup_scanline_pad, depth, capture.bits_per_pixel(), capture.byte_order());
    let yellow = (65535u16, 65535u16, 0u16);
    let cw = i32::from(capture.width());
    let ch = i32::from(capture.height());
    for y in 0..h {
        for x in 0..w {
            let sx = (rel.x + i32::from(x)).clamp(0, cw - 1) as u16;
            let sy = (rel.y + i32::from(y)).clamp(0, ch - 1) as u16;
            let bg = layout.decode(capture.get_pixel(sx, sy));
            let blended = blend16(bg, yellow, HIGHLIGHT_ALPHA);
            img.put_pixel(x, y, layout.encode(blended));
        }
    }
    img.into_owned()
}

#[allow(clippy::too_many_arguments)]
fn build_text_panel(
    conn: &impl Connection,
    depth: u8,
    layout: PixelLayout,
    font: &Font,
    lines: &[String],
    font_px: f32,
    w: i32,
    h: i32,
    padding: i32,
) -> Image<'static> {
    let w = w.max(1) as u16;
    let h = h.max(1) as u16;
    let setup = conn.setup();
    let mut img = Image::allocate_native(w, h, depth, setup)
        .expect("server-supported depth has a known native pixel format")
        .into_owned();
    let bg = encode_rgb(layout, (30, 30, 30));
    for y in 0..h {
        for x in 0..w {
            img.put_pixel(x, y, bg);
        }
    }
    let border = encode_rgb(layout, (90, 160, 255));
    for x in 0..w {
        img.put_pixel(x, 0, border);
        img.put_pixel(x, h - 1, border);
    }
    for y in 0..h {
        img.put_pixel(0, y, border);
        img.put_pixel(w - 1, y, border);
    }
    let line_height = font.line_height(font_px);
    font.draw_lines(&mut img, layout, font_px, lines, padding, padding, line_height, (235, 235, 235));
    img
}

fn build_message_panel(
    conn: &impl Connection,
    depth: u8,
    layout: PixelLayout,
    font: &Font,
    msg: &str,
    monitor: Rect,
) -> (i32, i32, Image<'static>) {
    let font_px = 20.0;
    let padding = 16;
    let max_w = (monitor.w - 2 * MARGIN).clamp(MIN_BOX_W, monitor.w.max(MIN_BOX_W));
    let natural_w = font.text_width(font_px, msg) as i32 + 2 * padding;
    let w = natural_w.clamp(MIN_BOX_W, max_w);
    let lines = logic::wrap_text(msg, (w - 2 * padding) as f32, |c| font.char_width(font_px, c));
    let h = if lines.len() <= 1 {
        font.ascent(font_px).ceil() as i32 + 2 * padding
    } else {
        (lines.len() as f32 * font.line_height(font_px)).ceil() as i32 + 2 * padding
    };
    let img = build_text_panel(conn, depth, layout, font, &lines, font_px, w, h, padding);
    let x = monitor.x + (monitor.w - w) / 2;
    let y = monitor.y + (monitor.h - h) / 2;
    (x, y, img)
}
