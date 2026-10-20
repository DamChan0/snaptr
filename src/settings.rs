//! Settings window (eframe): languages, font size, colors, hotkeys.

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use global_hotkey::hotkey::HotKey;

use crate::config::{self, Config, FONT_PX_MAX, FONT_PX_MIN, LANGS};
use crate::font;
use crate::paint::{self, Rgb};

const PANEL: Rgb = (0x1D, 0x1F, 0x23);
const SURFACE: Rgb = (0x16, 0x17, 0x1A);
const TEXT: Rgb = (0xEC, 0xED, 0xEF);
const SECONDARY: Rgb = (0xA4, 0xA7, 0xAE);
const RADIUS: u8 = 8;
/// Mirrors `paint.rs`'s private highlight fill alpha (design.md "Tokens"): the overlay's
/// accent highlight over original text is a 22% fill.
const HIGHLIGHT_FILL_ALPHA: f32 = 0.22;
const SAVED_MESSAGE: Duration = Duration::from_millis(1500);
const ERROR_TEXT: Rgb = (0xE0, 0x6C, 0x6C);
const PAGE_MARGIN: u8 = 24;
const CARD_RADIUS: u8 = 10;
/// Section card fill; one step lighter than PANEL so SURFACE-filled widgets read as inset.
const CARD: Rgb = (0x26, 0x28, 0x2D);
const BORDER: Rgb = (0x3A, 0x3C, 0x42);
/// Label column width in section grids.
const LABEL_W: f32 = 104.0;

/// Which hotkey row is currently waiting for a new key combo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HotkeySlot {
    Crop,
    Full,
}

pub fn run() -> Result<(), String> {
    let use_glow = std::env::var_os("SNAPTR_RENDERER").as_deref() == Some(std::ffi::OsStr::new("glow"));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("snaptr 설정")
            .with_inner_size([520.0, 880.0])
            .with_min_inner_size([460.0, 520.0]),
        renderer: if use_glow { eframe::Renderer::Glow } else { eframe::Renderer::Wgpu },
        ..Default::default()
    };
    let result = eframe::run_native(
        "snaptr 설정",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(SettingsApp::new()) as Box<dyn eframe::App>)
        }),
    );
    match result {
        Ok(()) => Ok(()),
        // winit event loops can only run once per process, so a failed wgpu init can't just
        // retry in-place: start a fresh process and let it pick the glow backend.
        Err(eframe::Error::Wgpu(err)) if !use_glow => {
            eprintln!("snaptr: wgpu renderer failed ({err}); retrying with glow");
            relaunch_with_glow()
        }
        Err(err) => Err(format!("eframe: {err}")),
    }
}

fn relaunch_with_glow() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let status = std::process::Command::new(exe)
        .arg("settings")
        .env("SNAPTR_RENDERER", "glow")
        .status()
        .map_err(|e| format!("relaunching with glow renderer: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("glow renderer exited with {status}"))
    }
}

/// egui's bundled font has no Hangul/CJK/Thai glyphs. `ko` covers ko/ja/zh on the Noto
/// CJK family (Linux) or Malgun Gothic (Windows) and goes first; ja/zh-CN/th are appended
/// as fallbacks only when they resolve to different bytes, so Noto CJK isn't loaded twice.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded: Vec<Vec<u8>> = Vec::new();
    for (i, (name, lang)) in
        [("snaptr-ko", "ko"), ("snaptr-ja", "ja"), ("snaptr-zh", "zh-CN"), ("snaptr-th", "th")].into_iter().enumerate()
    {
        let Ok((bytes, index)) = font::font_bytes(lang) else { continue };
        if loaded.contains(&bytes) {
            continue;
        }
        loaded.push(bytes.clone());
        let data = egui::FontData { index, ..egui::FontData::from_owned(bytes) };
        fonts.font_data.insert(name.to_string(), Arc::new(data));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = fonts.families.entry(family).or_default();
            if i == 0 {
                list.insert(0, name.to_string());
            } else {
                list.push(name.to_string());
            }
        }
    }
    ctx.set_fonts(fonts);
}

/// Dark theme matching the overlay's design tokens; `accent` is the user's current pick so
/// selection/hover/focus colors stay in sync with the color pickers below.
fn dark_visuals(accent: egui::Color32) -> egui::Visuals {
    use egui::{CornerRadius, Stroke};
    let mut v = egui::Visuals::dark();
    v.panel_fill = rgb32(PANEL);
    v.window_fill = rgb32(PANEL);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.extreme_bg_color = rgb32(SURFACE);
    v.faint_bg_color = rgb32(SURFACE);
    v.hyperlink_color = accent;
    v.selection.bg_fill = with_alpha(accent, 0.35);
    v.selection.stroke = Stroke::new(1.0, accent);

    v.widgets.noninteractive.bg_fill = rgb32(PANEL);
    v.widgets.noninteractive.weak_bg_fill = rgb32(PANEL);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, rgb32(TEXT));
    v.widgets.inactive.bg_fill = rgb32(SURFACE);
    v.widgets.inactive.weak_bg_fill = rgb32(SURFACE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, rgb32(TEXT));
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, rgb32(BORDER));
    v.widgets.hovered.bg_fill = rgb32(SURFACE);
    v.widgets.hovered.weak_bg_fill = rgb32(SURFACE);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, rgb32(TEXT));
    v.widgets.active.bg_fill = rgb32(SURFACE);
    v.widgets.active.weak_bg_fill = rgb32(SURFACE);
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent);
    v.widgets.active.fg_stroke = Stroke::new(1.5, accent);
    v.widgets.open.bg_fill = rgb32(SURFACE);
    v.widgets.open.weak_bg_fill = rgb32(SURFACE);
    v.widgets.open.bg_stroke = Stroke::new(1.0, accent);
    v.widgets.open.fg_stroke = Stroke::new(1.0, rgb32(TEXT));

    for w in
        [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open]
    {
        w.corner_radius = CornerRadius::same(RADIUS);
    }
    v
}

fn rgb32((r, g, b): Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

fn with_alpha(c: egui::Color32, a: f32) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn accent32(cfg: &Config) -> egui::Color32 {
    rgb32(cfg.accent)
}

fn surface32(cfg: &Config) -> egui::Color32 {
    rgb32(cfg.surface)
}

fn text32(cfg: &Config) -> egui::Color32 {
    rgb32(cfg.text)
}

struct SettingsApp {
    cfg: Config,
    /// Remembers the last explicit font size while "원문 크기에 맞춤" is checked, so toggling
    /// it back off doesn't lose the user's chosen size.
    last_font_px: u32,
    capturing: Option<HotkeySlot>,
    hotkey_error: Option<(HotkeySlot, String)>,
    /// The Super/Windows key has no entry in `egui::Modifiers`, so it's tracked by hand from
    /// raw `Key::SuperLeft`/`SuperRight` press/release events.
    super_held: bool,
    saved_until: Option<Instant>,
    save_error: Option<String>,
    renderer_logged: bool,
}

impl SettingsApp {
    fn new() -> Self {
        let cfg = Config::load();
        let last_font_px = if cfg.font_px == 0 { 24 } else { cfg.font_px };
        Self {
            cfg,
            last_font_px,
            capturing: None,
            hotkey_error: None,
            super_held: false,
            saved_until: None,
            save_error: None,
            renderer_logged: false,
        }
    }

    fn log_renderer_once(&mut self, frame: &eframe::Frame) {
        if self.renderer_logged {
            return;
        }
        self.renderer_logged = true;
        if frame.wgpu_render_state().is_some() {
            eprintln!("snaptr: settings window using the wgpu renderer");
        } else if frame.gl().is_some() {
            eprintln!("snaptr: settings window using the glow renderer");
        }
    }

    /// Consumes key events while a hotkey row is waiting for a new combo. Modifier-only
    /// presses (holding Ctrl/Alt/Shift/Super) are ignored until a non-modifier key arrives
    /// with at least one modifier held; Esc cancels.
    fn handle_hotkey_capture(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key { key, pressed, repeat, modifiers, .. } = event else { continue };
            if matches!(key, egui::Key::SuperLeft | egui::Key::SuperRight) {
                self.super_held = pressed;
                continue;
            }
            let Some(slot) = self.capturing else { continue };
            if !pressed || repeat {
                continue;
            }
            if key == egui::Key::Escape {
                self.capturing = None;
                self.hotkey_error = None;
                continue;
            }
            if matches!(
                key,
                egui::Key::ControlLeft
                    | egui::Key::ControlRight
                    | egui::Key::ShiftLeft
                    | egui::Key::ShiftRight
                    | egui::Key::AltLeft
                    | egui::Key::AltRight
            ) {
                continue;
            }
            if !(modifiers.ctrl || modifiers.alt || modifiers.shift || self.super_held) {
                continue;
            }
            let combo = hotkey_string(modifiers, self.super_held, key);
            match HotKey::from_str(&combo) {
                Ok(parsed) => {
                    let other = match slot {
                        HotkeySlot::Crop => &self.cfg.hotkey_full,
                        HotkeySlot::Full => &self.cfg.hotkey_crop,
                    };
                    let duplicate = HotKey::from_str(other).map(|o| o == parsed).unwrap_or(false);
                    if duplicate {
                        self.hotkey_error = Some((slot, "다른 단축키와 같아요".into()));
                    } else {
                        match slot {
                            HotkeySlot::Crop => self.cfg.hotkey_crop = combo,
                            HotkeySlot::Full => self.cfg.hotkey_full = combo,
                        }
                        self.capturing = None;
                        self.hotkey_error = None;
                    }
                }
                Err(_) => {
                    self.hotkey_error = Some((slot, "지원하지 않는 키예요".into()));
                }
            }
        }
    }

    fn draw_body(&mut self, ui: &mut egui::Ui) {
        section(ui, "번역", |ui| {
            grid(ui, "langs", |ui| {
                ui.label("원본 언어");
                lang_combo(ui, "source_lang", &mut self.cfg.source);
                ui.end_row();
                ui.label("번역 언어");
                lang_combo(ui, "target_lang", &mut self.cfg.target);
                ui.end_row();
                ui.label("글자 크기");
                ui.horizontal(|ui| {
                    let mut auto = self.cfg.font_px == 0;
                    if ui.checkbox(&mut auto, "원문에 맞춤").changed() {
                        if auto {
                            if self.cfg.font_px != 0 {
                                self.last_font_px = self.cfg.font_px;
                            }
                            self.cfg.font_px = 0;
                        } else {
                            self.cfg.font_px = self.last_font_px;
                        }
                    }
                    if !auto {
                        ui.add(egui::Slider::new(&mut self.cfg.font_px, FONT_PX_MIN..=FONT_PX_MAX).suffix(" px"));
                        self.last_font_px = self.cfg.font_px;
                    }
                });
                ui.end_row();
            });
        });

        section(ui, "색상", |ui| {
            grid(ui, "colors", |ui| {
                color_row(ui, "강조색", &mut self.cfg.accent);
                color_row(ui, "박스 배경", &mut self.cfg.surface);
                color_row(ui, "글자색", &mut self.cfg.text);
            });
            ui.add_space(paint::SPACE_12 as f32);
            draw_preview(ui, &self.cfg);
            ui.add_space(paint::SPACE_8 as f32);
            if ui.button("기본 색으로").clicked() {
                let d = Config::default();
                self.cfg.accent = d.accent;
                self.cfg.surface = d.surface;
                self.cfg.text = d.text;
            }
        });

        section(ui, "단축키", |ui| {
            grid(ui, "hotkeys", |ui| {
                self.hotkey_row(ui, "영역 번역", HotkeySlot::Crop);
                self.hotkey_row(ui, "화면 전체 번역", HotkeySlot::Full);
            });
        });

        ui.add_space(paint::SPACE_16 as f32);
    }

    fn hotkey_row(&mut self, ui: &mut egui::Ui, label: &str, slot: HotkeySlot) {
        ui.label(label);
        ui.horizontal(|ui| {
            if self.capturing == Some(slot) {
                ui.colored_label(accent32(&self.cfg), "키 조합을 누르세요 · Esc 취소");
            } else {
                let current = match slot {
                    HotkeySlot::Crop => &self.cfg.hotkey_crop,
                    HotkeySlot::Full => &self.cfg.hotkey_full,
                };
                ui.label(egui::RichText::new(current.replace('+', " + ")).monospace().strong());
                if ui.button("변경").clicked() {
                    self.capturing = Some(slot);
                    self.hotkey_error = None;
                    self.super_held = false;
                }
            }
            if let Some((err_slot, msg)) = &self.hotkey_error {
                if *err_slot == slot {
                    ui.colored_label(rgb32(ERROR_TEXT), msg);
                }
            }
        });
        ui.end_row();
    }

    fn draw_footer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let accent = accent32(&self.cfg);
            if let Some(until) = self.saved_until {
                if Instant::now() < until {
                    ui.colored_label(accent, "저장됨 · 트레이에 바로 반영돼요");
                    ctx.request_repaint_after(Duration::from_millis(100));
                } else {
                    self.saved_until = None;
                }
            }
            if let Some(err) = &self.save_error {
                ui.colored_label(rgb32(ERROR_TEXT), format!("저장 실패: {err}"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = egui::Button::new(egui::RichText::new("저장").strong().color(surface32(&self.cfg)))
                    .fill(accent)
                    .min_size(egui::vec2(88.0, 34.0));
                if ui.add(save).clicked() {
                    match self.cfg.save() {
                        Ok(()) => {
                            self.saved_until = Some(Instant::now() + SAVED_MESSAGE);
                            self.save_error = None;
                        }
                        Err(e) => self.save_error = Some(e),
                    }
                }
                if ui.add(egui::Button::new("닫기").min_size(egui::vec2(72.0, 34.0))).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.log_renderer_once(frame);
        let ctx = ui.ctx().clone();
        ctx.set_visuals(dark_visuals(accent32(&self.cfg)));
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            ctx.style_mut_of(theme, |s| {
                s.spacing.item_spacing = egui::vec2(8.0, 8.0);
                s.spacing.button_padding = egui::vec2(12.0, 6.0);
                s.spacing.interact_size.y = 30.0;
                s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
                s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
                s.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(14.0));
            });
        }
        self.handle_hotkey_capture(&ctx);
        if self.capturing.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }

        let footer = egui::Frame::new()
            .fill(rgb32(PANEL))
            .inner_margin(egui::Margin::symmetric(PAGE_MARGIN as i8, paint::SPACE_12 as i8));
        egui::Panel::bottom("footer").frame(footer).show(ui, |ui| self.draw_footer(ui, &ctx));
        let page = egui::Frame::new().fill(rgb32(PANEL)).inner_margin(egui::Margin::symmetric(PAGE_MARGIN as i8, 0));
        egui::CentralPanel::default().frame(page).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| self.draw_body(ui));
        });
    }
}

/// Titled card grouping one settings section.
fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(paint::SPACE_16 as f32);
    ui.label(egui::RichText::new(title).color(rgb32(SECONDARY)).size(13.0).strong());
    ui.add_space(paint::SPACE_8 as f32);
    let inner_w = ui.available_width() - 2.0 * paint::SPACE_16 as f32;
    egui::Frame::new()
        .fill(rgb32(CARD))
        .corner_radius(CARD_RADIUS)
        .inner_margin(paint::SPACE_16 as f32)
        .show(ui, |ui| {
            ui.set_width(inner_w);
            body(ui);
        });
}

/// Two-column label/control grid inside a section card.
fn grid(ui: &mut egui::Ui, id: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).min_col_width(LABEL_W).spacing([paint::SPACE_16 as f32, 12.0]).show(ui, body);
}

fn lang_combo(ui: &mut egui::Ui, id: &str, code: &mut String) {
    let current_label = config::lang(code).map(|l| l.label).unwrap_or("?");
    egui::ComboBox::from_id_salt(id).selected_text(current_label).show_ui(ui, |ui| {
        for l in LANGS {
            if ui.selectable_label(*code == l.google, l.label).clicked() {
                *code = l.google.to_string();
            }
        }
    });
}

fn color_row(ui: &mut egui::Ui, label: &str, color: &mut Rgb) {
    ui.label(label);
    ui.horizontal(|ui| {
        let mut rgb = [color.0, color.1, color.2];
        if ui.color_edit_button_srgb(&mut rgb).changed() {
            *color = (rgb[0], rgb[1], rgb[2]);
        }
        ui.label(egui::RichText::new(config::to_hex(*color)).monospace().color(rgb32(SECONDARY)));
    });
    ui.end_row();
}

/// Builds a `global_hotkey` syntax string, e.g. "Ctrl+Alt+T", in a fixed modifier order so
/// two captures of the same physical combo always produce the same string.
fn hotkey_string(modifiers: egui::Modifiers, super_held: bool, key: egui::Key) -> String {
    let mut parts = Vec::new();
    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if super_held {
        parts.push("Super");
    }
    parts.push(key.name());
    parts.join("+")
}

/// Live preview of the overlay: a highlighted "original text" line above a translation box,
/// painted with egui's painter using the colors and font size currently being edited.
fn draw_preview(ui: &mut egui::Ui, cfg: &Config) {
    let accent = accent32(cfg);
    let surface = surface32(cfg);
    let text = text32(cfg);
    let px = if cfg.font_px == 0 { 18.0 } else { cfg.font_px as f32 };
    let sample = "번역된 문장이 이렇게 보여요";

    let avail_w = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(avail_w, 120.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Light page standing in for the screenshot the real overlay sits on top of.
    painter.rect_filled(rect, paint::RADIUS_HIGHLIGHT, egui::Color32::from_rgb(0xEE, 0xED, 0xE8));

    let origin = rect.min + egui::vec2(paint::SPACE_16 as f32, paint::SPACE_16 as f32);
    let original =
        painter.layout_no_wrap("The original text".to_string(), egui::FontId::proportional(16.0), egui::Color32::from_gray(40));
    let highlight_rect = egui::Rect::from_min_size(origin, original.size() + egui::vec2(8.0, 4.0));
    painter.galley(origin + egui::vec2(4.0, 2.0), original, egui::Color32::from_gray(40));
    painter.rect_filled(highlight_rect, paint::RADIUS_HIGHLIGHT, with_alpha(accent, HIGHLIGHT_FILL_ALPHA));
    painter.rect_stroke(
        highlight_rect,
        paint::RADIUS_HIGHLIGHT,
        egui::Stroke::new(1.0, accent),
        egui::StrokeKind::Inside,
    );

    let font_id = egui::FontId::new(px, egui::FontFamily::Proportional);
    let galley = painter.layout_no_wrap(sample.to_string(), font_id, text);
    let (pad_x, pad_y) = paint::box_padding(px);
    let line_h = px * 1.45;
    let box_w = galley.size().x + 2.0 * pad_x as f32;
    let box_h = line_h + 2.0 * pad_y as f32;
    let box_rect = egui::Rect::from_min_size(
        egui::pos2(origin.x, highlight_rect.bottom() + paint::GAP as f32),
        egui::vec2(box_w, box_h),
    );

    let shadow = egui::Shadow {
        offset: [0, paint::SHADOW_OFFSET_Y as i8],
        blur: paint::SHADOW_BLUR as u8,
        spread: 0,
        color: with_alpha(egui::Color32::BLACK, paint::SHADOW_ALPHA),
    };
    painter.add(shadow.as_shape(box_rect, paint::RADIUS_BOX));
    painter.rect_filled(box_rect, paint::RADIUS_BOX, with_alpha(surface, paint::SURFACE_ALPHA));
    painter.rect_stroke(
        box_rect,
        paint::RADIUS_BOX,
        egui::Stroke::new(1.0, with_alpha(rgb32(paint::SURFACE_BORDER), paint::SURFACE_BORDER_ALPHA)),
        egui::StrokeKind::Inside,
    );
    let text_pos = egui::pos2(box_rect.min.x + pad_x as f32, box_rect.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_string_orders_modifiers() {
        let mods = egui::Modifiers { alt: true, ctrl: true, shift: false, mac_cmd: false, command: true };
        assert_eq!(hotkey_string(mods, false, egui::Key::T), "Ctrl+Alt+T");
        assert_eq!(hotkey_string(egui::Modifiers::default(), true, egui::Key::F5), "Super+F5");
    }

    #[test]
    fn hotkey_string_parses_as_valid_hotkey() {
        let mods = egui::Modifiers { alt: true, shift: true, ..Default::default() };
        let s = hotkey_string(mods, false, egui::Key::F5);
        assert_eq!(s, "Alt+Shift+F5");
        assert!(HotKey::from_str(&s).is_ok());
    }
}
