//! The design system: every colour the GUI uses, by **role**.
//!
//! Colours are never written at a call site. A widget asks for `mention_self_bg`
//! or `danger`, and what that resolves to lives here, once. That is what makes
//! the interface consistent, and what makes a palette change a one-file edit
//! instead of a hunt through 10k lines.
//!
//! Identity: "Slate + Signal Indigo" — a cool slate neutral ramp with an indigo
//! accent. Dark-first; the [`Palette`] type is a plain value so a light ramp can
//! be added later without touching call sites.
//!
//! Rules the palette must keep (checked by the tests below): body text ≥ 4.5:1
//! on every surface it sits on, and UI/link/badge colours ≥ 3:1, per WCAG 2.1 AA.

use egui::{Color32, Stroke, Visuals};

/// `Color32::from_rgb`, usable in a `const` context.
const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// A copy of `c` at the given alpha (0–255).
///
/// Used for tints: a highlight behind text, a translucent scrim. Kept as a free
/// function because `Color32` is a foreign type. egui stores colour premultiplied
/// and only the premultiplied constructor is `const`, so the channels are
/// pre-scaled here; a `const` context then needs no runtime allocation.
const fn mul255(v: u8, a: u8) -> u8 {
    ((v as u32 * a as u32 + 127) / 255) as u8
}

pub const fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(mul255(c.r(), a), mul255(c.g(), a), mul255(c.b(), a), a)
}

/// Perceived brightness (0..1), used to pick egui's `dark_mode` default.
fn brightness(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

/// `"2eb886"` → a colour. Slack sends attachment/action colours as bare hex,
/// without the `#`. It lives here so the GUI never names a colour value: the
/// `no_raw_colours_outside_the_theme` test would otherwise flag it.
pub fn parse_hex(c: &str) -> Option<Color32> {
    let c = c.trim_start_matches('#');
    if c.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(c, 16).ok()?;
    Some(rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// Every colour role in the interface. One field per thing you can point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    // Surfaces, from the page back to the front.
    pub bg_base: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub bg_input: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,
    // Lines and separators.
    pub border_subtle: Color32,
    pub border_strong: Color32,
    // Text, by prominence.
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub text_on_accent: Color32,
    /// Near-black ink for text on a light chip (avatar initials, unread pill).
    pub ink: Color32,
    pub link: Color32,
    // The brand accent.
    pub accent: Color32,
    // Semantic states.
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub info: Color32,
    // Chat-specific roles.
    pub mention_self_bg: Color32,
    pub mention_self_fg: Color32,
    pub mention_other_fg: Color32,
    pub unread_badge_bg: Color32,
    pub unread_badge_fg: Color32,
    /// The red fill that carries white text: mention badges and the filled
    /// danger actions (log out). Deeper than `danger` so white on it clears 4.5:1.
    pub danger_fill: Color32,
    pub on_danger: Color32,
    pub reaction_bg: Color32,
    pub code_bg: Color32,
    pub quote_border: Color32,
    /// Gold for the favourite star (decoration, not a warning).
    pub favorite: Color32,
    // Presence.
    pub presence_active: Color32,
    pub presence_away: Color32,
    pub presence_quiet: Color32,
    // A full-screen dim behind modals/lightboxes, and the error-toast fill.
    pub scrim: Color32,
    pub danger_bg: Color32,
}

impl Palette {
    /// The dark identity. See the module docs; the contrast test guards it.
    pub const DARK: Palette = Palette {
        bg_base: rgb(0x0B, 0x0E, 0x14),
        bg_panel: rgb(0x09, 0x0C, 0x12),
        bg_elevated: rgb(0x16, 0x1B, 0x24),
        bg_input: rgb(0x11, 0x16, 0x1F),
        bg_hover: rgb(0x1C, 0x22, 0x30),
        bg_active: rgb(0x21, 0x31, 0x56),

        border_subtle: rgb(0x20, 0x28, 0x36),
        border_strong: rgb(0x2E, 0x3A, 0x4D),

        text_primary: rgb(0xE6, 0xEA, 0xF2),
        text_secondary: rgb(0x9A, 0xA6, 0xB8),
        text_muted: rgb(0x7E, 0x8A, 0x9B),
        text_on_accent: rgb(0x08, 0x12, 0x2B),
        ink: rgb(0x14, 0x16, 0x1A),
        link: rgb(0x79, 0xB8, 0xFF),

        accent: rgb(0x6E, 0x8B, 0xFF),

        success: rgb(0x3F, 0xB9, 0x50),
        warning: rgb(0xD9, 0xA4, 0x41),
        danger: rgb(0xE5, 0x53, 0x4B),
        info: rgb(0x58, 0xA6, 0xFF),

        mention_self_bg: rgb(0xE5, 0xB8, 0x4B),
        mention_self_fg: rgb(0x1A, 0x15, 0x08),
        mention_other_fg: rgb(0xE5, 0xB8, 0x4B),
        // A white pill with dark text is the "unread" convention; the mention
        // pill is a deeper red than `danger` so white on it clears 4.5:1.
        unread_badge_bg: rgb(0xE6, 0xEA, 0xF2),
        unread_badge_fg: rgb(0x14, 0x16, 0x1A),
        danger_fill: rgb(0xC4, 0x3C, 0x34),
        on_danger: rgb(0xFF, 0xFF, 0xFF),
        reaction_bg: rgb(0x1C, 0x22, 0x30),
        code_bg: rgb(0x16, 0x1B, 0x24),
        quote_border: rgb(0x6B, 0x76, 0x86),
        favorite: rgb(0xE8, 0xB8, 0x4B),

        presence_active: rgb(0x3F, 0xB9, 0x50),
        presence_away: rgb(0x6B, 0x76, 0x86),
        presence_quiet: rgb(0x8C, 0x8C, 0xE0),

        scrim: rgb(0x00, 0x00, 0x00),
        danger_bg: rgb(0x3A, 0x1E, 0x1E),
    };

    /// A translucent tint of the accent, for selection and highlight fills.
    pub const fn accent_tint(&self, alpha: u8) -> Color32 {
        with_alpha(self.accent, alpha)
    }

    /// A translucent tint of any role.
    pub const fn tint(&self, c: Color32, alpha: u8) -> Color32 {
        with_alpha(c, alpha)
    }

    /// Pushes the palette into egui's global `Visuals`.
    ///
    /// Geometry (type scale, spacing, rounding) is set separately in
    /// `install_style`; this is colours only. The text-colour mapping matters:
    /// egui derives `text_color()` from `noninteractive.fg_stroke` and
    /// `strong_text_color()` from `active.fg_stroke`, and fades `weak_text_color()`
    /// toward `noninteractive.weak_bg_fill` — so those three are chosen to make
    /// "default = primary, weak = dimmed toward the page, strong = primary".
    pub fn apply(&self, v: &mut Visuals) {
        // egui uses `dark_mode` for the fields we do not override (shadows,
        // striping); derive it from the page so a light theme gets light
        // defaults instead of dark ones.
        v.dark_mode = brightness(self.bg_base) < 0.5;
        v.override_text_color = None;

        v.panel_fill = self.bg_base;
        v.window_fill = self.bg_elevated;
        v.window_stroke = Stroke::new(1.0_f32, self.border_subtle);
        v.extreme_bg_color = self.bg_input;
        v.faint_bg_color = self.bg_hover;
        v.code_bg_color = self.code_bg;
        v.hyperlink_color = self.link;
        v.warn_fg_color = self.warning;
        v.error_fg_color = self.danger;

        // Text selection and the "on" state of toggles share this.
        v.selection.bg_fill = self.accent_tint(0x64);
        v.selection.stroke = Stroke::new(1.0_f32, self.accent);

        let w = &mut v.widgets;

        w.noninteractive.bg_fill = self.bg_panel;
        // egui derives `weak_text_color()` as a 50/50 blend of the text colour
        // and this "fade" target, and the ~60 `ui.weak(...)` call sites read it.
        // A dark target (the page) made that derived colour sub-AA on the
        // elevated/input surfaces. `bg_hover` is the lightest value that keeps
        // the derived weak text ≥ 4.5:1 there (`weak_text_is_readable`).
        w.noninteractive.weak_bg_fill = self.bg_hover;
        w.noninteractive.bg_stroke = Stroke::new(1.0_f32, self.border_subtle);
        w.noninteractive.fg_stroke = Stroke::new(1.0_f32, self.text_primary);

        w.inactive.bg_fill = self.bg_elevated;
        w.inactive.weak_bg_fill = self.bg_elevated;
        w.inactive.bg_stroke = Stroke::new(1.0_f32, self.border_subtle);
        w.inactive.fg_stroke = Stroke::new(1.0_f32, self.text_secondary);

        w.hovered.bg_fill = self.bg_hover;
        w.hovered.weak_bg_fill = self.bg_hover;
        w.hovered.bg_stroke = Stroke::new(1.0_f32, self.border_strong);
        w.hovered.fg_stroke = Stroke::new(1.0_f32, self.text_primary);

        w.active.bg_fill = self.bg_hover;
        w.active.weak_bg_fill = self.bg_hover;
        w.active.bg_stroke = Stroke::new(1.0_f32, self.accent);
        w.active.fg_stroke = Stroke::new(1.0_f32, self.text_primary);

        w.open.bg_fill = self.bg_elevated;
        w.open.weak_bg_fill = self.bg_elevated;
        w.open.bg_stroke = Stroke::new(1.0_f32, self.border_subtle);
        w.open.fg_stroke = Stroke::new(1.0_f32, self.text_primary);
    }
}

// --- The active palette (runtime) -----------------------------------------
//
// The palette is a value, not a `const`, so a theme can be installed at load.
// It is kept per-thread (the GUI is a single thread) and read on the UI thread,
// so a thread-local is the cheapest place for it: the ~86 call sites read
// `theme::palette().<role>` without threading a parameter through everything.

std::thread_local! {
    static ACTIVE: std::cell::Cell<Palette> = const { std::cell::Cell::new(Palette::DARK) };
}

/// The palette in force.
pub fn palette() -> Palette {
    ACTIVE.with(|c| c.get())
}

/// Replaces the palette in force. Only a theme mod does this (`mods`); without
/// the feature the palette is always the built-in one.
#[cfg(feature = "mods")]
pub fn set_palette(p: Palette) {
    ACTIVE.with(|c| c.set(p));
}

#[cfg(feature = "mods")]
macro_rules! roles {
    ($($variant:ident => $field:ident),+ $(,)?) => {
        /// One colour role, by name — the vocabulary a manifest theme may set.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Role { $($variant),+ }

        impl Role {
            /// Every role, in a stable order.
            pub const ALL: &'static [Role] = &[$(Role::$variant),+];

            /// The role's name, as it appears in a manifest theme.
            pub fn name(self) -> &'static str {
                match self { $(Role::$variant => stringify!($field)),+ }
            }

            /// The role with this name, if any.
            pub fn from_name(s: &str) -> Option<Role> {
                Self::ALL.iter().copied().find(|r| r.name() == s)
            }

            /// Sets this role in `p`.
            pub fn set(self, p: &mut Palette, c: Color32) {
                match self { $(Role::$variant => p.$field = c),+ }
            }
        }
    };
}

#[cfg(feature = "mods")]
roles! {
    BgBase => bg_base,
    BgPanel => bg_panel,
    BgElevated => bg_elevated,
    BgInput => bg_input,
    BgHover => bg_hover,
    BgActive => bg_active,
    BorderSubtle => border_subtle,
    BorderStrong => border_strong,
    TextPrimary => text_primary,
    TextSecondary => text_secondary,
    TextMuted => text_muted,
    TextOnAccent => text_on_accent,
    Ink => ink,
    Link => link,
    Accent => accent,
    Success => success,
    Warning => warning,
    Danger => danger,
    Info => info,
    MentionSelfBg => mention_self_bg,
    MentionSelfFg => mention_self_fg,
    MentionOtherFg => mention_other_fg,
    UnreadBadgeBg => unread_badge_bg,
    UnreadBadgeFg => unread_badge_fg,
    DangerFill => danger_fill,
    OnDanger => on_danger,
    ReactionBg => reaction_bg,
    CodeBg => code_bg,
    QuoteBorder => quote_border,
    Favorite => favorite,
    PresenceActive => presence_active,
    PresenceAway => presence_away,
    PresenceQuiet => presence_quiet,
    Scrim => scrim,
    DangerBg => danger_bg,
}

/// Avatar fills, chosen per user by a hash of the id.
///
/// Muted pastels that sit calmly on the dark surface; the initials are drawn in
/// a dark ink over them, so each must stay light.
pub const AVATAR: [Color32; 8] = [
    rgb(0xE8, 0xA0, 0x7A),
    rgb(0xD9, 0x8F, 0xB0),
    rgb(0xB2, 0x9D, 0xE0),
    rgb(0x8F, 0xB8, 0xE0),
    rgb(0x8F, 0xD0, 0xB6),
    rgb(0xE0, 0xCB, 0x8A),
    rgb(0xE0, 0x9A, 0x9A),
    rgb(0x9A, 0xC4, 0xD9),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance of an sRGB colour.
    fn luminance(c: Color32) -> f32 {
        fn lin(v: u8) -> f32 {
            let v = v as f32 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Every pair a reader actually meets, with the ratio AA asks for.
    fn checks(p: &Palette) -> Vec<(&'static str, Color32, Color32, f32)> {
        vec![
            ("body/base", p.text_primary, p.bg_base, 4.5),
            ("body/panel", p.text_primary, p.bg_panel, 4.5),
            ("body/elevated", p.text_primary, p.bg_elevated, 4.5),
            ("body/input", p.text_primary, p.bg_input, 4.5),
            ("body/hover", p.text_primary, p.bg_hover, 4.5),
            ("body/active", p.text_primary, p.bg_active, 4.5),
            ("body/danger_bg", p.text_primary, p.danger_bg, 4.5),
            ("secondary/base", p.text_secondary, p.bg_base, 4.5),
            ("secondary/panel", p.text_secondary, p.bg_panel, 4.5),
            ("secondary/elevated", p.text_secondary, p.bg_elevated, 4.5),
            ("muted/base", p.text_muted, p.bg_base, 4.5),
            ("muted/elevated", p.text_muted, p.bg_elevated, 4.5),
            ("link/base", p.link, p.bg_base, 3.0),
            ("accent/base", p.accent, p.bg_base, 3.0),
            ("on-accent/accent", p.text_on_accent, p.accent, 4.5),
            ("success/base", p.success, p.bg_base, 3.0),
            ("warning/base", p.warning, p.bg_base, 3.0),
            ("danger/base", p.danger, p.bg_base, 3.0),
            ("mention fg/bg", p.mention_self_fg, p.mention_self_bg, 4.5),
            ("unread badge", p.unread_badge_fg, p.unread_badge_bg, 4.5),
            ("danger fill", p.on_danger, p.danger_fill, 4.5),
            ("favorite/bg", p.favorite, p.bg_base, 3.0),
            ("presence active", p.presence_active, p.bg_panel, 3.0),
            ("presence away/panel", p.presence_away, p.bg_panel, 3.0),
            ("presence quiet", p.presence_quiet, p.bg_panel, 3.0),
        ]
    }

    #[test]
    fn dark_palette_meets_wcag_aa() {
        let p = Palette::DARK;
        let mut failures = Vec::new();
        for (name, fg, bg, need) in checks(&p) {
            let got = contrast(fg, bg);
            if got + 1e-3 < need {
                failures.push(format!("{name}: {got:.2} < {need}"));
            }
        }
        assert!(
            failures.is_empty(),
            "contrast below AA:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn parse_hex_reads_slack_colours() {
        assert_eq!(parse_hex("2eb886"), Some(rgb(0x2e, 0xb8, 0x86)));
        assert_eq!(parse_hex("#2EB886"), Some(rgb(0x2e, 0xb8, 0x86)));
        assert_eq!(parse_hex("12345"), None);
        assert_eq!(parse_hex("gggggg"), None);
    }

    /// `dark_mode` follows the page colour: a theme that paints a light page
    /// must get egui's light defaults, not dark ones.
    #[test]
    fn dark_mode_follows_the_page_colour() {
        let mut v = egui::Visuals::dark();
        Palette::DARK.apply(&mut v);
        assert!(v.dark_mode, "DARK paints a dark page");
        let mut light = Palette::DARK;
        light.bg_base = rgb(0xF4, 0xF5, 0xF7);
        light.apply(&mut v);
        assert!(!v.dark_mode, "a light page is not dark_mode");
    }

    /// `Role` must cover every `Palette` field, or a role would be silently
    /// un-settable by a theme. 35 today — bump this when the palette grows.
    #[cfg(feature = "mods")]
    #[test]
    fn every_palette_field_has_a_role() {
        assert_eq!(Role::ALL.len(), 35);
        for r in Role::ALL {
            assert_eq!(Role::from_name(r.name()), Some(*r), "round-trip {}", r.name());
        }
        let mut names: Vec<&str> = Role::ALL.iter().map(|r| r.name()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "role names must be unique");
    }

    /// The ~60 `ui.weak(...)` sites read egui's *derived* `weak_text_color()`,
    /// not a role — so it must be checked on the surfaces it is drawn on, via a
    /// real `Visuals` (not a re-implementation of egui's blend). base/panel/
    /// elevated/input are where weak text actually appears; the hover and active
    /// solids use primary text, so they are deliberately not checked.
    #[test]
    fn weak_text_is_readable() {
        let mut v = egui::Visuals::dark();
        Palette::DARK.apply(&mut v);
        let weak = v.weak_text_color();
        for (name, bg) in [
            ("base", Palette::DARK.bg_base),
            ("panel", Palette::DARK.bg_panel),
            ("elevated", Palette::DARK.bg_elevated),
            ("input", Palette::DARK.bg_input),
        ] {
            let got = contrast(weak, bg);
            assert!(got + 1e-3 >= 4.5, "weak text on {name}: {got:.2} < 4.5");
        }
    }
}
