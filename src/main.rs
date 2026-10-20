// No console window on Windows; CLI errors there go nowhere, the UI reports them instead.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod capture;
mod clip;
mod config;
mod font;
mod io;
mod logic;
mod ocr;
mod overlay;
mod paint;
mod settings;
mod tray;

use config::Config;

const USAGE: &str = "usage:
  snaptr                      settings window (also starts the tray in the background)
  snaptr crop [--from L] [--to L]   drag to select a region and translate it
  snaptr full [--from L] [--to L]   translate the whole monitor under the pointer
  snaptr settings             settings window only
  snaptr tray                 tray icon + global hotkeys (stays running)
languages (L): Google codes, e.g. en ko ja zh-CN zh-TW de fr es";

fn fail(msg: &str) -> ! {
    eprintln!("snaptr: {msg}\n{USAGE}");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    let result = match cmd.as_deref() {
        None => {
            spawn_tray();
            settings::run()
        }
        Some("settings") => settings::run(),
        Some("tray") => tray::run(),
        Some(mode @ ("crop" | "full")) => {
            let mut cfg = Config::load();
            while let Some(flag) = args.next() {
                let value = args.next().unwrap_or_else(|| fail(&format!("{flag} requires a value")));
                if config::lang(&value).is_none() {
                    fail(&format!("unknown language '{value}'"));
                }
                match flag.as_str() {
                    "--from" => cfg.source = value,
                    "--to" => cfg.target = value,
                    _ => fail(&format!("unknown argument '{flag}'")),
                }
            }
            overlay::run(mode == "full", &cfg)
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => fail(&format!("unknown command '{other}'")),
    };
    if let Err(e) = result {
        eprintln!("snaptr: {e}");
        std::process::exit(1);
    }
}

/// Start `snaptr tray` detached. A second tray exits immediately on its own (single
/// instance), so this is safe to call every time the settings window opens.
pub fn spawn_tray() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .arg("tray")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
