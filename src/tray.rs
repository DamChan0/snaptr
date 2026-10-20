//! Resident process: tray icon + global hotkeys, spawns `snaptr crop|full|settings`.
//!
//! A single `snaptr tray` holds a TCP listener on [`SINGLE_INSTANCE_PORT`] for its whole
//! lifetime; a second instance finds the port taken and exits immediately. The winit event
//! loop never creates a window: it exists only to pump the Windows message loop (required by
//! `global-hotkey` there) and to wake up on a 1 Hz timer to re-check the config file for
//! hotkey/label changes.

use crate::config::Config;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::sync::Mutex;
use std::net::TcpListener;
use std::process::{Child, Command};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

#[cfg(target_os = "linux")]
use linux_tray::PlatformTray;

#[cfg(windows)]
use windows_tray::PlatformTray;

/// Port used purely as a single-instance lock; nothing ever connects to it.
const SINGLE_INSTANCE_PORT: u16 = 47913;

/// How often the resident process re-checks the config file for hotkey/label changes.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Events the tray and global hotkeys feed into the winit event loop.
#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    Crop,
    Full,
    Settings,
    Quit,
    /// Raw id from `global_hotkey`; resolved against the currently-registered hotkeys because
    /// the mapping can change after a config hot-reload.
    HotkeyPressed(u32),
}

pub fn run() -> Result<(), String> {
    // Held for the whole process lifetime: a second `snaptr tray` fails to bind and exits.
    let Ok(_listener) = TcpListener::bind(("127.0.0.1", SINGLE_INSTANCE_PORT)) else {
        return Ok(());
    };

    let cfg = Config::load();
    let event_loop: EventLoop<UserEvent> =
        EventLoop::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();

    // Must be created on this thread: on Windows it needs the thread's win32 message loop,
    // which `event_loop.run_app` below pumps.
    let manager = GlobalHotKeyManager::new().map_err(|e| format!("global hotkey manager: {e}"))?;
    let crop_hotkey = register_hotkey(&manager, &cfg.hotkey_crop, "영역 번역");
    let full_hotkey = register_hotkey(&manager, &cfg.hotkey_full, "화면 전체 번역");

    let hotkey_proxy = Mutex::new(proxy.clone());
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() == HotKeyState::Pressed {
            if let Ok(p) = hotkey_proxy.lock() {
                let _ = p.send_event(UserEvent::HotkeyPressed(event.id()));
            }
        }
    }));

    let tray = PlatformTray::new(&cfg, proxy);

    let mut app = App {
        manager,
        crop_hotkey,
        full_hotkey,
        config_mtime: config_mtime(),
        config: cfg,
        overlay_child: None,
        settings_child: None,
        tray,
    };

    event_loop.run_app(&mut app).map_err(|e| e.to_string())
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(Config::path()).and_then(|m| m.modified()).ok()
}

/// Parses and registers one hotkey, logging and returning `None` on either failure so a bad
/// config value never blocks the other hotkey or the tray itself.
fn register_hotkey(manager: &GlobalHotKeyManager, spec: &str, which: &str) -> Option<HotKey> {
    let hotkey = match HotKey::from_str(spec) {
        Ok(hotkey) => hotkey,
        Err(e) => {
            eprintln!("snaptr: {which} 단축키 '{spec}' 해석 실패: {e}");
            return None;
        }
    };
    match manager.register(hotkey) {
        Ok(()) => Some(hotkey),
        Err(e) => {
            eprintln!("snaptr: {which} 단축키 '{spec}' 등록 실패: {e}");
            None
        }
    }
}

/// Spawns `current_exe() <mode>` unless the process already in `slot` is still running.
fn spawn_if_idle(slot: &mut Option<Child>, mode: &str) {
    if let Some(child) = slot {
        if matches!(child.try_wait(), Ok(None)) {
            return;
        }
    }
    let Ok(exe) = std::env::current_exe() else { return };
    match Command::new(exe).arg(mode).spawn() {
        Ok(child) => *slot = Some(child),
        Err(e) => eprintln!("snaptr: '{mode}' 실행 실패: {e}"),
    }
}

struct App {
    manager: GlobalHotKeyManager,
    crop_hotkey: Option<HotKey>,
    full_hotkey: Option<HotKey>,
    config: Config,
    config_mtime: Option<SystemTime>,
    /// Shared by crop and full: only one overlay window makes sense at a time.
    overlay_child: Option<Child>,
    settings_child: Option<Child>,
    tray: PlatformTray,
}

impl App {
    fn poll_config(&mut self) {
        let mtime = config_mtime();
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        let new_cfg = Config::load();

        if new_cfg.hotkey_crop != self.config.hotkey_crop {
            if let Some(hk) = self.crop_hotkey.take() {
                let _ = self.manager.unregister(hk);
            }
            self.crop_hotkey = register_hotkey(&self.manager, &new_cfg.hotkey_crop, "영역 번역");
        }
        if new_cfg.hotkey_full != self.config.hotkey_full {
            if let Some(hk) = self.full_hotkey.take() {
                let _ = self.manager.unregister(hk);
            }
            self.full_hotkey = register_hotkey(&self.manager, &new_cfg.hotkey_full, "화면 전체 번역");
        }

        self.tray.update_labels(&new_cfg);
        self.config = new_cfg;
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _window_id: WindowId, _event: WindowEvent) {
        // The tray process never creates a window.
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. } | StartCause::Init) {
            self.poll_config();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Crop => spawn_if_idle(&mut self.overlay_child, "crop"),
            UserEvent::Full => spawn_if_idle(&mut self.overlay_child, "full"),
            UserEvent::Settings => spawn_if_idle(&mut self.settings_child, "settings"),
            UserEvent::Quit => event_loop.exit(),
            UserEvent::HotkeyPressed(id) => {
                if self.crop_hotkey.is_some_and(|h| h.id() == id) {
                    spawn_if_idle(&mut self.overlay_child, "crop");
                } else if self.full_hotkey.is_some_and(|h| h.id() == id) {
                    spawn_if_idle(&mut self.overlay_child, "full");
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
    }
}

/// 32x32 RGBA tray icon: an accent-colored rounded square with 3 dark horizontal bars
/// suggesting text lines. Shared by both platform backends, which want different byte layouts.
#[cfg(any(target_os = "linux", windows))]
fn icon_rgba() -> (u32, u32, Vec<u8>) {
    const SIZE: u32 = 32;
    const RADIUS: f32 = 6.0;
    const ACCENT: (u8, u8, u8) = (0xF2, 0xB5, 0x44);
    const BAR: (u8, u8, u8) = (0x16, 0x17, 0x1A);
    const BARS: [(f32, f32); 3] = [(9.0, 11.0), (13.0, 15.0), (17.0, 19.0)];
    const BAR_X: (f32, f32) = (7.0, 25.0);

    fn rounded_rect_contains(x: f32, y: f32, size: f32, r: f32) -> bool {
        let nx = if x < r { r - x } else if x > size - r { x - (size - r) } else { 0.0 };
        let ny = if y < r { r - y } else if y > size - r { y - (size - r) } else { 0.0 };
        nx * nx + ny * ny <= r * r
    }

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            if !rounded_rect_contains(fx, fy, SIZE as f32, RADIUS) {
                continue;
            }
            let in_bar = fx >= BAR_X.0
                && fx <= BAR_X.1
                && BARS.iter().any(|&(top, bottom)| fy >= top && fy < bottom);
            let (r, g, b) = if in_bar { BAR } else { ACCENT };
            let idx = ((y * SIZE + x) * 4) as usize;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 0xFF;
        }
    }
    (SIZE, SIZE, rgba)
}

#[cfg(target_os = "linux")]
mod linux_tray {
    use super::{icon_rgba, UserEvent};
    use crate::config::Config;
    use ksni::blocking::TrayMethods;
    use ksni::menu::{MenuItem as KsniMenuItem, StandardItem};
    use winit::event_loop::EventLoopProxy;

    pub struct PlatformTray {
        handle: Option<ksni::blocking::Handle<SnaptrTray>>,
    }

    struct SnaptrTray {
        proxy: EventLoopProxy<UserEvent>,
        hotkey_crop: String,
        hotkey_full: String,
    }

    /// ARGB32, network (big-endian) byte order, as `ksni::Icon::data` requires.
    fn argb_bytes() -> Vec<u8> {
        let (_, _, rgba) = icon_rgba();
        let mut argb = Vec::with_capacity(rgba.len());
        for px in rgba.chunks_exact(4) {
            argb.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
        }
        argb
    }

    impl ksni::Tray for SnaptrTray {
        fn id(&self) -> String {
            "snaptr".into()
        }

        fn title(&self) -> String {
            "snaptr".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![ksni::Icon { width: 32, height: 32, data: argb_bytes() }]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "snaptr".into(), ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.proxy.send_event(UserEvent::Settings);
        }

        fn menu(&self) -> Vec<KsniMenuItem<Self>> {
            vec![
                StandardItem {
                    label: format!("영역 번역 ({})", self.hotkey_crop),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.proxy.send_event(UserEvent::Crop);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: format!("화면 전체 번역 ({})", self.hotkey_full),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.proxy.send_event(UserEvent::Full);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "설정".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.proxy.send_event(UserEvent::Settings);
                    }),
                    ..Default::default()
                }
                .into(),
                KsniMenuItem::Separator,
                StandardItem {
                    label: "종료".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.proxy.send_event(UserEvent::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    impl PlatformTray {
        pub fn new(cfg: &Config, proxy: EventLoopProxy<UserEvent>) -> Self {
            let tray = SnaptrTray {
                proxy,
                hotkey_crop: cfg.hotkey_crop.clone(),
                hotkey_full: cfg.hotkey_full.clone(),
            };
            match tray.spawn() {
                Ok(handle) => PlatformTray { handle: Some(handle) },
                Err(e) => {
                    eprintln!("snaptr: 트레이 아이콘을 표시할 수 없습니다 (단축키는 계속 동작): {e}");
                    PlatformTray { handle: None }
                }
            }
        }

        pub fn update_labels(&self, cfg: &Config) {
            let Some(handle) = &self.handle else { return };
            let hotkey_crop = cfg.hotkey_crop.clone();
            let hotkey_full = cfg.hotkey_full.clone();
            handle.update(move |tray: &mut SnaptrTray| {
                tray.hotkey_crop = hotkey_crop;
                tray.hotkey_full = hotkey_full;
            });
        }
    }
}

#[cfg(windows)]
mod windows_tray {
    use super::{icon_rgba, UserEvent};
    use crate::config::Config;
    use std::sync::Mutex;
    use std::sync::Arc;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use winit::event_loop::EventLoopProxy;

    pub struct PlatformTray {
        // Kept alive for as long as the tray should be shown; dropping it removes the icon.
        _icon: Option<TrayIcon>,
        crop_item: MenuItem,
        full_item: MenuItem,
    }

    fn crop_label(cfg: &Config) -> String {
        format!("영역 번역 ({})", cfg.hotkey_crop)
    }

    fn full_label(cfg: &Config) -> String {
        format!("화면 전체 번역 ({})", cfg.hotkey_full)
    }

    impl PlatformTray {
        pub fn new(cfg: &Config, proxy: EventLoopProxy<UserEvent>) -> Self {
            let crop_item = MenuItem::new(crop_label(cfg), true, None);
            let full_item = MenuItem::new(full_label(cfg), true, None);
            let settings_item = MenuItem::new("설정", true, None);
            let quit_item = MenuItem::new("종료", true, None);

            let menu = Menu::new();
            if let Err(e) = menu.append_items(&[
                &crop_item,
                &full_item,
                &settings_item,
                &PredefinedMenuItem::separator(),
                &quit_item,
            ]) {
                eprintln!("snaptr: 트레이 메뉴 생성 실패: {e}");
            }

            let proxy = Arc::new(Mutex::new(proxy));
            let (crop_id, full_id, settings_id, quit_id) = (
                crop_item.id().clone(),
                full_item.id().clone(),
                settings_item.id().clone(),
                quit_item.id().clone(),
            );
            let menu_proxy = proxy.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let id = event.id();
                let sent = if *id == crop_id {
                    Some(UserEvent::Crop)
                } else if *id == full_id {
                    Some(UserEvent::Full)
                } else if *id == settings_id {
                    Some(UserEvent::Settings)
                } else if *id == quit_id {
                    Some(UserEvent::Quit)
                } else {
                    None
                };
                if let Some(event) = sent {
                    if let Ok(p) = menu_proxy.lock() {
                        let _ = p.send_event(event);
                    }
                }
            }));

            // Mirrors the Linux `activate` behavior: left click opens settings.
            let click_proxy = proxy.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } =
                    event
                {
                    if let Ok(p) = click_proxy.lock() {
                        let _ = p.send_event(UserEvent::Settings);
                    }
                }
            }));

            let (width, height, rgba) = icon_rgba();
            let icon = match Icon::from_rgba(rgba, width, height) {
                Ok(icon) => Some(icon),
                Err(e) => {
                    eprintln!("snaptr: 트레이 아이콘 생성 실패: {e}");
                    None
                }
            };

            let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("snaptr");
            if let Some(icon) = icon {
                builder = builder.with_icon(icon);
            }

            let icon_handle = match builder.build() {
                Ok(icon) => Some(icon),
                Err(e) => {
                    eprintln!("snaptr: 트레이 아이콘을 표시할 수 없습니다 (단축키는 계속 동작): {e}");
                    None
                }
            };

            PlatformTray { _icon: icon_handle, crop_item, full_item }
        }

        pub fn update_labels(&self, cfg: &Config) {
            self.crop_item.set_text(crop_label(cfg));
            self.full_item.set_text(full_label(cfg));
        }
    }
}
