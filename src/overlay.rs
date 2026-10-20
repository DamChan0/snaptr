//! Fullscreen selection/result overlay (winit + softbuffer, one window per monitor).

use crate::config::Config;

/// `full`: skip dragging and translate the monitor under the pointer.
pub fn run(_full: bool, _cfg: &Config) -> Result<(), String> {
    Err("overlay: not implemented yet".into())
}
