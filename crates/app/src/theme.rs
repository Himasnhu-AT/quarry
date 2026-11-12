//! DESIGN.md's visual language (owned by the `shell` stream): the palette as constants, the same
//! palette mapped onto ashlar's design tokens, and the embedded fonts (Inter 13 px for the UI,
//! JetBrains Mono 12 px for SQL and cells). Dark first: the light set is ashlar's own with
//! Quarry's accent and type scale.

use ashlar::egui::{self, Color32, FontFamily};
use ashlar::ui::fonts::FontInstall;
use ashlar::ui::tokens::{self, Tokens};

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
/// Text on the accent fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0B, 0x12, 0x1E);

/// Layout sizes in points.
pub const TOOLBAR: f32 = 44.0;
pub const SIDEBAR: f32 = 240.0;
pub const STATUS_BAR: f32 = 24.0;
pub const AGENT_PANEL: f32 = 360.0;
pub const ROW: f32 = 28.0;
/// Tab strip above the editor.
pub const TAB_STRIP: f32 = 34.0;

/// Type sizes: Inter 13 for the UI, JetBrains Mono 12 for SQL.
pub const UI_SIZE: f32 = 13.0;
pub const MONO_SIZE: f32 = 12.0;

std::thread_local! {
    static STRONG_BOUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Note whether the fonts [`install`] adds are live in `ctx` (they take effect one frame after
/// installing; egui panics on an unbound family). The view calls it at the start of each frame.
pub fn frame_start(ctx: &egui::Context) {
    let bound = ctx.fonts(|f| f.families().iter().any(|f| *f == FontFamily::Name(STRONG.into())));
    STRONG_BOUND.with(|b| b.set(bound));
}

const STRONG: &str = "quarry-strong";

/// The bold UI family (Inter SemiBold), or the regular one until it is installed.
pub fn strong_family() -> FontFamily {
    if STRONG_BOUND.with(std::cell::Cell::get) { FontFamily::Name(STRONG.into()) } else { FontFamily::Proportional }
}

static INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
static INTER_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");
static MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

/// Quarry's dark tokens: DESIGN.md's palette on ashlar's dark set.
pub fn dark() -> Tokens {
    let mut t = Tokens::dark();
    t.surface.app = BG;
    t.surface.panel = PANEL;
    t.surface.raised = RAISED;
    t.surface.sunken = Color32::from_rgb(0x0B, 0x0D, 0x10);
    t.surface.row_alt = Color32::from_rgb(0x13, 0x16, 0x1B);
    t.surface.hover = Color32::from_rgb(0x23, 0x29, 0x32);
    t.surface.pressed = Color32::from_rgb(0x2B, 0x32, 0x3D);
    t.text.primary = TEXT;
    t.text.secondary = MUTED;
    t.text.disabled = Color32::from_rgb(0x5F, 0x67, 0x72);
    t.text.on_accent = ON_ACCENT;
    t.status.info = INFO;
    t.status.success = OK;
    t.status.warning = WARN;
    t.status.danger = ERROR;
    t.accent = ACCENT;
    t.accent_hover = Color32::from_rgb(0x7A, 0xB0, 0xFF);
    t.selection = Color32::from_rgb(0x1F, 0x3A, 0x63);
    t.selection_text = TEXT;
    t.focus = ACCENT;
    t.border = BORDER;
    t.border_strong = Color32::from_rgb(0x3A, 0x42, 0x4F);
    t.separator = BORDER;
    scale(t)
}

/// The light set: ashlar's, with Quarry's accent and sizes.
pub fn light() -> Tokens {
    scale(Tokens::light().with_accent(Color32::from_rgb(0x1F, 0x6F, 0xEB)))
}

fn scale(mut t: Tokens) -> Tokens {
    t.radius.md = 6;
    t.type_scale.body = UI_SIZE;
    t.type_scale.button = UI_SIZE;
    t.type_scale.small = 12.0;
    t.type_scale.heading = 15.0;
    t.type_scale.title = 20.0;
    t.type_scale.mono = MONO_SIZE;
    t.control_height = 28.0;
    t
}

/// Install fonts and both token sets (the theme mode stays the user's choice).
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontInstall::new();
    let added = fonts
        .add("inter", INTER)
        .and_then(|f| f.add("inter-semibold", INTER_SEMIBOLD))
        .and_then(|f| f.add("jetbrains-mono", MONO))
        .and_then(|f| f.primary(FontFamily::Proportional, "inter"))
        .and_then(|f| f.primary(FontFamily::Monospace, "jetbrains-mono"))
        .and_then(|f| f.family(FontFamily::Name(STRONG.into()), &["inter-semibold", "inter"]));
    match added {
        Ok(f) => f.install(ctx),
        Err(e) => log::warn!("fonts: {e}"),
    }
    tokens::apply(ctx, &light());
    tokens::apply(ctx, &dark());
    ctx.all_styles_mut(|style| {
        use egui::{FontId, TextStyle};
        style.text_styles.insert(TextStyle::Body, FontId::proportional(UI_SIZE));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(UI_SIZE));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(11.0));
        style.text_styles.insert(TextStyle::Monospace, FontId::monospace(MONO_SIZE));
        style.text_styles.insert(TextStyle::Heading, FontId::proportional(15.0));
    });
}

/// The color of a connection's chip (prod red, staging warn, local ok).
pub fn conn_color(c: quarry_core::ConnColor) -> Option<Color32> {
    use quarry_core::ConnColor;
    match c {
        ConnColor::None => None,
        ConnColor::Local => Some(OK),
        ConnColor::Staging => Some(WARN),
        ConnColor::Prod => Some(ERROR),
    }
}

/// Text in the strong (semibold) UI face.
pub fn strong(text: impl Into<String>, size: f32) -> egui::RichText {
    egui::RichText::new(text).family(strong_family()).size(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashlar::ui::tokens::contrast_ratio;

    #[test]
    fn text_contrast_meets_wcag_aa() {
        let t = dark();
        for (fg, bg) in [(t.text.primary, BG), (t.text.primary, PANEL), (t.text.secondary, BG), (t.text.secondary, PANEL), (t.text.secondary, RAISED)]
        {
            assert!(contrast_ratio(fg, bg) >= 4.5, "{fg:?} on {bg:?}: {}", contrast_ratio(fg, bg));
        }
        assert!(t.dark && !light().dark);
    }

    #[test]
    fn fonts_and_tokens_install() {
        let ctx = egui::Context::default();
        install(&ctx);
        assert_eq!(tokens::of(&ctx, egui::Theme::Dark).accent, ACCENT);
        assert_eq!(tokens::of(&ctx, egui::Theme::Dark).type_scale.body, UI_SIZE);
    }
}
