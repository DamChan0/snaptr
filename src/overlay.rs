//! Fullscreen selection/result overlay: one borderless window over the whole virtual desktop
//! (winit + softbuffer), showing a frozen screenshot. The user drags or presses Enter to pick
//! a region; OCR + translation run on a worker thread; results are painted as
//! clickable highlight/translation boxes until Esc.
//!
//! Everything is in desktop-local physical pixels: (0, 0) is the virtual desktop's top-left.
//! Painting is client-side: `frame` holds the composed screen as 0x00RRGGBB; damaged areas are
//! recomposed from the screenshot (dimmed outside the selection) plus the elements on top.

use crate::capture;
use crate::clip;
use crate::config::Config;
use crate::font::Font;
use crate::io;
use crate::logic::{self, OcrBlock, Rect};
use crate::ocr;
use crate::paint::{self, Canvas, Rgb, Theme, GAP, PAINT_MARGIN, SPACE_16, SPACE_24, SPACE_8};

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};

const DIM: f32 = 0.55;
const OUTLINE_DARK: Rgb = (0x0C, 0x0C, 0x0E);
const MIN_FONT_PX: i32 = 13;
const MAX_FONT_PX: i32 = 56;
const MIN_BOX_W: i32 = 240;
const CLICK_THRESHOLD: i32 = 5;
const COPIED_FOR: Duration = Duration::from_millis(1200);
const FADE_FOR: Duration = Duration::from_millis(140);
const FADE_FRAME: Duration = Duration::from_millis(16);
const FADE_FROM: f32 = 0.3;

const HINT_TEXT: &str = "드래그해서 영역 선택 · Esc 닫기";
const READING_TEXT: &str = "글자 읽는 중";
const EMPTY_TEXT: &str = "글자를 찾지 못했어요 · 다시 드래그하거나 Esc";
const ALL_FAILED_TEXT: &str = "번역 실패 · 네트워크를 확인하세요";
const FAILED_LINE: &str = "번역 실패";
const COPIED_TEXT: &str = "복사됨";
const COPIED_ALL_TEXT: &str = "복사됨 · 전체";

/// `full`: skip dragging and translate the monitor under the pointer.
pub fn run(full: bool, cfg: &Config) -> Result<(), String> {
    let ui_font = Font::load("ko").map_err(|e| format!("loading UI font: {e}"))?;
    let text_font = match cfg.target.as_str() {
        "ko" => None,
        t => Some(Font::load(t).map_err(|e| format!("loading font for '{t}': {e}"))?),
    };
    let event_loop = EventLoop::<Msg>::with_user_event().build().map_err(|e| e.to_string())?;
    let mut app = App {
        cfg: cfg.clone(),
        theme: cfg.theme(),
        full,
        proxy: event_loop.create_proxy(),
        ui_font,
        text_font,
        chip_px: 14.0,
        win: None,
        error: None,
        s: State::default(),
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.error.map_or(Ok(()), Err)
}

/// Worker thread -> UI.
enum Msg {
    Reading(usize),
    Failed(String),
    Done(Vec<OcrBlock>, Vec<Result<String, String>>),
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
    /// Block rect for highlights, box/chip rect otherwise.
    rect: Rect,
    copy: Option<String>,
    expires: Option<Instant>,
}

impl Element {
    fn chip(role: Role, rect: Rect, text: &str, accent: bool) -> Element {
        Element { kind: Kind::Chip { text: text.to_string(), accent }, role, rect, copy: None, expires: None }
    }

    fn hit_rect(&self) -> Rect {
        match self.kind {
            Kind::Highlight => paint::highlight_bounds(self.rect),
            _ => self.rect,
        }
    }

    /// Area this element's painting touches.
    fn bounds(&self) -> Rect {
        match self.kind {
            Kind::Highlight => inflate(paint::highlight_paint_bounds(self.rect, true), 2),
            Kind::Box { .. } => inflate(self.rect, PAINT_MARGIN),
            _ => inflate(self.hit_rect(), 2),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Default)]
enum Phase {
    #[default]
    Selecting,
    Busy,
    Result,
}

/// Window, pixels and interaction state; created in `resumed`.
struct Win {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    /// Virtual desktop in global coordinates.
    desk: Rect,
    /// Monitors in desktop-local coordinates.
    monitors: Vec<Rect>,
    /// Frozen screen, packed RGB, desk.w x desk.h.
    shot: Vec<u8>,
    frame: Vec<u32>,
    damage: Vec<Rect>,
    full_present: bool,
    #[cfg(target_os = "linux")]
    focused: bool,
}

#[derive(Default)]
struct State {
    phase: Phase,
    pointer: (i32, i32),
    drag_start: Option<(i32, i32)>,
    ctrl: bool,
    /// Area shown undimmed.
    sel: Option<Rect>,
    /// Region of the current job, and its monitor.
    job: Option<(Rect, Rect)>,
    outline: bool,
    elements: Vec<Element>,
    hover: Option<usize>,
    /// Fade-in of elements[first..] started at the instant.
    fade: Option<(Instant, usize)>,
}

struct App {
    cfg: Config,
    theme: Theme,
    full: bool,
    proxy: EventLoopProxy<Msg>,
    ui_font: Font,
    /// Translation font when the target needs a different one than the UI font.
    text_font: Option<Font>,
    chip_px: f32,
    win: Option<Win>,
    error: Option<String>,
    s: State,
}

impl ApplicationHandler<Msg> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        if let Err(e) = self.open(el) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.present(),
            WindowEvent::ScaleFactorChanged { mut inner_size_writer, .. } => {
                // Keep covering the whole desktop when Windows reports a DPI change.
                if let Some(w) = &self.win {
                    let _ = inner_size_writer.request_inner_size(PhysicalSize::new(w.desk.w as u32, w.desk.h as u32));
                }
            }
            WindowEvent::Focused(focused) => {
                #[cfg(target_os = "linux")]
                if let Some(w) = &mut self.win {
                    w.focused = focused;
                }
            }
            WindowEvent::ModifiersChanged(m) => self.s.ctrl = m.state().control_key(),
            WindowEvent::CursorMoved { position, .. } => self.on_motion(position),
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Right, ElementState::Pressed) => el.exit(),
                (MouseButton::Left, ElementState::Pressed) => self.on_press(),
                (MouseButton::Left, ElementState::Released) => self.on_release(),
                _ => {}
            },
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::Escape) => el.exit(),
                    PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter)
                        if self.s.phase == Phase::Selecting && self.s.drag_start.is_none() =>
                    {
                        self.remove(Role::Hint);
                        let (x, y) = self.s.pointer;
                        let m = self.monitor_at(x, y);
                        self.start_job(m, m);
                    }
                    PhysicalKey::Code(KeyCode::KeyC) if self.s.ctrl && self.s.phase == Phase::Result => {
                        self.copy_all()
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        self.request_redraw();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, msg: Msg) {
        let Some((sel, monitor)) = self.s.job else { return };
        match msg {
            Msg::Reading(n) => {
                let chip = self.anchored_chip(Role::Status, sel, monitor, &format!("번역 중 · {n}개 블록"), false);
                self.replace(chip);
            }
            Msg::Failed(text) => {
                let chip = self.anchored_chip(Role::Status, sel, monitor, &text, true);
                self.replace(chip);
                self.s.phase = Phase::Result;
            }
            Msg::Done(blocks, translations) => {
                let all_failed = translations.iter().all(Result::is_err);
                self.remove(Role::Status);
                let new = self.layout_blocks(sel, &blocks, translations);
                let first = self.s.elements.len();
                self.s.elements.extend(new);
                if all_failed {
                    let chip = self.anchored_chip(Role::Status, sel, monitor, ALL_FAILED_TEXT, true);
                    self.s.elements.push(chip);
                }
                self.s.fade = Some((Instant::now(), first));
                self.redraw_from(first);
                self.s.phase = Phase::Result;
            }
        }
        self.request_redraw();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        self.expire(now);
        let mut next = self.s.elements.iter().filter_map(|e| e.expires).min();
        if let Some((start, first)) = self.s.fade {
            if now >= start + FADE_FOR {
                self.s.fade = None;
            } else {
                next = Some(next.map_or(now + FADE_FRAME, |n| n.min(now + FADE_FRAME)));
            }
            self.redraw_from(first);
        }
        el.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
        #[cfg(target_os = "linux")]
        self.ensure_focus();
        self.request_redraw();
    }
}

impl App {
    fn open(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        let global: Vec<Rect> = el
            .available_monitors()
            .map(|m| {
                let (p, s) = (m.position(), m.size());
                Rect { x: p.x, y: p.y, w: s.width as i32, h: s.height as i32 }
            })
            .filter(|r| r.w > 0 && r.h > 0)
            .collect();
        if global.is_empty() {
            return Err("no monitors found".into());
        }
        let desk = global.iter().skip(1).fold(global[0], |a, b| union(a, *b));
        let monitors: Vec<Rect> = global.iter().map(|m| Rect { x: m.x - desk.x, y: m.y - desk.y, ..*m }).collect();
        // Freeze the screen before our window exists.
        let shot = capture::capture(desk)?;

        let attrs = Window::default_attributes()
            .with_title("snaptr")
            .with_decorations(false)
            .with_resizable(false)
            .with_position(PhysicalPosition::new(desk.x, desk.y))
            .with_inner_size(PhysicalSize::new(desk.w as u32, desk.h as u32))
            .with_window_level(WindowLevel::AlwaysOnTop);
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::x11::WindowAttributesExtX11;
            attrs.with_override_redirect(true)
        };
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_skip_taskbar(true)
        };
        let window = Arc::new(el.create_window(attrs).map_err(|e| format!("creating overlay window: {e}"))?);
        window.set_cursor(CursorIcon::Crosshair);
        window.focus_window();
        self.chip_px = 14.0 * window.scale_factor() as f32;

        let context = softbuffer::Context::new(window.clone()).map_err(|e| e.to_string())?;
        let mut surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let (w, h) = (NonZeroU32::new(desk.w as u32).unwrap(), NonZeroU32::new(desk.h as u32).unwrap());
        surface.resize(w, h).map_err(|e| e.to_string())?;

        let frame = shot.chunks_exact(3).map(|p| pack(dim(p[0]), dim(p[1]), dim(p[2]))).collect();
        self.win = Some(Win {
            window,
            surface,
            desk,
            monitors,
            shot,
            frame,
            damage: Vec::new(),
            full_present: true,
            #[cfg(target_os = "linux")]
            focused: false,
        });

        let (px, py) = capture::pointer().map_or((desk.w / 2, desk.h / 2), |(x, y)| (x - desk.x, y - desk.y));
        self.s.pointer = (px, py);
        let m = self.monitor_at(px, py);
        if self.full {
            self.start_job(m, m);
        } else {
            let (cw, ch) = paint::chip_size(&self.ui_font, self.chip_px, HINT_TEXT);
            let r = Rect { x: m.x + (m.w - cw) / 2, y: m.bottom() - SPACE_24 - ch, w: cw, h: ch };
            self.add(Element::chip(Role::Hint, r, HINT_TEXT, false));
        }
        Ok(())
    }

    /// X11: the WM never focuses override-redirect windows; take focus once mapped.
    #[cfg(target_os = "linux")]
    fn ensure_focus(&mut self) {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Some(w) = &mut self.win else { return };
        if w.focused {
            return;
        }
        let xid = match w.window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Xlib(h)) => h.window as u32,
            Ok(RawWindowHandle::Xcb(h)) => h.window.get(),
            _ => return,
        };
        // Fails with BadMatch until the window is viewable; retried on the next wakeup.
        w.focused = capture::focus(xid).is_ok();
    }

    // ---- input -------------------------------------------------------------------

    fn on_motion(&mut self, p: PhysicalPosition<f64>) {
        let (x, y) = (p.x as i32, p.y as i32);
        self.s.pointer = (x, y);
        match self.s.phase {
            Phase::Selecting => {
                if let Some(start) = self.s.drag_start {
                    // Same clipping as on release: a selection never spans monitors.
                    let m = self.monitor_at(start.0, start.1);
                    let r = rect_from_points(start, (x, y)).intersect_opt(m).unwrap_or(Rect { x: start.0, y: start.1, w: 1, h: 1 });
                    self.set_selection(r, true);
                    let chip = self.anchored_chip(Role::SizeReadout, r, m, &format!("{} × {}", r.w, r.h), false);
                    self.replace(chip);
                }
            }
            Phase::Result => self.set_hover(self.target_at(x, y)),
            Phase::Busy => {}
        }
    }

    fn on_press(&mut self) {
        let (x, y) = self.s.pointer;
        match self.s.phase {
            Phase::Selecting if self.s.drag_start.is_none() => {
                self.remove(Role::Hint);
                self.s.drag_start = Some((x, y));
            }
            Phase::Result => match self.target_at(x, y) {
                Some(i) => self.copy_element(i),
                None => {
                    self.reset();
                    self.s.drag_start = Some((x, y));
                }
            },
            _ => {}
        }
    }

    fn on_release(&mut self) {
        let Some(start) = self.s.drag_start.take() else { return };
        if self.s.phase != Phase::Selecting {
            return;
        }
        self.remove(Role::SizeReadout);
        let end = self.s.pointer;
        if (end.0 - start.0).abs() < CLICK_THRESHOLD && (end.1 - start.1).abs() < CLICK_THRESHOLD {
            return;
        }
        let m = self.monitor_at(start.0, start.1);
        let r = rect_from_points(start, end).intersect_opt(m).unwrap_or(Rect { x: start.0, y: start.1, w: 1, h: 1 });
        self.start_job(r, m);
    }

    // ---- OCR + translation ----------------------------------------------------------

    fn start_job(&mut self, sel: Rect, monitor: Rect) {
        let Some(w) = &self.win else { return };
        let rgb = crop(&w.shot, w.desk.w, sel);
        self.s.phase = Phase::Busy;
        self.s.job = Some((sel, monitor));
        self.set_selection(sel, sel != monitor);
        let chip = self.anchored_chip(Role::Status, sel, monitor, READING_TEXT, false);
        self.replace(chip);

        let proxy = self.proxy.clone();
        let lang = self.cfg.source_lang();
        let target = self.cfg.target.clone();
        std::thread::spawn(move || {
            let msg = match ocr::ocr(&rgb, sel.w as u32, sel.h as u32, lang) {
                Err(e) if e.contains("없어요") => Msg::Failed(e),
                Err(e) => Msg::Failed(format!("글자 인식 실패 · {e}")),
                Ok(blocks) if blocks.is_empty() => Msg::Failed(EMPTY_TEXT.to_string()),
                Ok(blocks) => {
                    let _ = proxy.send_event(Msg::Reading(blocks.len()));
                    let translations = translate_all(&blocks, &target);
                    Msg::Done(blocks, translations)
                }
            };
            let _ = proxy.send_event(msg);
        });
    }

    fn layout_blocks(&self, sel: Rect, blocks: &[OcrBlock], translations: Vec<Result<String, String>>) -> Vec<Element> {
        let screen_rects: Vec<Rect> = blocks
            .iter()
            .map(|b| Rect { x: sel.x + b.rect.x, y: sel.y + b.rect.y, w: b.rect.w, h: b.rect.h })
            .collect();
        let font = self.text_font();
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
            // Font sizes are in logical px; chip_px carries the monitor scale factor.
            let scale = self.chip_px / 14.0;
            let min_px = (MIN_FONT_PX as f32 * scale).round() as i32;
            let preferred = match self.cfg.font_px {
                0 => ((block.avg_line_height * 1.05).round() as i32).clamp(min_px, (MAX_FONT_PX as f32 * scale) as i32),
                px => (px as f32 * scale).round() as i32,
            };
            // Shrinking to dodge neighbours makes text unreadable in dense layouts; below 85%
            // a readable overlap beats a tiny box.
            let floor = ((preferred as f32 * 0.85).round() as i32).max(min_px).min(preferred);
            let failed = translation.is_err();
            let text = translation.unwrap_or_else(|_| block.text.clone());
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
                floor,
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

    // ---- result interaction ---------------------------------------------------------

    /// Topmost copyable element under the point.
    fn target_at(&self, x: i32, y: i32) -> Option<usize> {
        let els = &self.s.elements;
        (0..els.len()).rev().find(|&i| els[i].copy.is_some() && els[i].hit_rect().contains_point(x, y))
    }

    fn set_hover(&mut self, new: Option<usize>) {
        if new == self.s.hover {
            return;
        }
        let old = std::mem::replace(&mut self.s.hover, new);
        for i in [old, new].into_iter().flatten() {
            self.draw_region(self.s.elements[i].bounds());
        }
        if let Some(w) = &self.win {
            w.window.set_cursor(if new.is_some() { CursorIcon::Pointer } else { CursorIcon::Crosshair });
        }
    }

    fn copy_element(&mut self, i: usize) {
        let Some(text) = self.s.elements[i].copy.clone() else { return };
        if clip::copy(&text).is_err() {
            return;
        }
        let target = self.s.elements[i].hit_rect();
        let (w, h) = paint::chip_size(&self.ui_font, self.chip_px, COPIED_TEXT);
        let y = match self.s.elements[i].kind {
            // Badge straddling the box's top edge.
            Kind::Box { .. } => target.y - h / 2,
            _ => target.y - h - 4,
        };
        let r = Rect { x: target.right() - w - SPACE_8, y, w, h }.clamp_into(self.monitor_at(target.x, target.y));
        let mut chip = Element::chip(Role::Copied, r, COPIED_TEXT, true);
        chip.expires = Some(Instant::now() + COPIED_FOR);
        self.replace(chip);
    }

    fn copy_all(&mut self) {
        let all: Vec<&str> = self
            .s
            .elements
            .iter()
            .filter(|e| matches!(e.kind, Kind::Box { .. }))
            .filter_map(|e| e.copy.as_deref())
            .collect();
        let Some((sel, monitor)) = self.s.job else { return };
        if all.is_empty() || clip::copy(&all.join("\n\n")).is_err() {
            return;
        }
        let mut chip = self.anchored_chip(Role::Copied, sel, monitor, COPIED_ALL_TEXT, true);
        chip.expires = Some(Instant::now() + COPIED_FOR);
        self.replace(chip);
    }

    fn expire(&mut self, now: Instant) {
        let gone: Vec<Rect> =
            self.s.elements.iter().filter(|e| e.expires.is_some_and(|t| t <= now)).map(Element::bounds).collect();
        if gone.is_empty() {
            return;
        }
        self.s.elements.retain(|e| e.expires.is_none_or(|t| t > now));
        self.s.hover = None;
        for b in gone {
            self.draw_region(b);
        }
    }

    /// Clear results and the selection; back to selecting.
    fn reset(&mut self) {
        let mut gone: Vec<Rect> = self.s.elements.iter().map(Element::bounds).collect();
        gone.extend(self.s.sel.map(|s| inflate(s, 2)));
        self.s = State { pointer: self.s.pointer, ctrl: self.s.ctrl, ..State::default() };
        for b in gone {
            self.draw_region(b);
        }
        if let Some(w) = &self.win {
            w.window.set_cursor(CursorIcon::Crosshair);
        }
    }

    // ---- element bookkeeping --------------------------------------------------------

    /// Chip 8 px below `anchor`'s bottom-left, flipped above when there is no room, inside
    /// the bottom-left corner when neither fits (whole-monitor selection).
    fn anchored_chip(&self, role: Role, anchor: Rect, monitor: Rect, text: &str, accent: bool) -> Element {
        let (w, h) = paint::chip_size(&self.ui_font, self.chip_px, text);
        let y = if anchor.bottom() + GAP + h <= monitor.bottom() {
            anchor.bottom() + GAP
        } else if anchor.y - GAP - h >= monitor.y {
            anchor.y - GAP - h
        } else {
            anchor.bottom() - SPACE_24 - h
        };
        let x = if anchor == monitor { anchor.x + SPACE_24 } else { anchor.x };
        Element::chip(role, Rect { x, y, w, h }.clamp_into(monitor), text, accent)
    }

    fn add(&mut self, e: Element) {
        let b = e.bounds();
        self.s.elements.push(e);
        self.draw_region(b);
    }

    fn remove(&mut self, role: Role) {
        let gone: Vec<Rect> = self.s.elements.iter().filter(|e| e.role == role).map(Element::bounds).collect();
        if gone.is_empty() {
            return;
        }
        self.s.elements.retain(|e| e.role != role);
        self.s.hover = None;
        for b in gone {
            self.draw_region(b);
        }
    }

    fn replace(&mut self, e: Element) {
        self.remove(e.role);
        self.add(e);
    }

    // ---- pixels ---------------------------------------------------------------------

    /// Show `r` undimmed and (optionally) outlined; repaint only what changed.
    fn set_selection(&mut self, r: Rect, outline: bool) {
        let old = self.s.sel.replace(r);
        self.s.outline = outline;
        let mut parts = Vec::new();
        match old {
            Some(o) => {
                parts.extend(subtract(inflate(o, 2), r));
                parts.extend(subtract(r, o));
            }
            None => parts.push(r),
        }
        for p in parts {
            self.draw_region(p);
        }
        if outline {
            for p in subtract(inflate(r, 2), r) {
                self.draw_region(p);
            }
        }
    }

    fn redraw_from(&mut self, first: usize) {
        let regions: Vec<Rect> = self.s.elements[first.min(self.s.elements.len())..].iter().map(Element::bounds).collect();
        for r in regions {
            self.draw_region(r);
        }
    }

    /// Recompose `r` (background + elements, with fade) into the frame.
    fn draw_region(&mut self, r: Rect) {
        let Some(w) = &self.win else { return };
        let Some(r) = r.intersect_opt(Rect { x: 0, y: 0, w: w.desk.w, h: w.desk.h }) else { return };
        let mut cv = Canvas::new((r.x, r.y), r.w, r.h, self.background(w, r));
        let fade = self.s.fade.map(|(start, first)| {
            let t = (start.elapsed().as_secs_f32() / FADE_FOR.as_secs_f32()).min(1.0);
            (FADE_FROM + (1.0 - FADE_FROM) * (1.0 - 2f32.powf(-10.0 * t)), first)
        });
        let split = fade.map_or(self.s.elements.len(), |(_, first)| first);
        self.paint_elements(&mut cv, r, 0..split);
        if let Some((alpha, first)) = fade.filter(|(a, _)| *a < 0.999) {
            let base = cv.px.clone();
            self.paint_elements(&mut cv, r, first..self.s.elements.len());
            for (d, b) in cv.px.iter_mut().zip(base) {
                *d = lerp8(b, *d, alpha);
            }
        } else {
            self.paint_elements(&mut cv, r, split..self.s.elements.len());
        }
        let w = self.win.as_mut().unwrap();
        for (row, src) in cv.px.chunks_exact(r.w as usize * 3).enumerate() {
            let start = ((r.y + row as i32) * w.desk.w + r.x) as usize;
            for (dst, p) in w.frame[start..start + r.w as usize].iter_mut().zip(src.chunks_exact(3)) {
                *dst = pack(p[0], p[1], p[2]);
            }
        }
        w.damage.push(r);
    }

    fn paint_elements(&self, cv: &mut Canvas, r: Rect, range: std::ops::Range<usize>) {
        for i in range {
            let e = &self.s.elements[i];
            if !e.bounds().overlaps(&r) {
                continue;
            }
            let hover = self.s.hover == Some(i);
            match &e.kind {
                Kind::Highlight => paint::highlight(cv, &self.theme, e.rect, hover),
                Kind::Box { lines, font_px, failed } => {
                    paint::translation_box(cv, &self.theme, e.rect, self.text_font(), *font_px, lines, *failed, hover)
                }
                Kind::Chip { text, accent } => {
                    paint::chip(cv, &self.theme, e.rect, &self.ui_font, self.chip_px, text, *accent)
                }
            }
        }
    }

    /// Screenshot pixels of `r` as currently shown: clear inside the selection, dimmed
    /// outside, with the selection outline (1px accent, then 1px dark) just outside it.
    /// The outline is part of the background so elements always paint over it.
    fn background(&self, w: &Win, r: Rect) -> Vec<u8> {
        let mut px = crop(&w.shot, w.desk.w, r);
        let sel = self.s.sel;
        let outline = sel.filter(|_| self.s.outline);
        let accent = self.theme.accent;
        for y in 0..r.h {
            for x in 0..r.w {
                let (gx, gy) = (r.x + x, r.y + y);
                if sel.is_some_and(|s| s.contains_point(gx, gy)) {
                    continue;
                }
                let i = ((y * r.w + x) * 3) as usize;
                let ring = outline.and_then(|s| {
                    if inflate(s, 1).contains_point(gx, gy) {
                        Some(accent)
                    } else if inflate(s, 2).contains_point(gx, gy) {
                        Some(OUTLINE_DARK)
                    } else {
                        None
                    }
                });
                let c = ring.unwrap_or((dim(px[i]), dim(px[i + 1]), dim(px[i + 2])));
                px[i..i + 3].copy_from_slice(&[c.0, c.1, c.2]);
            }
        }
        px
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.win {
            if w.full_present || !w.damage.is_empty() {
                w.window.request_redraw();
            }
        }
    }

    fn present(&mut self) {
        let Some(w) = &mut self.win else { return };
        let width = w.desk.w as usize;
        let Ok(mut buf) = w.surface.buffer_mut() else { return };
        // Age 0 = undefined contents: copy everything.
        let full = w.full_present || buf.age() == 0 || buf.len() != w.frame.len();
        let damage: Vec<softbuffer::Rect> = if full {
            let n = buf.len().min(w.frame.len());
            buf[..n].copy_from_slice(&w.frame[..n]);
            Vec::new()
        } else {
            w.damage
                .iter()
                .map(|r| {
                    for y in r.y..r.bottom() {
                        let s = y as usize * width + r.x as usize;
                        buf[s..s + r.w as usize].copy_from_slice(&w.frame[s..s + r.w as usize]);
                    }
                    softbuffer::Rect {
                        x: r.x as u32,
                        y: r.y as u32,
                        width: NonZeroU32::new(r.w as u32).unwrap(),
                        height: NonZeroU32::new(r.h as u32).unwrap(),
                    }
                })
                .collect()
        };
        let presented = if full { buf.present() } else { buf.present_with_damage(&damage) };
        if presented.is_ok() {
            w.full_present = false;
            w.damage.clear();
        }
    }

    fn monitor_at(&self, x: i32, y: i32) -> Rect {
        let Some(w) = &self.win else { return Rect { x: 0, y: 0, w: 1, h: 1 } };
        w.monitors.iter().find(|m| m.contains_point(x, y)).copied().unwrap_or(w.monitors[0])
    }

    fn text_font(&self) -> &Font {
        self.text_font.as_ref().unwrap_or(&self.ui_font)
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

    fn clamp_into(self, o: Rect) -> Rect {
        Rect {
            x: self.x.clamp(o.x, (o.right() - self.w).max(o.x)),
            y: self.y.clamp(o.y, (o.bottom() - self.h).max(o.y)),
            ..self
        }
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect { x, y, w: a.right().max(b.right()) - x, h: a.bottom().max(b.bottom()) - y }
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

/// RGB of `r` (which must lie inside the image) from a packed RGB image `img_w` wide.
fn crop(img: &[u8], img_w: i32, r: Rect) -> Vec<u8> {
    let mut out = Vec::with_capacity((r.w * r.h * 3) as usize);
    for y in r.y..r.bottom() {
        let s = ((y * img_w + r.x) * 3) as usize;
        out.extend_from_slice(&img[s..s + (r.w * 3) as usize]);
    }
    out
}

fn dim(v: u8) -> u8 {
    (f32::from(v) * DIM) as u8
}

fn pack(r: u8, g: u8, b: u8) -> u32 {
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

fn lerp8(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
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
