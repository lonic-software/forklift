//! Every token the desktop draws with, copied from the accepted design system,
//! docs/proof-load-design-system.html. This is the only file that names a colour or a type size:
//! tests/design_system.rs fails on a token here that differs from the page, a token of the page
//! missing here, or a colour or font size written anywhere else in src/.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, Visuals};

/// The two palettes and the page's token names, from one list, so a field cannot be added without
/// its name. `tokens` is what the parity test compares against the page.
macro_rules! palette {
    ($($field:ident = $token:literal),* $(,)?) => {
        /// The page's colour tokens for one theme.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Palette {
            $(pub $field: Color32,)*
        }

        impl Palette {
            /// Each colour with its CSS custom property name, in the page's order.
            pub fn tokens(&self) -> Vec<(&'static str, Color32)> {
                vec![$(($token, self.$field),)*]
            }
        }
    };
}

palette! {
    bg = "--bg", panel = "--panel", pad = "--pad", pad_hi = "--pad-hi", pad_lo = "--pad-lo",
    well = "--well", ink = "--ink", muted = "--muted", line = "--line", engrave = "--engrave",
    accent = "--accent", accent_ink = "--accent-ink", focus = "--focus", lamp_off = "--lamp-off",
    g_healed = "--g-healed", g_remedy = "--g-remedy", g_noremedy = "--g-noremedy",
    g_lost = "--g-lost", g_silent = "--g-silent", g_silent_ink = "--g-silent-ink",
    sampled = "--sampled", notrun = "--notrun",
    crt_bg = "--crt-bg", crt_ink = "--crt-ink", crt_dim = "--crt-dim", crt_red = "--crt-red",
    crt_amber = "--crt-amber",
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// The step screen is the same in both themes.
const CRT_BG: Color32 = hex(0x0d1512);
const CRT_INK: Color32 = hex(0x8fe3b4);
const CRT_DIM: Color32 = hex(0x4f8a6c);
const CRT_RED: Color32 = hex(0xff7a7a);
const CRT_AMBER: Color32 = hex(0xf0b45a);

pub const LIGHT: Palette = Palette {
    bg: hex(0xdcdee2), panel: hex(0xf4f5f7), pad: hex(0xeceef1), pad_hi: hex(0xffffff), pad_lo: hex(0xc9cdd3),
    well: hex(0xe3e6ea), ink: hex(0x1a1d23), muted: hex(0x5c6470), line: hex(0xd3d7dd), engrave: hex(0xb9bec6),
    accent: hex(0xe0982a), accent_ink: hex(0x1a1d23), focus: hex(0xe0982a), lamp_off: hex(0xc3c8cf),
    g_healed: hex(0x2fae6e), g_remedy: hex(0x2fae6e), g_noremedy: hex(0xd9503f), g_lost: hex(0xc8343a),
    g_silent: hex(0xa3203a), g_silent_ink: hex(0xffffff), sampled: hex(0x8a93a0), notrun: hex(0xc3c8cf),
    crt_bg: CRT_BG, crt_ink: CRT_INK, crt_dim: CRT_DIM, crt_red: CRT_RED, crt_amber: CRT_AMBER,
};

pub const DARK: Palette = Palette {
    bg: hex(0x0e0f12), panel: hex(0x1c2026), pad: hex(0x252a31), pad_hi: hex(0x343a43), pad_lo: hex(0x0a0b0d),
    well: hex(0x15181d), ink: hex(0xe9ebee), muted: hex(0x9aa3ae), line: hex(0x2c323a), engrave: hex(0x3a414b),
    accent: hex(0xe5a93d), accent_ink: hex(0x14110a), focus: hex(0xe5a93d), lamp_off: hex(0x3a414b),
    g_healed: hex(0x3fbf77), g_remedy: hex(0x3fbf77), g_noremedy: hex(0xe86a55), g_lost: hex(0xe05c5c),
    g_silent: hex(0xff5d7a), g_silent_ink: hex(0x14080b), sampled: hex(0x7d8794), notrun: hex(0x3a414b),
    crt_bg: CRT_BG, crt_ink: CRT_INK, crt_dim: CRT_DIM, crt_red: CRT_RED, crt_amber: CRT_AMBER,
};

/// No colour at all, for a frame or a lamp that draws only its stroke.
pub const NONE: Color32 = Color32::TRANSPARENT;

/// `--r-s`: controls, wells and chips.
pub const R_S: u8 = 6;
/// `--r-l`: plates.
pub const R_L: u8 = 10;
/// `--u`: the spacing grid.
pub const U: f32 = 4.0;

/// The page's density note: a table row, a control, and a plate's padding.
pub const ROW_HEIGHT: f32 = 28.0;
pub const CONTROL_HEIGHT: f32 = 32.0;
pub const PLATE_PADDING: f32 = 14.0;

/// The first family of the page's `--sans` and `--mono`, which the binary embeds.
pub const SANS_FAMILY: &str = "IBM Plex Sans";
pub const MONO_FAMILY: &str = "IBM Plex Mono";

/// The weights the page uses, each an egui family of its own: egui picks a font by family only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Sans,
    SansMedium,
    SansSemiBold,
    Mono,
    MonoMedium,
    MonoSemiBold,
}

impl Face {
    pub const ALL: [Face; 6] = [Face::Sans, Face::SansMedium, Face::SansSemiBold, Face::Mono, Face::MonoMedium, Face::MonoSemiBold];

    pub fn family(self) -> FontFamily {
        match self {
            Face::Sans => FontFamily::Proportional,
            Face::Mono => FontFamily::Monospace,
            other => FontFamily::Name(other.font_name().into()),
        }
    }

    /// The key of this face's font in the font definitions.
    pub fn font_name(self) -> &'static str {
        match self {
            Face::Sans => "plex-sans-400",
            Face::SansMedium => "plex-sans-500",
            Face::SansSemiBold => "plex-sans-600",
            Face::Mono => "plex-mono-400",
            Face::MonoMedium => "plex-mono-500",
            Face::MonoSemiBold => "plex-mono-600",
        }
    }
}

/// The page's type scale. `Label` is also drawn upper case and letter-spaced (`.engraved`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    /// Title 26/600.
    Title,
    /// Heading 17/600.
    Heading,
    /// Body 14/400.
    Body,
    /// A control's text: buttons 13/500, the segmented control and chips 12.5/500.
    Control,
    Chip,
    /// Label 10.5/600, engraved.
    Label,
    /// Small print under a field and beside a meter: 11.5/400.
    Small,
    /// Data mono 12.5/400, for scenario names and numbers.
    Data,
    /// Field input mono 13/400.
    Input,
    /// The step screen, mono 12/400.
    Screen,
    /// Readout mono 22/600.
    Readout,
}

impl Type {
    pub fn font(self) -> FontId {
        let (size, face) = match self {
            Type::Title => (26.0, Face::SansSemiBold),
            Type::Heading => (17.0, Face::SansSemiBold),
            Type::Body => (14.0, Face::Sans),
            Type::Control => (13.0, Face::SansMedium),
            Type::Chip => (12.5, Face::SansMedium),
            Type::Label => (10.5, Face::SansSemiBold),
            Type::Small => (11.5, Face::Sans),
            Type::Data => (12.5, Face::Mono),
            Type::Input => (13.0, Face::Mono),
            Type::Screen => (12.0, Face::Mono),
            Type::Readout => (22.0, Face::MonoSemiBold),
        };
        FontId::new(size, face.family())
    }

    /// The extra space between letters, in points: `.engraved`'s .12em, and none elsewhere.
    pub fn letter_spacing(self) -> f32 {
        match self {
            Type::Label => 10.5 * 0.12,
            _ => 0.0,
        }
    }
}

/// The palette egui is drawing with now.
pub fn palette(ctx: &egui::Context) -> &'static Palette {
    palette_of(ctx.theme())
}

pub fn palette_of(theme: egui::Theme) -> &'static Palette {
    match theme {
        egui::Theme::Light => &LIGHT,
        egui::Theme::Dark => &DARK,
    }
}

/// The page's sunk wells have a highlight along their lower edge in light only; in dark the shade
/// at the top is the whole effect (`--shadow-sink`).
pub fn sunk_lip(theme: egui::Theme) -> Option<Color32> {
    match theme {
        egui::Theme::Light => Some(LIGHT.pad_hi),
        egui::Theme::Dark => None,
    }
}

/// A 1 px stroke in one of the palette's colours.
pub fn hairline(color: Color32) -> Stroke {
    Stroke::new(1.0_f32, color)
}

/// egui's own widgets, dressed in the palette: text, fields and scroll bars that the desktop draws
/// through egui rather than through `widgets`.
pub fn visuals(theme: egui::Theme) -> Visuals {
    let p = palette_of(theme);
    let mut visuals = match theme {
        egui::Theme::Light => Visuals::light(),
        egui::Theme::Dark => Visuals::dark(),
    };
    visuals.override_text_color = Some(p.ink);
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.panel;
    visuals.window_stroke = hairline(p.line);
    visuals.window_corner_radius = CornerRadius::same(R_L);
    visuals.window_shadow = egui::epaint::Shadow::NONE;
    visuals.popup_shadow = egui::epaint::Shadow::NONE;
    visuals.extreme_bg_color = p.well;
    visuals.faint_bg_color = p.pad;
    visuals.code_bg_color = p.well;
    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.accent;
    visuals.error_fg_color = p.g_lost;
    visuals.selection.bg_fill = p.accent;
    visuals.selection.stroke = hairline(p.accent_ink);
    visuals.text_cursor.stroke = Stroke::new(2.0_f32, p.focus);
    let radius = CornerRadius::same(R_S);
    let states = [
        (&mut visuals.widgets.noninteractive, p.panel, p.line),
        (&mut visuals.widgets.inactive, p.pad, p.line),
        (&mut visuals.widgets.hovered, p.pad_hi, p.focus),
        (&mut visuals.widgets.active, p.well, p.focus),
        (&mut visuals.widgets.open, p.well, p.line),
    ];
    for (state, fill, stroke) in states {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = hairline(stroke);
        state.fg_stroke = hairline(p.ink);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    visuals.widgets.noninteractive.fg_stroke = hairline(p.muted);
    visuals
}

/// Spacing on the 4 px grid, for egui's own layout.
pub fn spacing(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(2.0 * U, 2.0 * U);
    style.spacing.button_padding = egui::vec2(3.5 * U, 2.0 * U);
    style.spacing.interact_size.y = CONTROL_HEIGHT;
    style.spacing.window_margin = egui::Margin::same(PLATE_PADDING as i8);
    style.text_styles = [
        (egui::TextStyle::Heading, Type::Heading.font()),
        (egui::TextStyle::Body, Type::Body.font()),
        (egui::TextStyle::Button, Type::Control.font()),
        (egui::TextStyle::Small, Type::Small.font()),
        (egui::TextStyle::Monospace, Type::Data.font()),
    ]
    .into();
}

/// The page's grade chips and LED meter well (`.grade`, `.leds`: 5 px), the pressed segment of the
/// segmented control (4 px), and an LED (2 px).
pub const CHIP_RADIUS: u8 = 5;
pub const SEGMENT_RADIUS: u8 = 4;
pub const LED_RADIUS: u8 = 2;
