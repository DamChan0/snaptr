//! Frozen-screen capture in physical pixels, virtual-desktop coordinates.

use crate::logic::Rect;

/// Packed RGB (3 bytes/px, row-major) of screen area `r`, including all windows on it.
pub fn capture(_r: Rect) -> Result<Vec<u8>, String> {
    Err("capture: not implemented yet".into())
}
