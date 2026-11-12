//! DESIGN.md's visual language as constants (owned by the `shell` stream).

use ashlar::egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x0F, 0x11, 0x15);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x1A, 0x20);
pub const RAISED: Color32 = Color32::from_rgb(0x1D, 0x22, 0x2A);
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3A);
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEB);
pub const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA3, 0xAE);
pub const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x9D, 0xFF);
pub const ERROR: Color32 = Color32::from_rgb(0xF2, 0x55, 0x5A);
pub const WARN: Color32 = Color32::from_rgb(0xF2, 0xA9, 0x3B);
pub const OK: Color32 = Color32::from_rgb(0x3C, 0xC8, 0xA0);
pub const INFO: Color32 = Color32::from_rgb(0x8F, 0xB8, 0xFF);

/// Layout sizes in points.
pub const TOOLBAR: f32 = 44.0;
pub const SIDEBAR: f32 = 240.0;
pub const STATUS_BAR: f32 = 24.0;
pub const AGENT_PANEL: f32 = 360.0;
pub const ROW: f32 = 28.0;
