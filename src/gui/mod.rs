//! Graphical front end (egui).
//!
//! The same application as the TUI: state and actions come from
//! [`crate::app`], here there is only the drawing and the input. It exists
//! because a terminal cannot show avatars, images and custom emoji — not
//! because Electron is inevitable: this window costs tens of MB, not an entire
//! browser.

use crate::app::images::Images;
use crate::app::mrkdwn::{self, Span};
use crate::app::{
    App, AppState, MessageEntry, NotifEntry, Reaction, SavedRow, ScheduledMessage, ThreadRow,
};
use crate::theme::{Palette, parse_hex};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The palette in force. Colours are asked for by role, here and in every call
/// site — never as a literal. It is a value, not a `const`, so a theme can
/// replace it at load (`crate::theme::set_palette`).
fn pal() -> Palette {
    crate::theme::palette()
}

/// The built-in palette with a mod's role overrides applied. Unknown roles and
/// malformed values are ignored (the default stays for them): a bad theme
/// degrades, it does not refuse.
#[cfg(feature = "mods")]
fn palette_from(theme: &[(String, String)]) -> Palette {
    let mut p = Palette::DARK;
    for (role, hex) in theme {
        if let (Some(r), Some(c)) = (crate::theme::Role::from_name(role), parse_hex(hex)) {
            r.set(&mut p, c);
        }
    }
    p
}

fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Downscales a captured RGBA buffer to a preview width, preserving aspect.
#[cfg(feature = "vp8")]
fn downscale_thumbnail(rgba: &[u8], width: u32, height: u32) -> Option<(u32, u32, Vec<u8>)> {
    const MAX_W: u32 = 320;
    if width == 0 || height == 0 {
        return None;
    }
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    let scale = (MAX_W as f32 / width as f32).min(1.0);
    let tw = ((width as f32 * scale).round() as u32).max(1);
    let th = ((height as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    Some((tw, th, resized.into_raw()))
}

/// A key meant for the completion, not for the composer.
#[derive(Clone, Copy)]
enum CompletionKey {
    Up,
    Down,
    Choose,
    Cancel,
}

/// A line's height in the composer, and the vertical padding around its text.
const COMPOSER_ROW: f32 = 18.0;
const COMPOSER_PAD: f32 = 12.0;

/// How many lines the composer shows before it stops growing and scrolls.
const COMPOSER_MAX_ROWS: usize = 8;

/// Composer height from a newline count (no wrapping known).
fn composer_height(text: &str) -> f32 {
    let rows = (text.lines().count().max(1)).min(COMPOSER_MAX_ROWS);
    COMPOSER_PAD + rows as f32 * COMPOSER_ROW
}

/// The composer box's height for `text` laid out at `width`, and whether it has
/// to be bounded (scroll inside). The height comes from the **wrapped** text: a
/// single long line wraps into several rows in a narrow box (`lines()` would
/// count one and miss it), so the height is measured from the laid-out galley and
/// capped at [`COMPOSER_MAX_ROWS`].
fn composer_box(ui: &egui::Ui, text: &str, width: f32) -> (f32, bool) {
    // Mirror the `TextEdit`'s default font selection (`FontSelection::Default`).
    let font = ui
        .style()
        .override_font_id
        .clone()
        .unwrap_or_else(|| egui::TextStyle::Body.resolve(ui.style()));
    let (galley, line) = ui.fonts(|f| {
        (
            f.layout(
                text.to_owned(),
                font.clone(),
                egui::Color32::PLACEHOLDER,
                width.max(1.0),
            ),
            // One line's height, to turn the wrapped height into a row count
            // without assuming a row size (which is font- and scale-dependent).
            f.layout(
                "x".to_owned(),
                font,
                egui::Color32::PLACEHOLDER,
                f32::INFINITY,
            ),
        )
    });
    let line_h = line.size().y.max(1.0);
    let rows = ((galley.size().y / line_h).ceil() as usize).max(1);
    let h = COMPOSER_PAD + rows.min(COMPOSER_MAX_ROWS) as f32 * COMPOSER_ROW;
    (h, rows > COMPOSER_MAX_ROWS)
}

/// Side of a composer button: a fixed square, so it never stretches with the
/// growing text box.
const COMPOSER_BTN_W: f32 = 32.0;

/// Size of the icons inside the composer buttons. Explicit so the
/// "icon + 2×padding fits the slot" invariant is checkable by a test.
const COMPOSER_ICON: f32 = 16.0;

/// Horizontal button padding from the theme. It must satisfy
/// `COMPOSER_ICON + 2 * BUTTON_PADDING_X <= COMPOSER_BTN_W`: otherwise the
/// composer buttons are wider than their fixed slot, overflow it, and a
/// content-sized panel (the thread) grows a sliver every frame. There is a
/// test for this below (`composer_icon_buttons_fit_their_slot`).
const BUTTON_PADDING_X: f32 = 6.0;

/// Avatar square, and how round its corners are. Shared by the downloaded image
/// and the initials plate drawn in its place, so the row does not shift.
const AVATAR_SIZE: f32 = 36.0;

/// The huddle participant tile, in the strip below a shared screen: a 72px
/// avatar and a one-line name inside a 150px frame. The margins mirror the
/// tile's `Frame`, so the strip can reserve exactly one row.
const HUDDLE_AVATAR: f32 = 72.0;
const HUDDLE_TILE_W: f32 = 150.0;
const HUDDLE_MARGIN: f32 = 10.0;
const HUDDLE_GAP: f32 = 6.0;
const HUDDLE_LEADING: f32 = 10.0;
const AVATAR_ROUNDING: f32 = 10.0;

/// The coloured bar on the left of an attachment, and the gap after it. The bar
/// is 3 wide: leaving 3 left the text stuck to it. Shared with the test that
/// pins the bar's presence.
const ATTACHMENT_STRIPE_W: f32 = 3.0;
const ATTACHMENT_STRIPE_GAP: f32 = 8.0;

/// How long the status line stays next to the connection dot before it fades:
/// the dot is the durable part, a stale "N conversations" is just noise.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(8);

/// How often to assert "active" while the app runs. Slack's `auto` presence is
/// decided server-side from activity, so this is a keepalive, not the mechanism —
/// and it is not gated on focus (see `drive_presence`).
const PRESENCE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether the presence keepalive is due: at most once per [`PRESENCE_KEEPALIVE`],
/// and never while the user has explicitly chosen **Away** — that is the *only* way
/// to be away. It runs while the **app** is running, regardless of whether its
/// window is in the foreground: working in another window is not being away, and
/// gating on focus is what let the account drop offline after a while.
///
/// Note what this can and cannot do: `users.setPresence(auto)` clears a *manual*
/// away, but Slack has no call that forces a user active — presence is Slack's to
/// decide from a connected, non-idle client. So the assert is paired with a
/// read-back (logged) rather than assumed to have worked.
fn presence_keepalive_due(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    manual_away: bool,
) -> bool {
    !manual_away && last.map_or(true, |t| now.duration_since(t) >= PRESENCE_KEEPALIVE)
}

/// Places a composer button as a square anchored to the **bottom** of a region
/// as tall as the text box, so attach/save/send line up along its bottom edge.
fn composer_button<R>(ui: &mut egui::Ui, h: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(COMPOSER_BTN_W, h),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            // Push the square down inside the region: a shorter widget in a
            // taller row would otherwise sit in the middle.
            ui.add_space((h - COMPOSER_BTN_W).max(0.0));
            add(ui)
        },
    )
    .inner
}

/// True if the user pressed Enter to send.
///
/// With Shift held the multiline `TextEdit` inserts a newline and should be
/// left to: that is how structured messages and code blocks get written.
fn sent(resp: &egui::Response, ui: &egui::Ui) -> bool {
    resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift)
}

/// The most used reactions, at the top of the picker.
const QUICK_REACTIONS: &[&str] = &[
    "+1",
    "eyes",
    "white_check_mark",
    "tada",
    "heart",
    "joy",
    "pray",
    "rocket",
    "fire",
    "100",
];

/// Suggested statuses in the profile popover: the ones that answer "where are
/// you" at a glance.
const QUICK_STATUS: &[&str] = &[
    "speech_balloon",
    "spiral_calendar_pad",
    "palm_tree",
    "house",
    "face_with_thermometer",
    "airplane",
    "coffee",
    "eyes",
    "no_entry_sign",
    "bulb",
];

/// Upper bound on the URL image cache.
///
/// Avatars and custom emoji are small and reused, but nothing used to free
/// them: over a long session every image ever shown stayed on the GPU. Keeping
/// the most recent few hundred covers what a screen can display, and the rest
/// is downloaded again on demand — the bytes are untouched.
const TEXTURE_CACHE_MAX: usize = 512;

// Animated images (GIF) are kept apart from the single-frame cache: one holds
// every frame, so it costs several textures, and an uncapped set of them would
// defeat the whole point of a light client. Three caps keep it bounded:
//  - a single GIF is decoded to at most this many frames (a pathological
//    thousand-frame file is truncated, not unrolled);
const MAX_ANIM_FRAMES: usize = 120;
//  - a single GIF wider than this many decoded bytes is *not* animated — its
//    first frame is shown statically, so one huge clip cannot blow the budget;
const ANIM_ITEM_BYTES_MAX: usize = 24 * 1024 * 1024;
//  - and the animated set as a whole is evicted, oldest first, back under this.
const ANIM_TOTAL_BYTES_MAX: usize = 48 * 1024 * 1024;

// How many older stretches a pending jump may pull in before giving up. Each
// one is a network round trip, so the bound keeps a missing target from
// loading the whole history.

/// Frames a jump may stay pending without progress before it is abandoned. It
/// is the last-resort safety: a request that never answers must not leave the
/// list stuck reading backwards.
const JUMP_MAX_FRAMES: usize = 1200;

const STAR: &str = "\u{2605}";

/// How a wrapped label may break when it runs out of width.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wrap {
    /// Break only at word boundaries: ordinary prose never splits a word.
    Words,
    /// Break inside a token when there is no other opportunity: a long URL or
    /// identifier has nowhere else to go.
    Anywhere,
}

/// A wrapping label.
///
/// egui breaks only at word boundaries, and a hundred-character URL or a hash
/// is a single word: they blow through the container and drag the whole message
/// list with them. A first attempt inserted zero-width spaces, but `U+200B` is
/// not whitespace and the layout does not treat them as break opportunities: it
/// did nothing. `break_anywhere` is the real switch, but it also cuts ordinary
/// words in half, so it is reserved for [`Wrap::Anywhere`].
fn wrapped(
    text: impl Into<String>,
    font: egui::FontId,
    color: egui::Color32,
    wrap: Wrap,
) -> egui::Label {
    let mut job = egui::text::LayoutJob::simple(text.into(), font, color, f32::INFINITY);
    job.wrap.break_anywhere = wrap == Wrap::Anywhere;
    egui::Label::new(job)
}

/// The version line in the profile popover: crate version, build date and git
/// hash.
///
/// Fixed under `cfg(test)`: it would otherwise change on every commit and make
/// the UI snapshot of the popover fail for a reason unrelated to the interface.
fn build_info() -> String {
    if cfg!(test) {
        return "slaq <version>  ·  <date>  ·  <hash>".to_string();
    }
    format!(
        "slaq {}  ·  {}  ·  {}",
        env!("CARGO_PKG_VERSION"),
        env!("SLAQ_BUILD_DATE"),
        env!("SLAQ_GIT_HASH"),
    )
}

/// Built-in slash commands offered while typing `/` at the start of the
/// composer. Slack has no endpoint to list them (custom ones are workspace
/// state the web client hardcodes), so this is the common set — anything else
/// can still be typed and run.
const SLASH_COMMANDS: &[&str] = &[
    "/me",
    "/remind",
    "/mute",
    "/dnd",
    "/status",
    "/away",
    "/active",
    "/shrug",
    "/tableflip",
    "/unflip",
    "/giphy",
    "/invite",
    "/leave",
    "/collapse",
    "/expand",
    "/topic",
    "/who",
    "/apps",
    "/help",
];

/// The command fragment being typed at the very start of `text`, if any.
///
/// A slash command only counts at the beginning, before the first space and
/// with no further slash (so `/usr/bin` is not a command).
fn slash_command_fragment(text: &str, byte_cursor: usize) -> Option<&str> {
    let upto = text.get(..byte_cursor)?;
    let after = upto.strip_prefix('/')?;
    if after.contains(char::is_whitespace) || after.contains('/') {
        return None;
    }
    Some(after)
}

/// Installs the bundled fonts: the full Noto Sans family for text and the
/// complete Noto Emoji as fallback.
///
/// egui's default proportional face has no bold or italic variant, so emphasis
/// could only be a brighter colour. Noto Sans ships as static per-weight faces
/// — unlike, say, the locally available Roboto, whose variable font egui cannot
/// pick weights from. Noto Sans Regular leads the proportional family; the
/// other three weights back the named families used for emphasis.
///
/// The complete Noto Emoji sits at the end of every family: the earlier fonts
/// take precedence for letters and it steps in where they have no glyph. egui
/// includes only a subset of 752 emoji, and the rest — the `:spiral_calendar_pad:`
/// of someone in a meeting, half the flags, many professions — were empty boxes.
///
/// It is 865 KiB in the binary. Against a client that takes little more than a
/// hundred MB in memory it is a negligible fraction, and the glyph cache only
/// grows for the glyphs actually drawn.
fn install_fonts(ctx: &egui::Context) {
    const SANS_REGULAR: &[u8] = include_bytes!("data/NotoSans-Regular.ttf");
    const SANS_BOLD: &[u8] = include_bytes!("data/NotoSans-Bold.ttf");
    const SANS_ITALIC: &[u8] = include_bytes!("data/NotoSans-Italic.ttf");
    const SANS_BOLD_ITALIC: &[u8] = include_bytes!("data/NotoSans-BoldItalic.ttf");
    const NOTO: &[u8] = include_bytes!("data/NotoEmoji-Regular.ttf");
    // Subset of Noto Sans Symbols 2: the geometric shapes, stars, check marks
    // and other symbols a message turns up (`○ ● ★ ☆ ✓ ✗`) that Noto Sans has
    // no glyph for. Subsetted to those blocks (91 KiB instead of 656) because
    // `Hack` already covers the arrows and math operators.
    const SYMBOLS2: &[u8] = include_bytes!("data/NotoSansSymbols2-Subset.ttf");

    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        ("noto-sans-regular", SANS_REGULAR),
        ("noto-sans-bold", SANS_BOLD),
        ("noto-sans-italic", SANS_ITALIC),
        ("noto-sans-bolditalic", SANS_BOLD_ITALIC),
        ("noto-sans-symbols2", SYMBOLS2),
        ("noto-emoji", NOTO),
    ] {
        fonts.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(bytes)),
        );
    }

    // Regular leads; egui's remaining defaults stay as fallback, emoji last.
    let proportional = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    proportional.insert(0, "noto-sans-regular".to_owned());
    // Symbols the text faces have no glyph for, in this order: Noto Sans
    // Symbols 2 first (geometric shapes, stars, check marks — `○ ● ★ ☆ ✓ ✗`),
    // then `Hack` (egui's monospace default, already bundled) for the arrows,
    // math operators and box-drawing only it has (`→ ⇒ ≤ ─`). Without a fallback
    // all of those rendered as a tofu box.
    if fonts.font_data.contains_key("noto-sans-symbols2") {
        proportional.insert(1, "noto-sans-symbols2".to_owned());
    }
    if fonts.font_data.contains_key("Hack") {
        proportional.push("Hack".to_owned());
    }
    proportional.push("noto-emoji".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("noto-emoji".to_owned());

    // Each named family starts with its face and falls back to the proportional
    // chain, so emoji and non-Latin text keep rendering there too.
    let fallback = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for (name, face) in [
        ("bold", "noto-sans-bold"),
        ("italic", "noto-sans-italic"),
        ("bolditalic", "noto-sans-bolditalic"),
    ] {
        let mut chain = vec![face.to_owned()];
        chain.extend(fallback.iter().cloned());
        fonts
            .families
            .insert(egui::FontFamily::Name(name.into()), chain);
    }
    ctx.set_fonts(fonts);
}

/// The look and feel: type scale, spacing and widget states.
///
/// egui's defaults are tuned for a maximally dense developer tool and read as
/// "everything is small" on a chat client, especially on a high-DPI screen. It
/// also varies a widget's `expansion` between states, which is what makes a
/// button visibly grow under the pointer. Here both are pinned: a slightly
/// larger, calmer type scale, and widget states that only change colour.
fn install_style(ctx: &egui::Context) {
    use egui::{FontFamily, FontId, TextStyle};

    let mut style = (*ctx.style()).clone();

    // Colours come from the design system (one source of truth); this function
    // keeps only geometry — type scale, spacing, rounding.
    pal().apply(&mut style.visuals);

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(14.0, FontFamily::Monospace),
        ),
    ]
    .into();

    let sp = &mut style.spacing;
    sp.item_spacing = egui::vec2(6.0, 3.0);
    // Keep the horizontal padding small: the composer's icon buttons are
    // allocated at a fixed `COMPOSER_BTN_W`, and a button whose icon + 2×padding
    // is wider than that overflows its slot. Inside a content-sized panel
    // (the thread) that overflow made the panel grow a sliver every frame.
    sp.button_padding = egui::vec2(BUTTON_PADDING_X, 3.0);
    sp.interact_size = egui::vec2(28.0, 26.0);
    sp.window_margin = egui::Margin::same(10.0);
    sp.menu_margin = egui::Margin::same(8.0);
    // Floating scrollbars overlay the content instead of reserving a strip,
    // which in the dark theme is a hard black column (it was showing between
    // the sidebar and the conversation).
    sp.scroll.floating = true;
    sp.scroll.bar_width = 9.0;

    // One corner radius everywhere, and no size change between states.
    let radius = egui::Rounding::same(8.0);
    style.visuals.window_rounding = egui::Rounding::same(10.0);
    style.visuals.menu_rounding = egui::Rounding::same(8.0);
    for w in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        w.rounding = radius;
        // `expansion` inflates the widget rectangle; leaving it non-zero is the
        // "button grows on hover" effect. Colour alone marks the state.
        w.expansion = 0.0;
    }

    // egui shows a cursor only for `Button`, and only if the app opts in.
    // Without this, every button — the saved icon, "Chiama", the dialog
    // buttons — hovered with no pointer change at all. `enforce_pointer_cursor`
    // is the safety net for the widgets egui never handles.
    style.visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

    ctx.set_style(style);
}

/// The colour emoji, served by Slack itself.
///
/// They are the Google ones, the same you see in the official client, and they
/// live on the host avatars and workspace emoji are already downloaded from: no
/// extra third party. The monochrome font stays as a fallback, for emoji newer
/// than this set and for when there is no network.
const EMOJI_CDN: &str =
    "https://a.slack-edge.com/production-standard-emoji-assets/14.0/google-medium";

/// Like [`wrapped`], but applying the inherited emphasis.
///
/// It is needed because `RichText` on its own cannot carry a `LayoutJob`'s wrap
/// mode: the emphasis has to go in the format instead of on the `RichText`.
fn wrapped_emph(
    text: impl Into<String>,
    mut font: egui::FontId,
    color: egui::Color32,
    emph: Emph,
    wrap: Wrap,
) -> egui::Label {
    // The emphasis is a real face, not a synthetic slant: overriding the family
    // here keeps colour decisions at the call site.
    font.family = emph.family();
    let mut job = egui::text::LayoutJob::default();
    job.wrap.break_anywhere = wrap == Wrap::Anywhere;
    job.append(
        &text.into(),
        0.0,
        egui::TextFormat {
            font_id: font,
            color,
            italics: false,
            strikethrough: if emph.strike {
                egui::Stroke::new(1.0_f32, color)
            } else {
                egui::Stroke::NONE
            },
            ..Default::default()
        },
    );
    egui::Label::new(job)
}

/// The emphasis a span inherits from the ones containing it.
#[derive(Clone, Copy, Default)]
struct Emph {
    bold: bool,
    italic: bool,
    strike: bool,
}

impl Emph {
    fn bold(self) -> Self {
        Emph { bold: true, ..self }
    }
    fn italic(self) -> Self {
        Emph {
            italic: true,
            ..self
        }
    }
    fn strike(self) -> Self {
        Emph {
            strike: true,
            ..self
        }
    }

    /// The named font family with the real face for this emphasis.
    ///
    /// Falls back to the proportional family when none is asked for. The named
    /// families all fall back to the proportional chain, so this is safe for
    /// any script or emoji.
    fn family(self) -> egui::FontFamily {
        match (self.bold, self.italic) {
            (true, true) => egui::FontFamily::Name("bolditalic".into()),
            (true, false) => egui::FontFamily::Name("bold".into()),
            (false, true) => egui::FontFamily::Name("italic".into()),
            (false, false) => egui::FontFamily::Proportional,
        }
    }
}

/// A button with a close cross, drawn.
///
/// `\u{2715}` does not exist in the fonts egui loads — like the triangles and
/// the presence dot — and was drawn as an empty box. Two segments cost less
/// than a dependency on a font.
fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let size = egui::vec2(18.0, 18.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let c = rect.center();
    let r = 4.0;
    let col = if resp.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let stroke = egui::Stroke::new(1.6_f32, col);
    let p = ui.painter();
    p.line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
    p.line_segment([c + egui::vec2(r, -r), c + egui::vec2(-r, r)], stroke);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// Adds a selectable row and shows the pointing hand while it is hovered.
///
/// egui's `SelectableLabel` never sets a cursor (unlike `Button` and `Link`),
/// so every selectable — the saved/pinned tabs, the sidebar entries, the
/// channel rows — needs it added by hand.
fn selectable(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    let r = ui.selectable_label(selected, text);
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// A centred modal with the design-system backdrop and sheet.
///
/// Unlike a floating `Window` — which does not dim what is behind it — a modal
/// paints the palette scrim and blocks input to the rest of the UI, which is
/// what makes a dialog read as a decision and not a popover. Closing on the
/// backdrop or Esc is [`egui::ModalResponse::should_close`].
fn modal(ctx: &egui::Context, id: &str) -> egui::Modal {
    egui::Modal::new(egui::Id::new(id))
        .backdrop_color(pal().tint(pal().scrim, 0x8C))
        .frame(egui::Frame::popup(&ctx.style()).fill(pal().bg_elevated))
}

/// Shows the pointing hand over anything that would be clicked.
///
/// egui sets a cursor for a few widgets only — `Button` when
/// `visuals.interact_cursor` is set, `Link`, `TextEdit` — while a
/// `SelectableLabel`, an `Image` or a hand-made clickable `Label` has to set it
/// itself, which is easy to forget and easy to miss in review. This runs once at
/// the end of the frame: `egui` already knows what a click right now would hit,
/// so if that is a widget and nothing more specific was claimed (a text cursor
/// over an input, a resize cursor over a splitter), show the pointing hand. One
/// place, so "clickable means a hand" holds no matter how a widget was built.
fn enforce_pointer_cursor(ctx: &egui::Context) {
    use egui::CursorIcon;
    if ctx.output(|o| o.cursor_icon) != CursorIcon::Default {
        return; // a more specific cursor already won this frame
    }
    if ctx.viewport(|vp| vp.hits.click.is_some()) {
        ctx.set_cursor_icon(CursorIcon::PointingHand);
    }
}

/// A labeled block in the channel info panel: topic or purpose.
fn info_block(ui: &mut egui::Ui, title: &str, value: Option<&str>) {
    ui.label(egui::RichText::new(title).small().strong());
    match value.filter(|v| !v.trim().is_empty()) {
        Some(v) => {
            ui.label(v);
        }
        None => {
            ui.weak("—");
        }
    }
    ui.add_space(6.0);
}

fn initials_of(name: &str) -> String {
    let mut initials = String::new();
    for word in name.split_whitespace().take(2) {
        if let Some(c) = word.chars().next() {
            initials.extend(c.to_uppercase());
        }
    }
    if initials.is_empty() {
        "?".to_string()
    } else {
        initials
    }
}

/// How close two messages from the same author must be to share one header.
const GROUP_WINDOW_SECS: i64 = 5 * 60;

/// The seconds part of a Slack `ts` (`"1700000000.000100"`).
fn ts_secs(ts: &str) -> Option<i64> {
    ts.split('.').next()?.parse().ok()
}

/// Whether `cur` continues the message above it: same author, same day, within
/// [`GROUP_WINDOW_SECS`]. A run like that drops the repeated avatar and name, so
/// one person's paragraphs read as a block instead of a column of headers.
fn is_continuation(prev: Option<&MessageEntry>, cur: &MessageEntry) -> bool {
    let Some(prev) = prev else { return false };
    if cur.user.is_empty() || prev.user != cur.user {
        return false;
    }
    let (Some(a), Some(b)) = (ts_secs(&prev.ts), ts_secs(&cur.ts)) else {
        return false;
    };
    // `checked_sub`: the two timestamps come from parsing untrusted text, so a
    // hostile pair must not overflow. A negative or over-large gap is not a
    // continuation.
    if !b
        .checked_sub(a)
        .is_some_and(|d| (0..=GROUP_WINDOW_SECS).contains(&d))
    {
        return false;
    }
    // The day boundary is the **reader's**, matching how the app renders
    // timestamps (`Local`). Grouping on the UTC day broke a run at the wrong
    // midnight for anyone not on UTC.
    same_day_in(&chrono::Local, a, b)
}

/// Whether two instants fall on the same calendar day in `tz`.
///
/// Factored out so the boundary is testable with a **fixed offset**: a test that
/// only used `Local` would pass on a UTC machine even with the old UTC rule.
fn same_day_in<Tz: chrono::TimeZone>(tz: &Tz, a: i64, b: i64) -> bool {
    let day = |s: i64| tz.timestamp_opt(s, 0).single().map(|d| d.date_naive());
    matches!((day(a), day(b)), (Some(x), Some(y)) if x == y)
}

/// A rounded count badge — a filled capsule with a small bold number, the
/// "pill" the design calls for (`design-system/slaq/MASTER.md`). egui's
/// `RichText::background_color` paints a tight rectangle; a pill needs its own
/// rounded frame, so the number sits inside a filled, generously rounded frame.
fn pill(ui: &mut egui::Ui, text: &str, fg: egui::Color32, bg: egui::Color32) -> egui::Response {
    egui::Frame::none()
        .fill(bg)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().strong().color(fg));
        })
        .response
}

/// Whether a finished sidebar drag should be persisted, updating the locally
/// committed width in place. Returns `true` only on a pointer release whose
/// settled width actually differs from the last committed one — a real resize.
///
/// Comparing against a locally tracked `committed` value (not the persisted
/// snapshot) is deliberate: the snapshot lags a frame behind a save and eframe
/// persists egui's own panel memory independently, so the live width can differ
/// from the snapshot for a benign reason. Comparing against the snapshot would
/// let the first pointer release anywhere (an ordinary click) persist a width
/// the user never dragged. The committed value is seeded on the first observed
/// width (no release yet), so the initial layout is never persisted.
fn sidebar_persist_decision(committed: &mut Option<f32>, width: f32, released: bool) -> bool {
    match *committed {
        None => {
            *committed = Some(width);
            false
        }
        Some(_) if !released => false,
        Some(prev) => {
            *committed = Some(width);
            (width - prev).abs() > 0.5
        }
    }
}

#[cfg(test)]
mod sidebar_persist_tests {
    use super::sidebar_persist_decision;

    #[test]
    fn first_frame_seeds_and_never_persists() {
        let mut c = None;
        // Even with a release on the very first observed width, nothing is saved:
        // that is the initial layout, not a drag.
        assert!(!sidebar_persist_decision(&mut c, 230.0, true));
        assert_eq!(c, Some(230.0));
    }

    #[test]
    fn a_plain_click_without_a_resize_does_not_persist() {
        let mut c = Some(230.0);
        // Pointer released but the width did not move: an ordinary click. This is
        // the regression — comparing against a lagging snapshot used to save here.
        assert!(!sidebar_persist_decision(&mut c, 230.0, true));
        assert_eq!(c, Some(230.0));
    }

    #[test]
    fn a_finished_drag_persists_the_new_width() {
        let mut c = Some(230.0);
        assert!(sidebar_persist_decision(&mut c, 300.0, true));
        assert_eq!(c, Some(300.0), "the committed width follows the drag");
        // A second release at the same width is now a no-op.
        assert!(!sidebar_persist_decision(&mut c, 300.0, true));
    }

    #[test]
    fn a_drag_in_progress_without_a_release_does_not_persist() {
        let mut c = Some(230.0);
        // Width is changing but the button is still down: wait for the release.
        assert!(!sidebar_persist_decision(&mut c, 260.0, false));
        assert_eq!(c, Some(230.0), "committed only moves on release");
    }

    #[test]
    fn a_sub_pixel_wobble_is_ignored() {
        let mut c = Some(230.0);
        assert!(!sidebar_persist_decision(&mut c, 230.3, true));
    }
}

/// A date header between two days in the conversation, like the official
/// client: a muted, centred label with a hairline rule on each side. The label
/// is `today`/`yesterday`/the full date (`app::format_day`).
fn day_divider(ui: &mut egui::Ui, ts: &str) {
    let label = crate::app::format_day(ts);
    if label.is_empty() {
        return;
    }
    ui.add_space(8.0);
    let font = ui.style().text_styles[&egui::TextStyle::Small].clone();
    let galley = ui.painter().layout_no_wrap(label, font, pal().text_muted);
    let sz = galley.size();
    // One allocation of *exactly* the available width, then paint into it. The
    // earlier version sub-allocated the two rules and the label separately, and
    // egui inserts `item_spacing` between allocations in a row — so the divider
    // came out a little wider than the column. With the conversation scroll area
    // set to `auto_shrink([false, false])` that surplus never shrinks back: it
    // accumulated frame after frame, inflating `available_width` for every
    // message until the text ran off the right edge. A single allocation cannot
    // overflow, so the column width stays put.
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), sz.y), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.0, pal().border_subtle);
    let cy = rect.center().y;
    let gap = 8.0;
    let tx = rect.left() + ((rect.width() - sz.x) / 2.0).max(0.0);
    let p = ui.painter();
    if tx - gap > rect.left() {
        p.hline(rect.left()..=(tx - gap), cy, stroke);
    }
    p.galley(egui::pos2(tx, rect.top()), galley, pal().text_muted);
    if tx + sz.x + gap < rect.right() {
        p.hline((tx + sz.x + gap)..=rect.right(), cy, stroke);
    }
    ui.add_space(8.0);
}

/// The frames of an animated GIF, already uploaded, with each frame's on-screen
/// duration. `start` is when it began playing; the frame to show is derived from
/// the elapsed time modulo `total`, so there is no per-frame state to tick.
struct AnimatedTexture {
    frames: Vec<egui::TextureHandle>,
    durations: Vec<std::time::Duration>,
    total: std::time::Duration,
    /// Decoded size across all frames, for the byte budget.
    bytes: usize,
    start: std::time::Instant,
    /// The `Gui::frame_seq` value when this GIF was last drawn. Eviction runs at
    /// the top of a frame, before anything is drawn, and never evicts a GIF seen
    /// on the previous frame — otherwise an on-screen GIF whose own bytes push
    /// the set over budget would be evicted and re-decoded (120 frames) every
    /// single frame, a thrash loop.
    last_seen: u64,
}

/// What `decode_image` made of some bytes: one frame, or several to animate.
enum Decoded {
    Static(egui::ColorImage),
    Animated(Vec<(egui::ColorImage, std::time::Duration)>),
}

/// Decode image bytes, detecting an animated GIF. A multi-frame GIF within the
/// byte budget comes back as `Animated`; anything else (a still, a one-frame
/// GIF, or a GIF too large to animate) comes back as `Static` — the first frame.
fn decode_image(bytes: &[u8]) -> Option<Decoded> {
    let is_gif = image::guess_format(bytes)
        .map(|f| f == image::ImageFormat::Gif)
        .unwrap_or(false);
    if is_gif {
        use image::AnimationDecoder;
        if let Ok(dec) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)) {
            if let Ok(frames) = dec.into_frames().collect_frames() {
                if frames.len() > 1 {
                    let mut out = Vec::new();
                    let mut bytes_total = 0usize;
                    for frame in frames.into_iter().take(MAX_ANIM_FRAMES) {
                        let (num, den) = frame.delay().numer_denom_ms();
                        // A GIF's delay is in centiseconds; a 0 or near-0 delay is
                        // the author saying "as fast as you can", which browsers
                        // clamp to ~100ms. Match that so it is not a strobe.
                        let mut ms = if den == 0 { 0 } else { (num / den) as u64 };
                        if ms < 20 {
                            ms = 100;
                        }
                        let buf = frame.into_buffer();
                        let size = [buf.width() as usize, buf.height() as usize];
                        bytes_total += size[0] * size[1] * 4;
                        out.push((
                            egui::ColorImage::from_rgba_unmultiplied(size, buf.as_raw()),
                            std::time::Duration::from_millis(ms),
                        ));
                    }
                    if out.len() > 1 {
                        if bytes_total <= ANIM_ITEM_BYTES_MAX {
                            return Some(Decoded::Animated(out));
                        }
                        // Too big to animate within budget: the first frame, still.
                        return Some(Decoded::Static(out.into_iter().next().unwrap().0));
                    }
                }
            }
        }
    }
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(Decoded::Static(egui::ColorImage::from_rgba_unmultiplied(
        size,
        img.as_raw(),
    )))
}

#[cfg(test)]
mod anim_tests {
    use super::{Decoded, decode_image};

    fn two_frame_gif() -> Vec<u8> {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            for colour in [Rgba([255, 0, 0, 255]), Rgba([0, 255, 0, 255])] {
                let img = RgbaImage::from_pixel(2, 2, colour);
                enc.encode_frame(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
            }
        }
        buf
    }

    #[test]
    fn a_multi_frame_gif_decodes_as_animated() {
        match decode_image(&two_frame_gif()) {
            Some(Decoded::Animated(frames)) => assert_eq!(frames.len(), 2),
            _ => panic!("a 2-frame GIF should decode as animated"),
        }
    }

    #[test]
    fn a_png_decodes_as_static() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(matches!(decode_image(&png), Some(Decoded::Static(_))));
    }

    #[test]
    fn garbage_bytes_decode_to_nothing() {
        assert!(decode_image(b"not an image at all").is_none());
    }
}

/// A stable, muted colour for a person's initials plate, derived from their id.
///
/// Deterministic on purpose: the same person keeps the same colour across
/// sessions, which is what makes the fallback readable as an identity.
fn avatar_color(id: &str) -> egui::Color32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    let palette = crate::theme::AVATAR;
    palette[(h.finish() as usize) % palette.len()]
}

/// Paints a plate with a person's initials.
///
/// The fallback shown while their avatar image has not arrived, or when there
/// is none: a coloured plate reads as a person, an empty grey square does not.
/// `rounding` is `height/2` for a circle, smaller for a rounded square.
fn paint_initials(painter: &egui::Painter, rect: egui::Rect, id: &str, name: &str, rounding: f32) {
    painter.rect_filled(rect, rounding, avatar_color(id));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initials_of(name),
        egui::FontId::proportional(rect.height() * 0.42),
        pal().ink,
    );
}

/// Draws a small triangle, taking up the space of one character.
///
/// None of the Unicode arrows — neither `\u{25b2}`, nor `\u{25be}`, nor
/// `\u{2193}` — exists in the fonts egui loads, not even with the whole Noto
/// Emoji: they were all drawn as a box. Three vertices are not worth depending
/// on a font for.
fn triangle(ui: &mut egui::Ui, dir: Dir, color: egui::Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::click());
    let c = rect.center();
    let r = 4.0;
    let p = match dir {
        Dir::Up => vec![
            egui::pos2(c.x, c.y - r),
            egui::pos2(c.x + r, c.y + r * 0.7),
            egui::pos2(c.x - r, c.y + r * 0.7),
        ],
        Dir::Down => vec![
            egui::pos2(c.x, c.y + r),
            egui::pos2(c.x - r, c.y - r * 0.7),
            egui::pos2(c.x + r, c.y - r * 0.7),
        ],
        Dir::Right => vec![
            egui::pos2(c.x + r, c.y),
            egui::pos2(c.x - r * 0.7, c.y - r),
            egui::pos2(c.x - r * 0.7, c.y + r),
        ],
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(p, color, egui::Stroke::NONE));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
    Right,
}

/// Whether the loaded font can draw this text.
fn drawable(ui: &egui::Ui, s: &str) -> bool {
    let font = egui::TextStyle::Body.resolve(ui.style());
    ui.fonts(|f| f.has_glyphs(&font, s))
}

/// Splits the unreads between those above and those below the visible
/// part.
///
/// It also returns where the nearest one in each direction is, in content
/// coordinates: that is what a click jumps to. A row only partly visible counts
/// as visible — flagging it would be an invitation to scroll towards something
/// you are already looking at.
fn split_unread(
    marks: &[UnreadMark],
    view_top: f32,
    view_bottom: f32,
    offset: f32,
) -> (UnreadCount, UnreadCount) {
    let (mut above, mut below) = (UnreadCount::default(), UnreadCount::default());
    for m in marks {
        // A coordinate inside the content, not on screen: needed to be able
        // to bring the list there afterwards.
        let content_y = offset + (m.top - view_top);
        if m.bottom < view_top {
            above.channels += 1;
            above.mentions += m.mention as usize;
            // The nearest one above is the lowest among those above.
            if above.channels == 1 || content_y > above.nearest {
                above.nearest = content_y;
            }
        } else if m.top > view_bottom {
            below.channels += 1;
            below.mentions += m.mention as usize;
            if below.channels == 1 || content_y < below.nearest {
                below.nearest = content_y;
            }
        }
    }
    (above, below)
}

/// An unread row and where it is in the list.
#[derive(Clone, Copy)]
struct UnreadMark {
    top: f32,
    bottom: f32,
    mention: bool,
}

/// How many unreads there are in one direction, and where the nearest is.
#[derive(Clone, Copy, Default, PartialEq)]
struct UnreadCount {
    channels: usize,
    mentions: usize,
    /// The vertical coordinate of the nearest one, for the jump on click.
    nearest: f32,
}

impl UnreadCount {
    fn any(&self) -> bool {
        self.channels > 0
    }

    /// "3 unread · 1 mention"
    fn label(&self) -> String {
        let c = if self.channels == 1 {
            t!("gui.unread_one").to_string()
        } else {
            t!("gui.unread_many", n = self.channels).to_string()
        };
        match self.mentions {
            0 => c,
            1 => t!("gui.unread_mention_one", base = c).to_string(),
            n => t!("gui.unread_mention_many", base = c, n = n).to_string(),
        }
    }
}

/// How a person is doing, for the mark in front of their name.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Presence {
    /// Connected right now.
    Active,
    /// Not connected.
    Away,
    /// In quiet hours, or on a manual pause.
    Quiet,
    /// Not known yet: the data arrives shortly after startup.
    Unknown,
}

/// How to show a person's presence.
///
/// The order matters: someone in quiet hours is marked as such even if they
/// show as connected, because that is the information that changes the decision
/// whether to write to them. As long as nothing is known we do not assume they
/// are away.
fn presence_of(snap: &Snapshot, user: &str) -> Presence {
    let now = chrono::Utc::now().timestamp();
    if snap
        .dnd
        .get(user)
        .is_some_and(|d| crate::app::in_quiet_hours(now, d))
    {
        return Presence::Quiet;
    }
    match snap.presence.get(user) {
        Some(true) => Presence::Active,
        Some(false) => Presence::Away,
        None => Presence::Unknown,
    }
}

/// Draws the presence mark.
///
/// The dot is painted, not written: `\u{25cf}` exists in none of the loaded
/// fonts and was drawn as an empty box. For a status indicator, depending on a
/// font's coverage is fragile for no reason — it is two drawing primitives.
fn draw_presence(ui: &mut egui::Ui, p: Presence) {
    let size = egui::vec2(14.0, 14.0);

    match p {
        Presence::Quiet => {
            // The moon, on the other hand, is covered by the font, and it is the
            // symbol Slack itself uses: there is no reason to draw it by hand.
            ui.label(egui::RichText::new("\u{1f319}").size(11.0));
        }
        Presence::Unknown => {
            ui.label(egui::RichText::new("@").color(pal().text_muted));
        }
        Presence::Active | Presence::Away => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let c = rect.center();
            if p == Presence::Active {
                ui.painter().circle_filled(c, 4.5, pal().presence_active);
            } else {
                ui.painter()
                    .circle_stroke(c, 4.0, egui::Stroke::new(1.5_f32, pal().presence_away));
            }
        }
    }
}

/// Draws the mark for a huddle in progress: a dot that "broadcasts".
///
/// Painted like the presence, and for the same reason: no loaded font has a
/// headset. The ring around the dot tells it apart at a glance from the
/// presence dot, which is simply filled.
fn draw_huddle(ui: &mut egui::Ui, ringing: bool) -> egui::Response {
    let colore = if ringing {
        pal().warning
    } else {
        pal().success
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let c = rect.center();
    ui.painter().circle_filled(c, 2.5, colore);
    ui.painter()
        .circle_stroke(c, 6.0, egui::Stroke::new(1.5_f32, colore));
    resp
}

/// A channel as the sidebar needs it.
#[derive(Clone)]
struct ChannelRow {
    id: String,
    name: String,
    is_dm: bool,
    unread: bool,
    mentions: i64,
    /// The other side, for a two-party conversation: needed to find their
    /// presence and status.
    user: Option<String>,
}

/// The `ts` of the messages whose text contains `query`, case-insensitively,
/// in conversation order. Identified by `ts` so a page loading mid-jump cannot
/// shift the result onto a different message.
fn matching_timestamps(messages: &[MessageEntry], query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    messages
        .iter()
        .filter(|m| m.text.to_lowercase().contains(&q))
        .map(|m| m.ts.clone())
        .collect()
}

/// Resolves a conversation id to a human label: the peer for a DM/MPIM, the
/// `#name` for a channel. A bare id is only ever a last resort, prefixed so it
/// cannot be mistaken for a name.
fn conversation_label(
    channel: Option<&ChannelRow>,
    user_names: &HashMap<String, String>,
    channel_id: &str,
) -> String {
    let Some(c) = channel else {
        return t!("gui.conversation", id = channel_id).to_string();
    };
    if c.is_dm {
        if let Some(u) = c.user.as_deref() {
            if let Some(n) = user_names.get(u).filter(|n| !n.is_empty()) {
                return n.clone();
            }
        }
        if !c.name.is_empty() && c.name != c.id {
            return c.name.clone();
        }
        return t!("gui.conversation", id = channel_id).to_string();
    }
    if !c.name.is_empty() && c.name != c.id {
        return format!("#{}", c.name);
    }
    t!("gui.conversation", id = channel_id).to_string()
}

/// The kind of a conversation, read from the shape of its Slack id.
///
/// Slack ids carry the kind: `D…` is a direct message, `C…`/`G…` are public
/// and group (private) channels. The test is the id, never the emptiness of a
/// name: for a DM hit `search.messages` fills `channel.name` with the peer's
/// raw user id, so trusting a non-empty name prints `#U…`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelKind {
    /// A two-party conversation: the label is the person, never the id.
    Direct,
    /// A channel: the label is `#name`.
    Channel,
    /// Unknown kind: resolve it as best we can.
    Other,
}

fn channel_kind(id: &str) -> ChannelKind {
    match id.chars().next() {
        Some('D') => ChannelKind::Direct,
        Some('C') | Some('G') => ChannelKind::Channel,
        _ => ChannelKind::Other,
    }
}

/// The "where" label of a search hit, chosen by the channel id kind.
///
/// `who` is the already-resolved author, used as the DM fallback; `channel`
/// is the known sidebar row, when there is one, to resolve the peer and to
/// tell a group channel from a multi-person DM (both `G…`). It never returns
/// `#<id>` nor a bare id.
fn search_hit_label(
    channel_id: &str,
    channel_name: &str,
    who: &str,
    channel: Option<&ChannelRow>,
    user_names: &HashMap<String, String>,
) -> String {
    match channel_kind(channel_id) {
        ChannelKind::Direct => {
            if let Some(uid) = channel.and_then(|c| c.user.as_deref()) {
                if let Some(n) = user_names.get(uid).filter(|n| !n.is_empty()) {
                    return n.clone();
                }
            }
            if !who.is_empty() && who != "?" {
                return who.to_string();
            }
            if let Some(c) = channel {
                if !c.name.is_empty() && c.name != channel_id {
                    return c.name.clone();
                }
            }
            t!("gui.conversation", id = channel_id).to_string()
        }
        ChannelKind::Channel => {
            // A `G…` id is a group channel or a multi-person DM. The row, when
            // known, disambiguates: an `is_dm` row is an MPIM and resolves as
            // a conversation, never as `#name`.
            if let Some(c) = channel {
                if c.is_dm {
                    return conversation_label(Some(c), user_names, channel_id);
                }
            }
            if !channel_name.is_empty() && channel_name != channel_id {
                format!("#{channel_name}")
            } else {
                conversation_label(channel, user_names, channel_id)
            }
        }
        ChannelKind::Other => conversation_label(channel, user_names, channel_id),
    }
}

/// A copy of the state used to draw one frame.
///
/// egui is synchronous and `AppState` sits behind an async `RwLock`. Reading
/// with `try_read` and keeping the last good copy avoids both blocking the
/// graphics thread and making the window flicker when the lock is held.
#[derive(Default)]
struct Snapshot {
    /// The mods consent dialog, when it should be on screen (cloned per frame).
    #[cfg(feature = "mods")]
    mods_consent: Option<crate::app::ModConsentDialog>,
    /// The mod settings form, when it should be on screen (cloned per frame).
    #[cfg(feature = "mods")]
    mods_settings: Option<crate::app::ModSettingsDialog>,
    /// Updated mods waiting for Keep/Forget of their saved secrets.
    #[cfg(feature = "mods")]
    mods_secrets: Vec<crate::mods::StaleSecrets>,
    channels: Vec<ChannelRow>,
    selected: usize,
    messages: Vec<MessageEntry>,
    user_names: Arc<HashMap<String, String>>,
    usernames: Arc<HashMap<String, String>>,
    avatars: Arc<HashMap<String, String>>,
    bot_users: Arc<std::collections::HashSet<String>>,
    emoji: Arc<HashMap<String, String>>,
    folders: Arc<Vec<crate::app::sections::Folder>>,
    collapsed: Arc<std::collections::HashSet<String>>,
    favorites: Arc<std::collections::HashSet<String>>,
    loading_older: bool,
    reached_start: bool,
    search_results: Arc<Vec<crate::api::client::SearchHit>>,
    searching: bool,
    /// Pinned messages of the open channel, newest first.
    pins: Vec<MessageEntry>,
    /// True while they are being fetched.
    pins_loading: bool,
    /// The "Later" list with its content resolved.
    saved: Vec<SavedRow>,
    saved_counts: crate::api::client::SavedCounts,
    /// True while the saved list is loading.
    saved_loading: bool,
    /// The "Threads" inbox.
    threads: Vec<ThreadRow>,
    threads_unread: i64,
    threads_loading: bool,
    /// Messages that mention the user.
    mentions: Arc<Vec<crate::api::client::SearchHit>>,
    mentions_loading: bool,
    /// Messages queued to be sent later.
    scheduled: Vec<ScheduledMessage>,
    thread_parent: Option<String>,
    thread_channel: Option<String>,
    thread_messages: Vec<MessageEntry>,
    channel_info: Option<crate::api::client::ChannelInfo>,
    channel_members: Vec<String>,
    /// The text typed in the member filter, kept across frames.
    channel_filter: String,
    status: String,
    live: bool,
    authenticated: bool,
    /// True when the session expired while in use: the chat stays, and the
    /// re-login dialog is shown.
    session_expired: bool,
    team_id: String,
    me: String,
    /// user id -> connected right now, for the people in the sidebar.
    presence: Arc<HashMap<String, bool>>,
    user_status: Arc<HashMap<String, crate::app::UserStatus>>,
    typing: Arc<HashMap<String, HashMap<String, std::time::Instant>>>,
    dnd: Arc<HashMap<String, crate::api::client::DndInfo>>,
    call: Option<crate::calls::live::CallStatus>,
    /// The last picture of a shared screen. The same `Arc` until a new frame
    /// arrives, which is how the window knows when to upload a texture.
    screen: Option<Arc<crate::calls::screen::Picture>>,
    /// The huddles in progress, by channel.
    huddles: HashMap<String, HuddleRow>,
    /// The notification centre, newest first.
    notifications: Vec<NotifEntry>,
    /// Transient messages (errors) still on screen.
    toasts: Vec<crate::app::Toast>,
    /// How many of them are still unread, for the bell badge.
    notif_unread: usize,
    /// The `ts` to bring into view, if a notification asked for a jump.
    jump_ts: Option<String>,
    /// The conversation that jump belongs to, when it is specific to one.
    jump_channel: Option<String>,
    /// The conversation the loaded messages belong to, by id.
    messages_channel: Option<String>,
    /// Mod decorations to render under messages, by channel and then `ts`.
    decorations: Arc<HashMap<String, HashMap<String, Vec<String>>>>,
    /// The mods' message actions, for the hover toolbar.
    #[cfg(feature = "mods")]
    mod_actions: Arc<Vec<crate::app::ModAction>>,
    /// The mods' context-menu items, for a message's context menu.
    #[cfg(feature = "mods")]
    mod_context_actions: Arc<Vec<crate::app::ModAction>>,
    /// The mods' views, for the views picker.
    #[cfg(feature = "mods")]
    mod_views: Arc<Vec<crate::app::ModView>>,
    /// The open mod-view panel, if any.
    #[cfg(feature = "mods")]
    mod_view_panel: Option<crate::app::ModViewPanel>,
    /// The open mod dialog, if any (rendered as a modal).
    #[cfg(feature = "mods")]
    mod_dialog: Option<crate::app::ModDialog>,
    /// The mods' composer actions, rendered by the composer.
    #[cfg(feature = "mods")]
    mod_composer_actions: Arc<Vec<crate::app::ModComposerAction>>,
    /// The mods' colour-role overrides (`ui.theme`), applied to the palette.
    #[cfg(feature = "mods")]
    mod_theme: Arc<Vec<(String, String)>>,
    /// The mod host, for the "Running mods" window: its status counters are
    /// lock-free, so reading them every frame is cheap.
    #[cfg(feature = "mods")]
    mods_host: Option<Arc<crate::mods::Host>>,
    /// Interface scale, applied as a zoom factor.
    ui_scale: f32,
    /// Persisted sidebar width (`0.0` = unset → the default).
    sidebar_width: f32,
    /// Whether desktop notifications are on (the settings switch).
    desktop_notifications: bool,
}

/// A huddle as the window needs it.
#[derive(Clone)]
struct HuddleRow {
    /// Who is in it, for the tiles in the huddle window.
    participants: Vec<String>,
    caller: Option<String>,
    ringing: bool,
}

impl HuddleRow {
    fn people(&self) -> usize {
        self.participants.len()
    }
}

impl Snapshot {
    fn refresh_from(&mut self, state: &Arc<RwLock<AppState>>) {
        let Ok(s) = state.try_read() else { return };
        #[cfg(feature = "mods")]
        {
            self.mods_consent = s.mods_consent.clone();
            self.mods_settings = s.mods_settings.clone();
            self.mods_secrets = s.mods_secrets.clone();
        }
        self.channels = s
            .channels
            .iter()
            .map(|c| ChannelRow {
                id: c.id.clone(),
                name: c.name.clone(),
                is_dm: c.is_dm,
                user: c.user.clone(),
                unread: c.unread,
                mentions: c.mentions,
            })
            .collect();
        self.selected = s.selected_channel;
        self.messages = s.messages.iter().map(clone_message).collect();
        self.user_names = s.user_names.clone();
        self.usernames = s.usernames.clone();
        self.avatars = s.avatars.clone();
        self.bot_users = s.bot_users.clone();
        self.emoji = s.emoji.clone();
        self.folders = s.folders.clone();
        self.collapsed = s.collapsed.clone();
        self.favorites = s.favorites.clone();
        self.search_results = s.search_results.clone();
        self.searching = s.searching;
        self.pins = s.pins.iter().map(clone_message).collect();
        self.pins_loading = s.pins_loading;
        self.saved = s
            .saved
            .iter()
            .map(|r| SavedRow {
                channel: r.channel.clone(),
                ts: r.ts.clone(),
                state: r.state.clone(),
                message: clone_message(&r.message),
            })
            .collect();
        self.saved_counts = s.saved_counts.clone();
        self.saved_loading = s.saved_loading;
        self.threads = s
            .threads
            .iter()
            .map(|t| ThreadRow {
                channel: t.channel.clone(),
                root: clone_message(&t.root),
                replies: t.replies.iter().map(clone_message).collect(),
                reply_count: t.reply_count,
            })
            .collect();
        self.threads_unread = s.threads_unread;
        self.threads_loading = s.threads_loading;
        self.mentions = s.mentions.clone();
        self.mentions_loading = s.mentions_loading;
        self.scheduled = s.scheduled.clone();
        self.loading_older = s.loading_older;
        self.reached_start = s.reached_start;
        self.thread_parent = s.thread_parent_ts.clone();
        self.thread_channel = s.thread_channel_id.clone();
        self.thread_messages = s.thread_messages.iter().map(clone_message).collect();
        self.channel_info = s.channel_info.clone();
        self.channel_members = s.channel_members.clone();
        self.status = s.status.clone();
        self.live = s.live;
        self.authenticated = s.authenticated;
        self.session_expired = s.session_expired;
        self.team_id = s.team_id.clone();
        self.me = s.my_user_id.clone();
        self.presence = s.presence.clone();
        self.user_status = s.user_status.clone();
        self.typing = s.typing.clone();
        self.dnd = s.dnd.clone();
        self.call = s.call.as_ref().and_then(|c| c.status());
        self.screen = s.call.as_ref().and_then(|c| c.screen());
        self.huddles = s
            .huddles
            .iter()
            .map(|(id, h)| {
                (
                    id.clone(),
                    HuddleRow {
                        participants: h.participants.clone(),
                        caller: h.caller.clone(),
                        ringing: h.ringing,
                    },
                )
            })
            .collect();
        self.notifications = s.notifications.clone();
        self.toasts = s.toasts.clone();
        self.notif_unread = s.notifications.iter().filter(|n| !n.read).count();
        self.jump_ts = s.jump_to_ts.clone();
        self.jump_channel = s.jump_channel.clone();
        self.messages_channel = s.messages_channel.clone();
        self.ui_scale = s.ui_scale;
        self.sidebar_width = s.sidebar_width;
        self.desktop_notifications = s.desktop_notifications;
        self.decorations = s.decorations.clone();
        #[cfg(feature = "mods")]
        {
            self.mod_actions = s.mod_actions.clone();
            self.mod_context_actions = s.mod_context_actions.clone();
            self.mod_views = s.mod_views.clone();
            self.mod_view_panel = s.mod_view_panel.clone();
            self.mod_dialog = s.mod_dialog.clone();
            self.mod_composer_actions = s.mod_composer_actions.clone();
            self.mod_theme = s.mod_theme.clone();
            self.mods_host = s.mods.clone();
        }
    }
}

fn clone_message(m: &MessageEntry) -> MessageEntry {
    MessageEntry {
        user: m.user.clone(),
        text: m.text.clone(),
        ts: m.ts.clone(),
        reactions: m
            .reactions
            .iter()
            .map(|r| Reaction {
                name: r.name.clone(),
                count: r.count,
                mine: r.mine,
                users: r.users.clone(),
            })
            .collect(),
        thread_ts: m.thread_ts.clone(),
        reply_count: m.reply_count,
        author_icon: m.author_icon.clone(),
        attachments: m.attachments.clone(),
        files: m.files.clone(),
        actions: m.actions.clone(),
        bot_id: m.bot_id.clone(),
        app_id: m.app_id.clone(),
    }
}

/// A monitor card in the share picker, with its preview once captured.
struct SharePicker {
    screens: Vec<MonitorInfo>,
    thumbs: HashMap<u32, egui::TextureHandle>,
    rx: std::sync::mpsc::Receiver<ShareThumb>,
}

/// The identifying data of a monitor, enough to draw its card.
struct MonitorInfo {
    id: u32,
    width: u32,
    height: u32,
    is_primary: bool,
}

/// A downscaled screenshot produced off the UI thread.
struct ShareThumb {
    id: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// The "new channel" dialog: the name typed so far and whether it is private.
#[derive(Default)]
struct NewChannel {
    name: String,
    private: bool,
}

/// The people picker. It starts a group DM, or — when `invite_to` is set —
/// invites the chosen people to that channel. `selected` holds user ids.
#[derive(Default)]
struct PeoplePick {
    invite_to: Option<String>,
    search: String,
    selected: std::collections::HashSet<String>,
}

/// The buffers of a channel's info panel while it is being edited: the name and
/// the topic. Reseeded whenever the panel switches to another channel.
struct Manage {
    id: String,
    name: String,
    topic: String,
}

/// The emoji/reaction picker as a first-class popup.
///
/// It is not a submenu of the hover toolbar: it lives in its own foreground
/// area anchored under the react button, and stays open until the pointer
/// presses outside it (or Esc) — picking an emoji only toggles the reaction.
/// `toolbar_id` is the toolbar it was opened from, so that toolbar does not
/// vanish while the popup is up.
struct EmojiPopup {
    toolbar_id: egui::Id,
    channel: String,
    ts: String,
    anchor: egui::Rect,
}

/// Which list the saved view is showing.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SavedTab {
    /// Slack's "Later" list (saved for later).
    #[default]
    Saved,
    /// The open conversation's pinned messages.
    Pinned,
}

/// The centres that take over the middle of the window. They share the same
/// place, so only one is ever open.
#[derive(Clone, Copy)]
enum CenterView {
    Threads,
    Mentions,
    Scheduled,
    Saved,
    Notifications,
}

/// A row of the "Threads" view, flattened for drawing.
struct ThreadRowView {
    channel: String,
    where_: String,
    root_ts: String,
    root_user: String,
    root_who: String,
    text: String,
    icon: String,
    replies: Vec<ReplyView>,
    count: i64,
}

/// A reply shown under a thread row.
struct ReplyView {
    user: String,
    who: String,
    text: String,
    icon: String,
}

/// The display name of a user id, falling back to the id itself.
fn display_name(names: &HashMap<String, String>, user: &str) -> String {
    names.get(user).cloned().unwrap_or_else(|| user.to_string())
}

/// Tomorrow at 09:00 local, as a Unix timestamp.
fn tomorrow_morning() -> i64 {
    use chrono::TimeZone;
    let now = chrono::Local::now();
    (now.date_naive() + chrono::Days::new(1))
        .and_hms_opt(9, 0, 0)
        .and_then(|d| d.and_local_timezone(chrono::Local).single())
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| now.timestamp() + 24 * 3600)
}

/// A Unix timestamp as local time: `HH:MM` today, `DD/MM HH:MM` otherwise.
fn format_datetime(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(ts, 0).single() {
        Some(dt) if dt.date_naive() == Local::now().date_naive() => dt.format("%H:%M").to_string(),
        Some(dt) => dt.format("%d/%m %H:%M").to_string(),
        None => String::new(),
    }
}

/// One edit to the open mod-settings form, queued by the GUI and applied in order.
#[cfg(feature = "mods")]
enum SettingsEdit {
    Text(String, String, String),
    Toggle(String, String),
    Cycle(String, String, bool),
    Clear(String, String),
    Tab(usize),
    Move(isize),
}

/// Applies one queued edit to the form.
#[cfg(feature = "mods")]
fn apply_settings_edit(d: &mut crate::app::ModSettingsDialog, edit: SettingsEdit) {
    // A save in flight owns the form: ignore edits (they would be lost when it
    // closes, since it snapshotted the panes).
    if d.busy {
        return;
    }
    fn field<'a>(
        d: &'a mut crate::app::ModSettingsDialog,
        id: &str,
        key: &str,
    ) -> Option<&'a mut crate::app::ModSettingField> {
        d.panes
            .iter_mut()
            .find(|p| p.id == id)
            .and_then(|p| p.fields.iter_mut().find(|f| f.key == key))
    }
    match edit {
        SettingsEdit::Text(id, key, text) => {
            if let Some(f) = field(d, &id, &key) {
                f.text = text;
                f.has_value = true;
                f.dirty = true;
                // New text cancels a pending clear (it is a replacement).
                f.clear = false;
            }
        }
        SettingsEdit::Toggle(id, key) => {
            if let Some(f) = field(d, &id, &key) {
                f.text = if f.text == "true" { "false" } else { "true" }.to_string();
                f.has_value = true;
                f.dirty = true;
            }
        }
        SettingsEdit::Cycle(id, key, forward) => {
            if let Some(f) = field(d, &id, &key) {
                if f.options.is_empty() {
                    return;
                }
                let n = f.options.len();
                let j = match f.options.iter().position(|o| *o == f.text) {
                    Some(i) => {
                        let step = if forward { 1 } else { -1 };
                        (i as isize + step).rem_euclid(n as isize) as usize
                    }
                    None => {
                        if forward {
                            0
                        } else {
                            n - 1
                        }
                    }
                };
                f.text = f.options[j].clone();
                f.has_value = true;
                f.dirty = true;
            }
        }
        SettingsEdit::Tab(i) => {
            if i < d.panes.len() {
                d.tab = i;
                d.cursor = 0;
                d.editing = false;
            }
        }
        SettingsEdit::Clear(id, key) => {
            if let Some(f) = field(d, &id, &key) {
                f.text.clear();
                f.has_value = false;
                f.clear = true;
                f.dirty = true;
            }
        }
        SettingsEdit::Move(delta) => {
            let n = d.panes.get(d.tab).map(|p| p.fields.len()).unwrap_or(0);
            if n > 0 {
                d.cursor = (d.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
            }
        }
    }
}

struct Gui {
    app: App,
    images: Images,
    /// URL -> texture already uploaded to the GPU. Textures cannot be
    /// created outside the graphics thread, so the conversion happens here, once
    /// per image.
    textures: HashMap<String, egui::TextureHandle>,
    /// Insertion order of `textures`, so eviction can take the oldest first.
    texture_order: std::collections::VecDeque<String>,
    /// URL -> the frames of an animated GIF (emoji, reaction or attachment).
    /// Separate from `textures` because one entry owns several textures and is
    /// advanced by the clock; bounded by `ANIM_TOTAL_BYTES_MAX`.
    anim: HashMap<String, AnimatedTexture>,
    /// Insertion order of `anim`, for oldest-first eviction.
    anim_order: std::collections::VecDeque<String>,
    /// Monotonic frame counter, bumped once per `draw`. Used by `evict_anim` to
    /// recognise (and spare) an animated GIF that was drawn on the previous
    /// frame, so eviction cannot thrash an on-screen one.
    frame_seq: u64,
    /// The sidebar width we last saw settled (pointer up) and treated as the
    /// committed value, used to decide when a drag is worth persisting. It is
    /// tracked locally rather than read back from the snapshot: the snapshot
    /// lags a frame behind a `set_sidebar_width`, and eframe persists egui's own
    /// panel memory independently, so the panel's live width can differ from the
    /// snapshot for a benign reason — comparing against *that* would let an
    /// ordinary click persist a width the user never dragged. `None` until the
    /// first frame draws the panel and seeds it.
    committed_sidebar_w: Option<f32>,
    /// Bundled monochrome icons, decoded and uploaded once. Keyed by name; they
    /// are local PNGs and never go through the URL download cache.
    icon_textures: HashMap<&'static str, egui::TextureHandle>,
    /// Allows `tokio::spawn` from the graphics thread, which is not in the runtime.
    rt: tokio::runtime::Handle,
    snap: Snapshot,
    /// The zoom factor currently applied to the egui context, so it is only
    /// changed when the requested scale changes.
    zoom_applied: f32,
    input: String,
    /// Per-conversation drafts, kept for the session: switching conversation
    /// stashes what was typed and brings back that conversation's own text,
    /// instead of carrying the draft along.
    drafts: HashMap<String, String>,
    /// The conversation `input` currently belongs to, to detect a switch.
    draft_channel: Option<String>,
    /// When we last told Slack we are active (see `drive_presence`): a keepalive,
    /// not a call per frame.
    presence_sent: Option<std::time::Instant>,
    /// The user's explicit Active/Away choice from the profile popover, if any: an
    /// explicit **Away** is respected (never re-asserted to active), and without
    /// one presence is asserted regardless of the window's focus.
    presence_manual: Option<bool>,
    thread_input: String,
    search: String,
    emoji_search: String,
    /// A message to forward: (source channel, ts, text for the preview).
    forwarding: Option<(String, String, String)>,
    forward_search: String,
    msg_search: String,
    /// The "new channel" dialog, when it is open.
    new_channel: Option<NewChannel>,
    /// The people picker: a new group DM, or an invite (`invite_to`).
    people_pick: Option<PeoplePick>,
    /// The name/topic buffers of the open channel's info panel.
    manage: Option<Manage>,
    /// A member pending removal, while the confirmation dialog is up:
    /// (channel id, user id, name).
    kick_pending: Option<(String, String, String)>,
    /// The "find in conversation" bar: whether it is open and the query. The
    /// matches themselves are recomputed each frame from the loaded messages,
    /// so they never go stale.
    find_open: bool,
    find: String,
    /// The current match, identified by its `ts`. A position would go stale:
    /// older pages loaded during a jump prepend messages and shift every index,
    /// which is what used to make the scroll land on the wrong row and the
    /// highlight disappear.
    find_cur: Option<String>,
    /// The `ts` to bring into view on the next frame, cleared once honoured.
    /// It is separate from `find_cur` so the list moves once, when the match
    /// changes, and then stays still.
    find_scroll: Option<String>,
    /// Set on the frame the bar opens (or Ctrl+F is pressed) to put the caret
    /// in the field.
    find_focus: bool,
    /// Set on the frame Ctrl+K is pressed to put the caret in the sidebar
    /// filter — the quick channel switcher.
    sidebar_focus: bool,
    /// The last status message seen and when it changed: the line beside the
    /// connection dot shows only briefly, then fades.
    status_seen: String,
    status_at: Option<std::time::Instant>,
    /// The highlighted entry in the `@` / `#` completion.
    complete_sel: usize,
    /// Completion entries computed at the start of the frame: (label, insert).
    complete_items: Vec<(String, String)>,
    /// The stretch to replace when choosing: (start, end) in bytes.
    complete_span: Option<(usize, usize)>,
    /// Mentions chosen from the popup: (inserted text, Slack form).
    ///
    /// In the composer you write `@Name Surname`, readable; the translation into
    /// `<@U123>` happens on send. The exact inserted text is kept, so the
    /// substitution is literal: if the user edits it, it no longer matches and
    /// stays plain text, instead of producing the wrong mention.
    pending_mentions: Vec<(String, String)>,
    /// The composer's position, to put the popup above it.
    composer_rect: egui::Rect,
    composer_id: Option<egui::Id>,
    /// Which composer the mention/channel completion is acting on. The state
    /// (`complete_items`, `complete_span`) is shared because only one composer
    /// has focus at a time.
    complete_in_thread: bool,
    /// The thread panel's area, to work out where a file was dropped.
    thread_rect: egui::Rect,
    /// The unread rows drawn on the last frame: vertical position and
    /// whether they contain a mention. It is filled while the list is drawn.
    unread_marks: Vec<UnreadMark>,
    /// How many unreads are outside the visible part, above and below.
    ///
    /// They are only known after drawing the list, but the strips go above and
    /// below it: the previous frame's values are used and a repaint is requested
    /// when they change.
    unread_above: UnreadCount,
    unread_below: UnreadCount,
    /// The position to bring the list to on the next frame.
    scroll_to: Option<f32>,
    /// The content height at the moment the previous stretch was asked for.
    ///
    /// It is what keeps us where we were: old messages are added **above**, and
    /// without compensating, the view stays at zero — which is still "at the top",
    /// from which another load starts at once, and you end up at the bottom of the
    /// history in moments.
    older_anchor: Option<f32>,
    /// Where to bring the message list on the next frame.
    msg_scroll_to: Option<f32>,
    /// The message being edited: (ts, text).
    editing: Option<(String, String)>,
    /// The hover toolbar shown on the last frame: its unique id (rendering
    /// location plus message) and its rect.
    ///
    /// The toolbar floats above the row, so while the pointer is on it the row
    /// no longer reports a hover. Remembering the rect keeps the toolbar from
    /// flickering as the pointer crosses from the row onto it. The id is not
    /// the bare timestamp: an open thread draws the same message a second time,
    /// and the two instances must not share state or egui ids.
    toolbar: Option<(egui::Id, egui::Rect)>,
    /// The toolbar whose mod-actions menu is open. The menu's dropdown is a
    /// separate `Area`, so without this the toolbar's keep-alive fails the moment
    /// the pointer moves onto the dropdown — the toolbar and the menu vanish and
    /// flicker. Tracking it keeps the toolbar alive while its menu is open.
    toolbar_menu_open: Option<egui::Id>,
    /// The emoji picker, when open. Kept alive until dismissed, and it is what
    /// keeps its toolbar from disappearing while the pointer is on the picker.
    emoji_popup: Option<EmojiPopup>,
    /// The image opened full screen: (url, name, permalink).
    viewing: Option<(String, String, String)>,
    /// True when reading back: it suspends the anchoring to the bottom.
    scrolled_back: bool,
    /// The jump target the counters below refer to; resetting it is what makes
    /// a repeated jump to the same `ts` start its budget over.
    jump_target: Option<String>,
    /// Older stretches requested while the pending jump was not yet found.
    jump_attempts: usize,
    /// Frames the jump has been pending since the last request; the safety
    /// valve for a load that never reports back.
    jump_frames: usize,
    /// The row a jump landed on, kept centered for a few frames while the
    /// layout settles (avatars and images change row heights after loading).
    center_ts: Option<String>,
    center_frames: usize,
    /// The single "current highlighted" message, by `ts`. The find bar and a
    /// search-result jump both set it, so the same wash marks both and the
    /// jump can highlight what it brought into view. It is dropped when the
    /// conversation changes, the find bar closes or the user scrolls away.
    highlight_ts: Option<String>,
    /// The conversation `highlight_ts` belongs to: a mismatch drops it.
    highlight_channel: Option<String>,
    /// The list offset when the highlight was honoured. A later change reads
    /// as the user scrolling away and clears the highlight.
    highlight_offset: Option<f32>,
    /// The height the huddle participant tiles needed last frame, used as the
    /// strip's minimum height so a row is never cut (egui panels settle in one
    /// frame, so the previous value is enough).
    huddle_row_min: f32,
    /// The `ts` to bring into view in the thread panel, for a search hit that
    /// is a thread reply. It is kept out of `jump_ts` so the main list, which
    /// ignores jumps while a thread is open, is never involved.
    thread_jump: Option<String>,
    /// The shared screen as a texture, with the picture it came from: the
    /// comparison is what avoids uploading the same frame twice.
    screen_tex: Option<(Arc<crate::calls::screen::Picture>, egui::TextureHandle)>,
    /// The monitor picker shown before a screen share starts.
    share_picker: Option<SharePicker>,
    /// Whether `Ctrl` was held on the previous frame.
    ///
    /// It is there to recognise `Ctrl+V` on release: if `Ctrl` is let go before
    /// `V`, by the time `V` is released the modifier already shows as up.
    ctrl_before: bool,
    waker_set: bool,
    /// True while the browser login is running, so the menu entry cannot be
    /// started twice. Shared with the spawned task, which clears it again when
    /// the login ends without a restart (the success path re-execs instead).
    logging_in: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the profile popover is open, and where its button sits so the
    /// popover can be anchored under it.
    profile_open: bool,
    profile_rect: egui::Rect,
    /// The top bar's rect in the last frame. Kept so a regression test can
    /// check the profile button stays inside it: a too-short bar clipped the
    /// avatar and the presence dot.
    top_rect: egui::Rect,
    /// The status being edited in the popover: text and shortcode without the
    /// colons. Seeded from the current status every time the popover opens.
    profile_status_text: String,
    profile_status_emoji: String,
    /// Whether the notification centre is open, and where the bell sits so the
    /// panel can be anchored under it.
    notif_open: bool,
    notif_rect: egui::Rect,
    /// Whether the settings window is open, and where its gear button sits so
    /// the window can be anchored under it.
    settings_open: bool,
    /// Whether the "Mod views…" picker is open.
    #[cfg(feature = "mods")]
    views_open: bool,
    /// Whether the "Running mods…" window is open.
    #[cfg(feature = "mods")]
    running_open: bool,
    /// The consent dialog's open channel/host editors (`<mod id>/channels`,
    /// `<mod id>/hosts`) and their text buffers, by the same key.
    #[cfg(feature = "mods")]
    access_ui: AccessUi,
    /// The theme last applied, so it is recomputed only when the mods' overrides
    /// change (a reload), not on every frame.
    #[cfg(feature = "mods")]
    applied_theme: Option<Arc<Vec<(String, String)>>>,
    settings_rect: egui::Rect,
    /// Mod settings edits made this frame but not yet flushed to the state. The
    /// form is a per-frame clone; the queue preserves order and never drops an
    /// edit on lock contention.
    #[cfg(feature = "mods")]
    settings_pending: Vec<SettingsEdit>,
    /// A Save requested while edits were still queued.
    #[cfg(feature = "mods")]
    settings_save_pending: bool,
    /// Whether the "saved" (Later) view is open.
    saved_open: bool,
    /// Which list that view shows.
    saved_tab: SavedTab,
    /// Whether the "Threads" view is open.
    threads_open: bool,
    /// Whether the "Mentions" view is open.
    mentions_open: bool,
    /// Whether the schedule-a-message popup is open.
    schedule_open: bool,
    /// Whether the "Scheduled" view is open.
    scheduled_open: bool,
}

/// The translated heading for a capability's consent group.
#[cfg(feature = "mods")]
fn group_label(cap: crate::mods::capability::Capability) -> String {
    match cap.group() {
        "read" => t!("gui.mods_group_read").to_string(),
        "storage" => t!("gui.mods_group_storage").to_string(),
        "interface" => t!("gui.mods_group_interface").to_string(),
        "network" => t!("gui.mods_group_network").to_string(),
        "filesystem" => t!("gui.mods_group_filesystem").to_string(),
        "commands" => t!("gui.mods_group_commands").to_string(),
        "write" => t!("gui.mods_group_write").to_string(),
        "background" => t!("gui.mods_group_background").to_string(),
        other => other.to_string(),
    }
}

/// A bounded rendering of an egress allow-list (first few hosts, then a count).
#[cfg(feature = "mods")]
fn hosts_text(hosts: &[String]) -> String {
    let mut head: Vec<String> = hosts
        .iter()
        .take(3)
        .map(|h| crate::mods::sanitize(h))
        .collect();
    let rest = hosts.len().saturating_sub(head.len());
    if rest > 0 {
        head.push(format!("+{rest}"));
    }
    head.join(", ")
}

/// Local state of the consent dialog's channel/host editors: which are open,
/// and their text buffers (filter, host being typed). Keyed `<mod id>/channels`
/// or `<mod id>/hosts`.
#[cfg(feature = "mods")]
#[derive(Default)]
struct AccessUi {
    open: std::collections::HashSet<String>,
    text: HashMap<String, String>,
}

/// A host on this machine (a local agent or model server), worth a note.
#[cfg(feature = "mods")]
fn is_local_host(h: &str) -> bool {
    let host = h.rsplit_once(':').map_or(h, |(h, _)| h);
    host == "localhost" || host.starts_with("127.")
}

/// A channel id as the sidebar names it (`#name`, or the person for a DM),
/// falling back to the id for one not in the sidebar.
#[cfg(feature = "mods")]
fn channel_display(id: &str, channels: &[ChannelRow]) -> String {
    match channels.iter().find(|c| c.id == id) {
        Some(c) if c.is_dm => crate::mods::sanitize(&c.name),
        Some(c) => format!("#{}", crate::mods::sanitize(&c.name)),
        None => crate::mods::sanitize(id),
    }
}

/// The one-line summary of a scope, by channel name.
#[cfg(feature = "mods")]
fn scope_summary(scope: &crate::mods::capability::ReadScope, channels: &[ChannelRow]) -> String {
    use crate::mods::capability::ReadScope;
    match scope {
        ReadScope::All => t!("gui.mods_scope_all").to_string(),
        ReadScope::Channels { channels: ids } if ids.is_empty() => {
            t!("gui.mods_access_none").to_string()
        }
        ReadScope::Channels { channels: ids } => {
            let mut head: Vec<String> =
                ids.iter().take(3).map(|c| channel_display(c, channels)).collect();
            let rest = ids.len().saturating_sub(head.len());
            if rest > 0 {
                head.push(format!("+{rest}"));
            }
            head.join(", ")
        }
    }
}

/// The "Channels" row under `messages.read` in the consent dialog: a summary
/// with Change…, which opens the editor (as the mod asks / all / only these,
/// with a filterable channel list).
#[cfg(feature = "mods")]
fn access_channels_ui(
    ui: &mut egui::Ui,
    e: &crate::app::ModConsentEntry,
    channels: &[ChannelRow],
    state: &mut AccessUi,
    edits: &mut Vec<(String, crate::app::AccessEdit)>,
) {
    use crate::app::AccessEdit;
    use crate::mods::capability::ReadScope;
    let key = format!("{}/channels", e.id);
    let is_open = state.open.contains(&key);
    let summary = match &e.access.user_scope {
        Some(sc) => scope_summary(sc, channels),
        None => format!(
            "{} ({})",
            t!("gui.mods_access_as_manifest"),
            scope_summary(e.read_scope.as_ref().unwrap_or(&ReadScope::All), channels)
        ),
    };
    ui.horizontal(|ui| {
        ui.label(format!("    {}: {summary}", t!("gui.mods_access_channels")));
        let btn = if is_open {
            t!("gui.mods_access_done")
        } else {
            t!("gui.mods_access_edit")
        };
        if ui.small_button(btn.to_string()).clicked() {
            if is_open {
                state.open.remove(&key);
            } else {
                state.open.insert(key.clone());
            }
        }
    });
    if !is_open {
        return;
    }
    ui.indent(("access-channels", &e.id), |ui| {
        let mode = match &e.access.user_scope {
            None => 0,
            Some(ReadScope::All) => 1,
            Some(ReadScope::Channels { .. }) => 2,
        };
        for (m, label, edit) in [
            (0, t!("gui.mods_access_as_manifest"), AccessEdit::ScopeManifest),
            (1, t!("gui.mods_access_all"), AccessEdit::ScopeAll),
            (2, t!("gui.mods_access_some"), AccessEdit::ScopeSome),
        ] {
            if ui.radio(mode == m, label.to_string()).clicked() && mode != m {
                edits.push((e.id.clone(), edit));
            }
        }
        let Some(ReadScope::Channels { channels: picked }) = &e.access.user_scope else {
            return;
        };
        let filter = state.text.entry(key.clone()).or_default();
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text(t!("gui.mods_access_filter").to_string())
                .desired_width(240.0),
        );
        if picked.is_empty() {
            ui.colored_label(pal().warning, t!("gui.mods_access_pick_one").to_string());
        }
        let needle = filter.trim().to_lowercase();
        // Picked ids the sidebar does not list (left, archived, or typed on
        // the CLI) first, so they can still be unpicked; then the sidebar.
        let mut rows: Vec<(String, String)> = picked
            .iter()
            .filter(|id| !channels.iter().any(|c| &c.id == *id))
            .map(|id| (id.clone(), crate::mods::sanitize(id)))
            .collect();
        rows.extend(channels.iter().map(|c| (c.id.clone(), channel_display(&c.id, channels))));
        rows.retain(|(id, label)| {
            needle.is_empty()
                || label.to_lowercase().contains(&needle)
                || id.to_lowercase().contains(&needle)
        });
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt(("access-channel-list", &e.id))
            .max_height(160.0)
            .auto_shrink([false, true])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for (id, label) in &rows[range] {
                    let mut on = picked.contains(id);
                    if ui.checkbox(&mut on, label.as_str()).changed() {
                        edits.push((e.id.clone(), AccessEdit::ToggleChannel(id.clone())));
                    }
                }
            });
    });
}

/// The "Hosts" row under `net.egress` in the consent dialog: a summary with
/// Change…, which opens the editor (the manifest's hosts as toggles, the hosts
/// the user added with Remove, and a field to add one).
#[cfg(feature = "mods")]
fn access_hosts_ui(
    ui: &mut egui::Ui,
    e: &crate::app::ModConsentEntry,
    state: &mut AccessUi,
    edits: &mut Vec<(String, crate::app::AccessEdit)>,
) {
    use crate::app::AccessEdit;
    let key = format!("{}/hosts", e.id);
    let is_open = state.open.contains(&key);
    let all: Vec<String> = e
        .egress
        .iter()
        .filter(|h| e.access.egress_allowed.contains(h))
        .chain(e.access.egress_added.iter())
        .cloned()
        .collect();
    let summary = if all.is_empty() {
        t!("gui.mods_access_none").to_string()
    } else {
        hosts_text(&all)
    };
    ui.horizontal(|ui| {
        ui.label(format!("    {}: {summary}", t!("gui.mods_access_hosts")));
        let btn = if is_open {
            t!("gui.mods_access_done")
        } else {
            t!("gui.mods_access_edit")
        };
        if ui.small_button(btn.to_string()).clicked() {
            if is_open {
                state.open.remove(&key);
            } else {
                state.open.insert(key.clone());
            }
        }
    });
    if !is_open {
        return;
    }
    let local = |h: &str| {
        if is_local_host(h) {
            format!(" · {}", t!("gui.mods_access_local"))
        } else {
            String::new()
        }
    };
    ui.indent(("access-hosts", &e.id), |ui| {
        for h in &e.egress {
            let mut on = e.access.egress_allowed.contains(h);
            let label = format!(
                "{} ({}){}",
                crate::mods::sanitize(h),
                t!("gui.mods_access_from_manifest"),
                local(h)
            );
            if ui.checkbox(&mut on, label).changed() {
                edits.push((e.id.clone(), AccessEdit::ToggleHost(h.clone())));
            }
        }
        for h in &e.access.egress_added {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} ({}){}",
                    crate::mods::sanitize(h),
                    t!("gui.mods_access_added"),
                    local(h)
                ));
                if ui.small_button(t!("gui.mods_access_remove").to_string()).clicked() {
                    edits.push((e.id.clone(), AccessEdit::RemoveHost(h.clone())));
                }
            });
        }
        ui.horizontal(|ui| {
            let buf = state.text.entry(key.clone()).or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(buf)
                    .hint_text(t!("gui.mods_access_host_hint").to_string())
                    .desired_width(200.0),
            );
            let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui.button(t!("gui.mods_access_add").to_string()).clicked();
            if (entered || clicked) && !buf.trim().is_empty() {
                // Keep what was typed when it is refused, so it can be fixed.
                let typed = if crate::mods::capability::normalize_user_host(buf).is_ok() {
                    std::mem::take(buf)
                } else {
                    buf.clone()
                };
                edits.push((e.id.clone(), AccessEdit::AddHost(typed)));
            }
        });
        if let Some(err) = &e.access.error {
            ui.colored_label(pal().danger, err.clone());
        }
    });
}

impl eframe::App for Gui {
    /// The window background behind the panels. eframe's default is a
    /// translucent dark grey that shows as a black column wherever the panels
    /// leave a sliver uncovered (between the sidebar and the conversation).
    /// Matching the panel fill makes those slivers invisible.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

impl Gui {
    /// Draws one frame of the whole interface.
    ///
    /// Split out of [`eframe::App::update`] so the real UI can be driven by
    /// `egui_kittest` in tests — no eframe window required.
    fn draw(&mut self, ctx: &egui::Context) {
        let _guard = self.rt.enter();

        // A new frame: bump the counter first, so a GIF drawn this frame stamps
        // the current `frame_seq` and `evict_anim` (end of frame) can spare it.
        self.frame_seq = self.frame_seq.wrapping_add(1);

        // Trim the image cache before anything is looked up this frame, so a
        // texture fetched while drawing is never evicted until a later frame.
        // (Animated GIFs are trimmed at the *end* of the frame instead — see
        // `evict_anim` — so a GIF scrolled into view is not dropped before it
        // has been marked as used.)
        self.evict_textures();

        // The waker is registered on the first frame, when the egui context
        // exists: from then on, state that changes repaints at once instead of
        // waiting for the tick.
        if !self.waker_set {
            let ctx = ctx.clone();
            self.app
                .set_waker(std::sync::Arc::new(move || ctx.request_repaint()));
            self.waker_set = true;
        }

        // Flush queued mod-settings edits *before* refreshing the snapshot, so
        // this frame renders the applied queue (a text edit is an absolute string;
        // a stale snapshot would let consecutive frames overwrite each other).
        #[cfg(feature = "mods")]
        self.flush_settings();
        self.snap.refresh_from(self.app.state());

        // A theme mod may have changed the palette (on load/reload): apply it now
        // that the snapshot reflects it, before this frame is drawn.
        #[cfg(feature = "mods")]
        self.apply_mod_theme(ctx);

        // A mod dialog owns the keyboard: keep Escape (to dismiss) but drop other
        // key, text and clipboard events, so global shortcuts, typing and paste do
        // not reach the UI behind it. Pointer events are kept for its buttons (the
        // dialog's own full-screen rect absorbs clicks aimed at the UI behind it).
        #[cfg(feature = "mods")]
        if self.snap.mod_dialog.is_some() {
            ctx.input_mut(|i| {
                i.events.retain(|e| match e {
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        ..
                    } => true,
                    egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Ime(_)
                    | egui::Event::Cut
                    | egui::Event::Copy
                    | egui::Event::Paste(_) => false,
                    _ => true,
                });
            });
        }

        // Per-conversation drafts: on a switch, stash what was typed under the
        // conversation it belonged to and bring back that conversation's own
        // draft. Without this the text followed you from one channel to the
        // next. Kept in memory for the session.
        let now = self.current_channel();
        if now != self.draft_channel {
            if let Some(prev) = self.draft_channel.take() {
                let draft = std::mem::take(&mut self.input);
                if draft.trim().is_empty() {
                    self.drafts.remove(&prev);
                } else {
                    self.drafts.insert(prev, draft);
                }
            }
            self.input = now
                .as_ref()
                .and_then(|id| self.drafts.get(id).cloned())
                .unwrap_or_default();
            // The conversation changed: its pins load here, whether or not the
            // channel info panel is open.
            if let Some(id) = now.clone() {
                self.app.load_pins(id);
            }
            self.draft_channel = now;
        }

        // A composer action may have asked the composer to hold text (a "translate
        // and review" flow): take it for the open conversation.
        #[cfg(feature = "mods")]
        if let Some(id) = self.draft_channel.clone() {
            if let Some(text) = App::take_composer_prefill(self.app.state(), &id) {
                self.input = text;
            }
        }

        // Apply the interface scale when it changes. `set_zoom_factor` is
        // relative to the screen's own scale, so on a 4K display this is a
        // multiplier on top of it, not a replacement.
        if (self.snap.ui_scale - self.zoom_applied).abs() > f32::EPSILON {
            ctx.set_zoom_factor(self.snap.ui_scale.max(0.1));
            self.zoom_applied = self.snap.ui_scale;
        }

        // A jump's budget belongs to its target: a new `ts` (or the clearing)
        // starts the counters over, so the same target asked twice is honoured.
        if self.snap.jump_ts != self.jump_target {
            self.jump_target = self.snap.jump_ts.clone();
            self.jump_attempts = 0;
            self.jump_frames = 0;
        }
        if self.snap.jump_ts.is_some() {
            self.jump_frames += 1;
            // Last-resort safety: a target that cannot be found and a load that
            // never answers must not leave the list stuck reading backwards.
            if self.jump_frames > JUMP_MAX_FRAMES {
                self.app.clear_jump();
                self.snap.jump_ts = None;
                self.snap.status = t!("gui.message_not_found").to_string();
            }
        }

        let ctrl_now = ctx.input(|i| i.modifiers.command || i.modifiers.ctrl);

        // Consent is a local decision: offer it even before logging in.
        #[cfg(feature = "mods")]
        if self.snap.mods_consent.is_some() {
            self.mods_consent_window(ctx);
        }
        #[cfg(feature = "mods")]
        if self.snap.mods_settings.is_some() {
            self.mods_settings_window(ctx);
        }
        // After the consent prompt, never over it (one decision at a time).
        #[cfg(feature = "mods")]
        if self.snap.mods_consent.is_none() && !self.snap.mods_secrets.is_empty() {
            self.mods_secrets_window(ctx);
        }
        #[cfg(feature = "mods")]
        if self.views_open {
            // A reload that empties the views closes the picker.
            if self.snap.mod_views.is_empty() {
                self.views_open = false;
            } else {
                self.mod_views_window(ctx);
            }
        }
        #[cfg(feature = "mods")]
        if self.snap.mod_view_panel.is_some() {
            self.mod_view_window(ctx);
        }
        #[cfg(feature = "mods")]
        if self.running_open {
            self.mods_running_window(ctx);
        }
        #[cfg(feature = "mods")]
        if self.snap.mod_dialog.is_some() {
            self.mod_dialog_window(ctx);
        }

        if !self.snap.authenticated && self.snap.call.is_none() {
            self.login_screen(ctx);
            return;
        }

        // Only with a session: on the login screen `users.setPresence` has no
        // workspace and would fail on every keepalive.
        self.drive_presence();

        // Find in conversation: Ctrl+F opens the bar (and puts the caret in
        // the field), Esc closes it.
        if ctx.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::F)) {
            self.find_open = true;
            self.find_focus = true;
        }
        // Quick channel switcher: Ctrl+K puts the caret in the sidebar filter,
        // so a conversation is a few keystrokes away (type, then pick a row).
        if ctx.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::K)) {
            self.sidebar_focus = true;
        }
        if self.find_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.clear_find_highlight();
            self.find_open = false;
        }
        // A jump in flight is aborted with Esc: the pending target and the
        // highlight go away, and no further page is fetched.
        if self.snap.jump_ts.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.app.cancel_jump();
            self.snap.jump_ts = None;
            self.set_highlight(None);
        }
        if !self.find_open {
            self.find_cur = None;
            self.find_scroll = None;
        }
        if self.snap.channel_info.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.app.close_channel_info();
        }

        let top_bar = egui::TopBottomPanel::top("top")
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal().bg_panel))
            .show(ctx, |ui| {
                // egui's default vertical margin is only 2px, which left the
                // profile avatar (28px) and the bell badge flush against the panel
                // edges, so they looked clipped. A little air above and below keeps
                // them whole.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let (dot, color) = if self.snap.live {
                        ("●", pal().success)
                    } else {
                        ("○", pal().text_muted)
                    };
                    // The status line is transient: it appears when something
                    // changes and then fades, so a stale "N conversations" does not
                    // sit in the bar forever. The dot carries the durable state,
                    // and the full status is on its tooltip.
                    let status = self.snap.status.clone();
                    if status != self.status_seen {
                        self.status_seen = status.clone();
                        self.status_at = Some(std::time::Instant::now());
                    }
                    let hint = t!("gui.connection_hover").to_string();
                    let tip = if status.is_empty() {
                        hint
                    } else {
                        format!("{hint}\n{status}")
                    };
                    ui.colored_label(color, dot).on_hover_text(tip);
                    if !status.is_empty() {
                        if let Some(at) = self.status_at {
                            let age = at.elapsed();
                            if age < STATUS_TTL {
                                ui.label(&status);
                                ui.ctx()
                                    .request_repaint_after(STATUS_TTL.saturating_sub(age));
                            }
                        }
                    }
                    if self.snap.jump_ts.is_some()
                        && ui
                            .link(t!("gui.cancel").to_string())
                            .on_hover_text(t!("gui.cancel_jump_hover").to_string())
                            .clicked()
                    {
                        self.app.cancel_jump();
                        self.snap.jump_ts = None;
                        self.set_highlight(None);
                    }
                    let search_tex = self.icon_texture(ui.ctx(), "search");
                    let search_icon = egui::Image::new(&search_tex)
                        .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                        .tint(ui.visuals().text_color());
                    if ui
                        .add(egui::Button::image(search_icon))
                        .on_hover_text(t!("gui.find_in_conversation_hover").to_string())
                        .clicked()
                    {
                        self.find_open = true;
                        self.find_focus = true;
                    }
                    // The new channel / group DM entry point: a square button so it
                    // matches the other bar buttons instead of shrinking to the
                    // width of "+".
                    let new_btn = egui::Button::new("+").min_size(egui::vec2(26.0, 26.0));
                    egui::menu::menu_custom_button(ui, new_btn, |ui| {
                        if ui.button(t!("gui.new_channel").to_string()).clicked() {
                            self.new_channel = Some(NewChannel::default());
                            ui.close_menu();
                        }
                        if ui.button(t!("gui.new_group_dm").to_string()).clicked() {
                            self.people_pick = Some(PeoplePick::default());
                            ui.close_menu();
                        }
                    })
                    .response
                    .on_hover_text(t!("gui.new_hover").to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let r = self.profile_button(ui);
                        self.profile_rect = r.rect;
                        if r.clicked() {
                            self.profile_open = !self.profile_open;
                            if self.profile_open {
                                self.seed_profile_status();
                            }
                        }
                        let r = self.notification_bell(ui);
                        self.notif_rect = r.rect;
                        if r.clicked() {
                            self.toggle_center_view(CenterView::Notifications);
                        }
                        let r = self.settings_button(ui);
                        self.settings_rect = r.rect;
                        if r.clicked() {
                            self.settings_open = !self.settings_open;
                        }
                        let saved_active = self.saved_open;
                        let saved_btn = ui
                            .push_id("saved_btn", |ui| {
                                let tex = self.icon_texture(ui.ctx(), "bookmark");
                                let tint = if saved_active {
                                    ui.visuals().hyperlink_color
                                } else {
                                    ui.visuals().text_color()
                                };
                                ui.add(
                                    egui::Button::image(
                                        egui::Image::new(&tex)
                                            .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                            .tint(tint),
                                    )
                                    .frame(saved_active),
                                )
                            })
                            .inner;
                        if saved_btn
                            .on_hover_text(t!("gui.saved_title").to_string())
                            .clicked()
                        {
                            self.toggle_center_view(CenterView::Saved);
                        }
                        let in_call = self.snap.call.is_some();
                        // Where there is a huddle already you do not call: you join. It
                        // is the difference between opening a new room and reaching the one
                        // the others are already talking in.
                        let joinable = !in_call
                            && self
                                .current_channel()
                                .is_some_and(|id| self.snap.huddles.contains_key(&id));
                        let label = if in_call {
                            t!("gui.hang_up_f2").to_string()
                        } else if joinable {
                            t!("gui.join_f2").to_string()
                        } else {
                            t!("gui.call_f2").to_string()
                        };
                        if ui.button(label).clicked() {
                            self.toggle_call();
                        }
                        // Channel info sits with the other actions on the right.
                        let info_btn = ui
                            .add_enabled(
                                self.current_channel().is_some(),
                                egui::Button::new(t!("gui.info").to_string()),
                            )
                            .on_hover_text(t!("gui.channel_info_hover").to_string());
                        if info_btn.clicked() {
                            if let Some(id) = self.current_channel() {
                                self.app.open_channel_info(id);
                            }
                        }
                        // The layout is right to left: the space added here pushes the
                        // box towards the middle of the space left, instead of leaving it
                        // stuck to the buttons.
                        const SEARCH_W: f32 = 420.0;
                        // The clear "x" sits on the field's right; in a right-to-left
                        // layout it is added *before* the field. It appears only when
                        // there is something to clear, so the resting layout is
                        // unchanged.
                        const CLEAR_W: f32 = 24.0;
                        let clear =
                            !self.msg_search.is_empty() || !self.snap.search_results.is_empty();
                        ui.add_space(
                            ((ui.available_width() - SEARCH_W - if clear { CLEAR_W } else { 0.0 })
                                / 2.0)
                                .max(0.0),
                        );
                        if clear
                            && close_button(ui)
                                .on_hover_text(t!("gui.clear_search_hover").to_string())
                                .clicked()
                        {
                            self.msg_search.clear();
                            self.app.clear_search();
                        }
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.msg_search)
                                // Fixed id, like the composer: the clear "×"
                                // appears *before* the field once there is
                                // something to clear, which shifted the auto id
                                // and dropped the focus after the first letter.
                                .id(egui::Id::new("msg_search"))
                                .hint_text(t!("gui.search_messages_hint").to_string())
                                .desired_width(SEARCH_W),
                        );
                        let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if go && !self.msg_search.trim().is_empty() {
                            self.close_views();
                            self.app.search(self.msg_search.clone());
                        }
                    });
                });
                ui.add_space(6.0);
            });
        self.top_rect = top_bar.response.rect;

        if self.profile_open {
            self.profile_popover(ctx);
        }

        if self.notif_open {
            self.notification_popover(ctx);
        }

        if self.settings_open {
            self.settings_window(ctx);
        }

        if self.snap.session_expired {
            self.reauth_dialog(ctx);
        }

        self.schedule_popup(ctx);
        self.ring_bar(ctx);

        // Resizable within sensible bounds, and the width is remembered across
        // restarts (`AppState::sidebar_width`). It used to be pinned with
        // `exact_width` because the panel ballooned to its widest channel name;
        // names now truncate width-aware (see `channel_row`), so a dragged width
        // holds instead. `0.0` (unset, the `Default`) falls back to the default,
        // so the demo and the snapshot tests stay at 230.
        let sidebar_w = if self.snap.sidebar_width >= crate::app::SIDEBAR_WIDTH_MIN {
            self.snap.sidebar_width
        } else {
            crate::app::SIDEBAR_WIDTH_DEFAULT
        };
        let channels_panel = egui::SidePanel::left("channels")
            .resizable(true)
            .default_width(sidebar_w)
            .width_range(crate::app::SIDEBAR_WIDTH_MIN..=crate::app::SIDEBAR_WIDTH_MAX)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal().bg_panel))
            .show(ctx, |ui| {
                // The selected conversation is `bg_active` (a calm slate blue),
                // distinct from the accent tint that text selection uses.
                ui.visuals_mut().selection.bg_fill = pal().bg_active;
                ui.add_space(4.0);
                // The filter row: the field, plus a clear "x" on its right. The
                // "x" appears only with text in the field, which also keeps the
                // resting row exactly the height it always had.
                const CLEAR_W: f32 = 32.0;
                let w = ui.available_width();
                let search_box = if self.search.is_empty() {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .id(egui::Id::new("sidebar_search"))
                            .hint_text(t!("gui.search_hint").to_string())
                            .desired_width(w),
                    )
                } else {
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .id(egui::Id::new("sidebar_search"))
                                .hint_text(t!("gui.search_hint").to_string())
                                .desired_width(w - CLEAR_W),
                        );
                        if close_button(ui)
                            .on_hover_text(t!("gui.clear_search_hover").to_string())
                            .clicked()
                        {
                            self.search.clear();
                        }
                        r
                    })
                    .inner
                };
                if self.sidebar_focus {
                    search_box.request_focus();
                    self.sidebar_focus = false;
                }
                // Enter opens the first match — the quick switcher's "go". With
                // nothing to open the caret stays in the field.
                if search_box.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !self.search.trim().is_empty()
                {
                    if self.open_first_search_match() {
                        self.search.clear();
                    } else {
                        search_box.request_focus();
                    }
                }
                ui.separator();

                // With a search active the sidebar becomes a list of results: the
                // folders are there to find your way around, not to filter.
                if !self.search.trim().is_empty() {
                    let q = self.search.to_lowercase();
                    egui::ScrollArea::vertical().show(ui, |ui| self.search_results(ui, &q));
                    return;
                }

                self.unread_strip(ui, true);

                // The space for the bottom strip has to be taken **first**: the
                // list claims all the remaining height, and whatever is drawn after
                // it has no room left. The previous frame's count is used, which is
                // already what decides whether the strip will appear.
                let reserve = if self.unread_below.any() {
                    Self::STRIP_H
                } else {
                    0.0
                };
                let mut area = egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - reserve).max(0.0));
                if let Some(off) = self.scroll_to.take() {
                    area = area.vertical_scroll_offset(off);
                }
                self.unread_marks.clear();
                let out = area.show(ui, |ui| {
                    self.threads_entry(ui);
                    self.mentions_entry(ui);
                    self.scheduled_entry(ui);
                    self.notif_shortcuts(ui);
                    self.favorites_section(ui);
                    self.agents_section(ui);
                    // Until the folders have arrived the flat list is shown: better
                    // channels now than an empty sidebar.
                    if self.snap.folders.is_empty() {
                        let channels = self.snap.channels.clone();
                        for (i, c) in channels.iter().enumerate() {
                            let r = ui.scope(|ui| self.channel_row(ui, i, c)).response.rect;
                            self.note_unread(r, c.unread, c.mentions > 0);
                        }
                        return;
                    }
                    let folders = self.snap.folders.clone();
                    let channels = self.snap.channels.clone();
                    for f in folders.iter() {
                        let open = !self.snap.collapsed.contains(&f.id);
                        // A collapsed folder does not draw its channels, which
                        // therefore have no position: without this, an unread in there
                        // would be invisible to the strips, and the arrow would lead to
                        // a folder that looks as though it has nothing new.
                        let hidden: Vec<_> = if open {
                            Vec::new()
                        } else {
                            f.channels.iter().filter_map(|&i| channels.get(i)).collect()
                        };
                        let hidden_unread = hidden.iter().any(|c| c.unread);
                        let hidden_mentions: i64 = hidden.iter().map(|c| c.mentions).sum();

                        if !f.name.is_empty() {
                            let emoji = f.emoji.clone();
                            let r = ui
                                .horizontal(|ui| {
                                    let dir = if open { Dir::Down } else { Dir::Right };
                                    let a = triangle(ui, dir, pal().text_muted);
                                    // The folder emoji goes through the same path as
                                    // those in the messages, so it is in colour: before, it
                                    // was the last place left with the font's glyph.
                                    if !emoji.is_empty() {
                                        self.emoji(ui, &emoji);
                                        ui.add_space(4.0);
                                    }
                                    if a.clicked()
                                        || ui
                                            .add(
                                                egui::Label::new(
                                                    egui::RichText::new(&f.name)
                                                        .strong()
                                                        .color(pal().text_secondary),
                                                )
                                                .sense(egui::Sense::click())
                                                .selectable(false),
                                            )
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                    {
                                        self.app.toggle_folder(f.id.clone());
                                    }
                                    if hidden_mentions > 0 {
                                        pill(
                                            ui,
                                            &hidden_mentions.to_string(),
                                            pal().on_danger,
                                            pal().danger_fill,
                                        );
                                    } else if hidden_unread {
                                        ui.label(
                                            egui::RichText::new("\u{2022}")
                                                .strong()
                                                .color(pal().link),
                                        );
                                    }
                                })
                                .response
                                .rect;
                            self.note_unread(r, hidden_unread, hidden_mentions > 0);
                        }
                        if open {
                            for &i in &f.channels {
                                if let Some(c) = channels.get(i) {
                                    let r = ui
                                        .horizontal(|ui| {
                                            ui.add_space(10.0);
                                            self.channel_row(ui, i, c);
                                        })
                                        .response
                                        .rect;
                                    self.note_unread(r, c.unread, c.mentions > 0);
                                }
                            }
                        }
                        ui.add_space(4.0);
                    }
                });

                self.unread_strip(ui, false);
                self.recount_unread(ui.ctx(), &out);
            });
        // Persist the width only when the user finishes dragging the edge — not
        // every frame of the drag, and not on an ordinary click elsewhere.
        //
        // We compare the live width against a locally tracked `committed` value,
        // never against `sidebar_w` (the persisted snapshot): the snapshot lags
        // a frame behind a `set_sidebar_width`, and eframe persists egui's own
        // panel memory independently — so the panel's live width can differ from
        // `sidebar_w` for a benign reason. Comparing against the snapshot would
        // let the first pointer release anywhere (an ordinary channel click)
        // during that divergence persist a width the user never dragged.
        //
        // On the frame the pointer is released we take the settled width as the
        // new committed value; if it actually differs from the last committed
        // one, that was a resize, so persist it. A plain click leaves the width
        // unchanged, so the guard skips it. The committed value is seeded on the
        // first frame (no release yet) so the initial layout is never persisted.
        let dragged = channels_panel.response.rect.width();
        let released = ctx.input(|i| i.pointer.any_released());
        if sidebar_persist_decision(&mut self.committed_sidebar_w, dragged, released) {
            self.app.set_sidebar_width(dragged);
        }

        egui::TopBottomPanel::bottom("input").show(ctx, |ui| {
            // The typing line sits above the composer, and only in the
            // conversation (not while a centre view is open).
            let centre_open =
                self.threads_open || self.mentions_open || self.scheduled_open || self.saved_open;
            if !centre_open {
                if let Some(label) = self.typing_label(ctx) {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new(label).weak().small());
                }
            }
            if let Some((ts, text)) = self.editing.clone() {
                ui.add_space(4.0);
                let mut cancelled = false;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t!("gui.edit").to_string()).strong());
                    if ui.button(t!("gui.cancel").to_string()).clicked() {
                        cancelled = true;
                    }
                });
                if cancelled {
                    self.editing = None;
                    return;
                }
                let mut buf = text;
                let h = composer_height(&buf);
                let width =
                    (ui.available_width() - COMPOSER_BTN_W - ui.spacing().item_spacing.x - 8.0)
                        .max(80.0);
                let (resp, save_clicked) = ui
                    .horizontal(|ui| {
                        let resp = ui.add_sized(
                            [width, h],
                            egui::TextEdit::multiline(&mut buf)
                                // Fixed id: see the main composer.
                                .id(egui::Id::new("composer_edit"))
                                .desired_rows(1),
                        );
                        let save_tex = self.icon_texture(ui.ctx(), "check");
                        let save_icon = egui::Image::new(&save_tex)
                            .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                            .tint(ui.visuals().text_color());
                        let clicked = composer_button(ui, h, |ui| {
                            ui.add_sized(
                                [COMPOSER_BTN_W, COMPOSER_BTN_W],
                                egui::Button::image(save_icon),
                            )
                            .on_hover_text(t!("gui.save_hover").to_string())
                            .clicked()
                        });
                        (resp, clicked)
                    })
                    .inner;
                self.editing = Some((ts.clone(), buf.clone()));
                if save_clicked || sent(&resp, ui) {
                    if let Some(ch) = self.current_channel() {
                        self.app.edit_message(ch, ts, buf.trim_end().to_string());
                    }
                    self.editing = None;
                }
                ui.add_space(4.0);
                return;
            }
            ui.add_space(4.0);
            // Mod composer actions: a row of buttons above the composer, only when
            // a loaded mod declares one (so the default layout is unchanged).
            #[cfg(feature = "mods")]
            if !self.snap.mod_composer_actions.is_empty() {
                let actions = self.snap.mod_composer_actions.clone();
                let draft = self.input.clone();
                let channel = self.draft_channel.clone();
                let mut invoked: Option<(String, String)> = None;
                ui.horizontal_wrapped(|ui| {
                    for a in actions.iter() {
                        // The declared icon leads the label, like the message-action
                        // menu (an emoji survives `sanitize`).
                        let text = crate::mods::sanitize(&match &a.icon {
                            Some(ic) if !ic.is_empty() => format!("{ic} {}", a.label),
                            _ => a.label.clone(),
                        });
                        if ui
                            .button(text)
                            .on_hover_text(crate::mods::sanitize(&a.mod_name))
                            .clicked()
                        {
                            invoked = Some((a.mod_id.clone(), a.action_id.clone()));
                        }
                    }
                });
                if let (Some((mid, aid)), Some(ch)) = (invoked, channel) {
                    App::invoke_mod_composer_action(self.app.state().clone(), mid, aid, ch, draft);
                }
            }
            ui.horizontal(|ui| {
                // The box width, estimated *before* the buttons, so the wrapped
                // height — and so the height the buttons anchor to — is known when
                // they are placed.
                let est = (ui.available_width()
                    - 3.0 * COMPOSER_BTN_W
                    - 3.0 * ui.spacing().item_spacing.x
                    - 8.0)
                    .max(80.0);
                let (h, bounded) = composer_box(ui, &self.input, est);
                // A file can also be dragged onto the window, but without a
                // button that is not discoverable. It is the same Lucide set as
                // the other composer icons — not an emoji, which renders in
                // whatever colour the font has.
                let attach_tex = self.icon_texture(ui.ctx(), "paperclip");
                let attach_icon = egui::Image::new(&attach_tex)
                    .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                    .tint(ui.visuals().text_color());
                if composer_button(ui, h, |ui| {
                    ui.add_sized(
                        [COMPOSER_BTN_W, COMPOSER_BTN_W],
                        egui::Button::image(attach_icon),
                    )
                    .on_hover_text(t!("gui.attach_file_hover").to_string())
                    .clicked()
                }) {
                    self.pick_file(None);
                }
                let width = (ui.available_width()
                    - 2.0 * COMPOSER_BTN_W
                    - 2.0 * ui.spacing().item_spacing.x
                    - 8.0)
                    .max(80.0);
                // The completion keys have to be taken **before** the
                // `TextEdit` sees them: otherwise Enter sends the message,
                // Tab moves the focus and neither of them picks the entry.
                self.handle_paste(ui, None);
                let action = self.completion_keys(ui);

                // A `TextEdit` gets only a *floor* from `min_size`: left alone it
                // grows to fit all the text, making the row (and the panel) as tall
                // as the post — the buttons, which anchor to the row's bottom, then
                // went off the panel. Past the row cap (`composer_box`), bound it and
                // scroll inside; up to the cap the rendering is unchanged.
                let lock = !self.complete_items.is_empty();
                let hint = t!("gui.composer_hint").to_string();
                let mut text_edit = |ui: &mut egui::Ui| {
                    egui::TextEdit::multiline(&mut self.input)
                        // A **fixed** id, not the auto one: rows can appear and
                        // disappear *above* the composer (the "X is typing…"
                        // line, the edit banner, a mod's action row), and every
                        // widget after them shifts down a slot. The auto id
                        // moves with the slot, so egui took the typed-in field
                        // for a new one and dropped the focus mid-sentence. A
                        // global `Id::new` and not `id_salt`: past the row cap
                        // the field moves inside a `ScrollArea`, whose extra `Ui`
                        // layer would re-salt it.
                        .id(egui::Id::new("composer_input"))
                        .desired_rows(1)
                        .desired_width(width)
                        // With the popup open Tab has to pick the entry, not move
                        // the focus: egui intercepts it at the start of the frame,
                        // before it can be consumed, and `lock_focus` is the only way
                        // to stop it.
                        .lock_focus(lock)
                        .min_size(egui::vec2(width, h))
                        .hint_text(hint.as_str())
                        .show(ui)
                };
                let (out, viewport) = if bounded {
                    let area = egui::ScrollArea::vertical()
                        .id_salt("composer_scroll")
                        .max_height(h)
                        .show(ui, |ui| text_edit(ui));
                    (area.inner, area.inner_rect)
                } else {
                    let out = text_edit(ui);
                    let rect = out.response.rect;
                    (out, rect)
                };
                let resp = out.response;
                // Anchor the mention popup and the drop zone to the box (a scroll
                // area's **viewport**, not its possibly taller content).
                self.composer_rect = viewport;
                self.composer_id = Some(resp.id);

                let cursor = out
                    .cursor_range
                    .map(|c| c.primary.ccursor.index)
                    .unwrap_or_else(|| self.input.chars().count());
                self.completion_update(cursor, resp.has_focus());
                let consumed = self.completion_draw(ui, action);

                let clock_clicked = composer_button(ui, h, |ui| {
                    let tex = self.icon_texture(ui.ctx(), "clock");
                    ui.add_sized(
                        [COMPOSER_BTN_W, COMPOSER_BTN_W],
                        egui::Button::image(
                            egui::Image::new(&tex)
                                .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                                .tint(ui.visuals().text_color()),
                        ),
                    )
                    .on_hover_text(t!("gui.schedule_hover").to_string())
                    .clicked()
                });
                if clock_clicked && !self.input.trim().is_empty() {
                    self.schedule_open = true;
                }
                let send_tex = self.icon_texture(ui.ctx(), "send");
                let send_icon = egui::Image::new(&send_tex)
                    .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                    .tint(pal().text_on_accent);
                let send_clicked = composer_button(ui, h, |ui| {
                    ui.add_sized(
                        [COMPOSER_BTN_W, COMPOSER_BTN_W],
                        egui::Button::image(send_icon)
                            .fill(pal().accent)
                            .rounding(8.0),
                    )
                    .on_hover_text(t!("gui.send_hover").to_string())
                    .clicked()
                });
                if (send_clicked || (!consumed && sent(&resp, ui))) && !self.input.trim().is_empty()
                {
                    self.send();
                    resp.request_focus();
                }
            });
            ui.add_space(4.0);
        });

        if self.snap.thread_parent.is_some() {
            self.thread_panel(ctx);
        }

        if self.snap.channel_info.is_some() {
            self.channel_info_panel(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // The call in progress sits above everything: it is the thing you
            // want to be able to end without hunting for it.
            self.call_bar(ui);
            if self.threads_open {
                self.threads_view(ui);
                return;
            }
            if self.mentions_open {
                self.mentions_view(ui);
                return;
            }
            if self.scheduled_open {
                self.scheduled_view(ui);
                return;
            }
            if self.saved_open {
                self.saved_view(ui);
                return;
            }
            if self.snap.searching || !self.snap.search_results.is_empty() {
                self.search_view(ui);
                return;
            }
            if self.find_open {
                self.find_bar(ui);
            }
            if self.snap.messages.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak(if self.snap.channels.is_empty() {
                        t!("gui.loading_conversations").to_string()
                    } else {
                        t!("gui.no_messages").to_string()
                    });
                });
                return;
            }
            let mut area = egui::ScrollArea::vertical();
            if let Some(off) = self.msg_scroll_to.take() {
                area = area.vertical_scroll_offset(off);
            }
            // While a thread is open any pending jump belongs to its panel:
            // opening a thread must never move the conversation list. A jump
            // A channel-scoped jump ('vai alla conversazione') also moves the
            // timeline with a thread open, because it opens that thread too; a
            // jump without a channel (a notification) stays suppressed while a
            // thread is open so it does not drag the list around. The jump is
            // honoured only in its own conversation, or it would page the one
            // still on screen during a switch.
            // Gate on the channel the loaded messages belong to, not on the
            // sidebar selection: the sidebar order can shift under the jump.
            let jump_ok = match self.snap.jump_channel.as_deref() {
                Some(c) => self.snap.messages_channel.as_deref() == Some(c),
                None => self.snap.thread_parent.is_none(),
            };
            let jump = if jump_ok {
                self.snap.jump_ts.clone()
            } else {
                None
            };
            if jump.is_some() {
                // The jump owns the offset: a stale compensation left over from
                // a previous scroll-up must not shove the view somewhere else.
                self.older_anchor = None;
                self.msg_scroll_to = None;
            }
            // The row tinted as "current": the single highlighted `ts`, set by
            // the find bar or by a search-result jump.
            let highlight = self.highlight_ts.clone();
            let find_scroll = if self.find_open {
                self.find_scroll.clone()
            } else {
                None
            };
            // While a jump or a find scroll is pending the list must not stick
            // to the bottom, or the target would be dragged out of view.
            if jump.is_some() || find_scroll.is_some() {
                self.scrolled_back = true;
            }
            let mut jumped = false;
            let mut found_scroll = false;
            let mut jump_rect: Option<egui::Rect> = None;
            let center_ts = self.center_ts.clone();
            let mut center_rect: Option<egui::Rect> = None;
            let out = area
                // `stick_to_bottom` only until you have scrolled back:
                // otherwise every repaint would drag you to the bottom while you
                // are reading the history.
                .stick_to_bottom(!self.scrolled_back)
                // Without this, content wider than the panel scrolls the
                // whole list sideways: the messages come out cut off on the left
                // and the avatars disappear.
                .hscroll(false)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    if self.snap.loading_older {
                        ui.vertical_centered(|ui| {
                            ui.add_space(6.0);
                            ui.spinner();
                            ui.add_space(6.0);
                        });
                    } else if self.snap.reached_start && !self.snap.messages.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(6.0);
                            ui.weak(t!("gui.start_of_conversation").to_string());
                            ui.add_space(6.0);
                        });
                    }
                    let deco_channel = self.snap.messages_channel.clone().unwrap_or_default();
                    let messages = std::mem::take(&mut self.snap.messages);
                    // The panel viewport rect, captured before the inset frame
                    // shrinks `ui.max_rect()`.
                    let panel = ui.max_rect();
                    // A little inset so the conversation does not sit flush
                    // against the panel edges, which reads as cramped.
                    egui::Frame::none()
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            let mut prev: Option<&MessageEntry> = None;
                            for m in &messages {
                                let continuation = is_continuation(prev, m);
                                // A date header before the first message and at
                                // every local-day change, like the official client.
                                let new_day = match prev
                                    .and_then(|p| ts_secs(&p.ts))
                                    .zip(ts_secs(&m.ts))
                                {
                                    Some((a, b)) => !same_day_in(&chrono::Local, a, b),
                                    None => true,
                                };
                                if new_day {
                                    day_divider(ui, &m.ts);
                                }
                                let rect = ui
                                    .scope(|ui| {
                                        self.message(ui, m, &deco_channel, continuation, panel)
                                    })
                                    .response
                                    .rect;
                                if highlight.as_deref() == Some(m.ts.as_str()) {
                                    // Painted after the row: a translucent wash that
                                    // marks it without hiding the text.
                                    ui.painter().rect_filled(
                                        rect.expand(2.0),
                                        3.0,
                                        pal().accent_tint(0x5A),
                                    );
                                }
                                if find_scroll.as_deref() == Some(m.ts.as_str()) {
                                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                    found_scroll = true;
                                }
                                if jump.as_deref() == Some(m.ts.as_str()) {
                                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                    jump_rect = Some(rect);
                                    jumped = true;
                                }
                                if center_ts.as_deref() == Some(m.ts.as_str()) {
                                    center_rect = Some(rect);
                                }
                                prev = Some(m);
                            }
                        });
                    self.snap.messages = messages;
                });
            // Center the jumped-to row by hand: `scroll_to_rect` inside a
            // stick-to-bottom area is not always honoured, so the target could
            // stay off screen. This derives the offset from the row's rect.
            if let Some(r) = center_rect.or(jump_rect) {
                let inner_h = out.inner_rect.height();
                let max = (out.content_size.y - inner_h).max(0.0);
                let target =
                    (out.state.offset.y + r.center().y - out.inner_rect.center().y).clamp(0.0, max);
                self.msg_scroll_to = Some(target);
                ui.ctx().request_repaint();
            }
            if self.center_frames > 0 {
                self.center_frames -= 1;
                if self.center_frames == 0 {
                    self.center_ts = None;
                }
            }
            // The find request is one-shot: whether or not the row was found,
            // the list must not stay in the forced "reading back" state. It can
            // only be missing if a page reload dropped it, which does not
            // happen in normal use.
            if found_scroll || self.find_scroll == find_scroll {
                self.find_scroll = None;
            }
            if jumped {
                // Keep the jumped-to row marked. For 'vai' the target is the
                // thread root, which is what must be highlighted — not the
                // reply the search hit pointed at, which is not in this list.
                self.highlight_ts = jump.clone();
                self.highlight_channel = self.snap.messages_channel.clone();
                self.highlight_offset = None;
                self.center_ts = jump.clone();
                self.center_frames = 60;
                self.app.clear_jump();
            } else if jump.is_some() && self.snap.thread_parent.is_none() {
                // The target is not in the loaded window (otherwise the draw
                // above would have scrolled to it). Pull older stretches until
                // it shows up, the history ends or the budget runs out, so the
                // list always leaves the forced "reading back" state. With a
                // thread open the jump belongs to it, not to this list.
                if self.snap.reached_start {
                    self.app.clear_jump();
                    self.snap.status = t!("gui.message_not_found").to_string();
                } else if self.snap.loading_older {
                    // A request is already in flight: wait for its answer.
                } else {
                    // No page limit: keep pulling older stretches until the
                    // message shows or the history ends; Esc / "Annulla" stop
                    // it in the meantime.
                    self.jump_attempts += 1;
                    self.jump_frames = 0;
                    if let Some(id) = self.current_channel() {
                        self.app.load_older(id);
                    }
                }
            }

            // The highlight is a "you are here" marker: drop it once the
            // conversation changes or the user scrolls away. It is skipped
            // while a thread is open or a jump is still pending, when the
            // offset moves on its own.
            if self.highlight_ts.is_some()
                && self.snap.thread_parent.is_none()
                && jump.is_none()
                && !self.snap.loading_older
                && self.center_frames == 0
            {
                if self.highlight_channel.as_deref() != self.current_channel().as_deref() {
                    self.set_highlight(None);
                } else if let Some(prev) = self.highlight_offset {
                    if (out.state.offset.y - prev).abs() > 24.0 {
                        self.set_highlight(None);
                    }
                } else {
                    self.highlight_offset = Some(out.state.offset.y);
                }
            }

            // At the top the previous stretch is loaded. The threshold is
            // generous so it starts just before reaching the edge, and the jump is
            // less noticeable.
            let at_top = out.state.offset.y <= 40.0;
            let scrollable = out.content_size.y > out.inner_rect.height();
            self.scrolled_back = scrollable
                && out.state.offset.y + out.inner_rect.height() < out.content_size.y - 40.0;

            // The previous stretch has arrived: we move down by however much the
            // content grew, so the same message stays under your eyes.
            if let Some(before) = self.older_anchor {
                let grown = out.content_size.y - before;
                if grown > 1.0 {
                    self.msg_scroll_to = Some(out.state.offset.y + grown);
                    self.older_anchor = None;
                    // The correction applies on the next frame: without asking for
                    // it immediately we wait for the next event, and the shift shows up
                    // as a jolt.
                    ui.ctx().request_repaint();
                }
            }

            // One load at a time: `older_anchor` stays set until the content
            // has grown, and it is what prevents the avalanche. It must not
            // depend on the list being scrollable: a conversation whose window
            // holds one message is shorter than the viewport, so it can never be
            // scrolled and its older messages would never be requested.
            if at_top
                && self.older_anchor.is_none()
                && !self.snap.loading_older
                && !self.snap.reached_start
                // While a jump is pending its own bounded loader drives the
                // requests: the scroll-driven one would run them unbounded.
                && jump.is_none()
            {
                if let Some(id) = self.current_channel() {
                    self.older_anchor = Some(out.content_size.y);
                    self.app.load_older(id);
                }
            }
        });

        if self.emoji_popup.is_some() {
            self.emoji_popup(ctx);
        }

        if self.forwarding.is_some() {
            self.forward_dialog(ctx);
        }

        if self.new_channel.is_some() {
            self.new_channel_dialog(ctx);
        }
        if self.people_pick.is_some() {
            self.people_pick_dialog(ctx);
        }
        if self.kick_pending.is_some() {
            self.kick_confirm_dialog(ctx);
        }

        self.handle_dropped_files(ctx);

        if self.viewing.is_some() {
            self.image_viewer(ctx);
        }

        if self.snap.call.is_some() {
            self.huddle_window(ctx);
        } else {
            self.share_picker = None;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::F2)) {
            self.toggle_call();
        }

        // Everything is drawn: guarantee the pointing hand over anything that
        // would be clicked, however the widget was built.
        enforce_pointer_cursor(ctx);

        // Reactive repainting, not continuous: a window standing still must
        // not burn CPU. A second is enough for new messages to appear.
        // A safety net: state changes come from the waker, this only covers what
        // the waker does not.
        // While a call is running the speaker highlight has to follow the SFU's
        // audio metadata, which arrives several times a second and does not go
        // through the waker.
        let repaint = if self.snap.call.is_some() {
            std::time::Duration::from_millis(150)
        } else {
            std::time::Duration::from_secs(2)
        };
        ctx.request_repaint_after(repaint);
        self.toasts(ctx);
        self.ctrl_before = ctrl_now;
        // End of frame: now that every GIF shown this frame is marked
        // most-recently-used, trim the animated set to its byte budget.
        self.evict_anim();
    }

    /// Applies the mods' `ui.theme` overrides to the palette when they change
    /// (a reload), then re-installs the style so egui's `Visuals` follow.
    #[cfg(feature = "mods")]
    fn apply_mod_theme(&mut self, ctx: &egui::Context) {
        let theme = self.snap.mod_theme.clone();
        if self.applied_theme.as_ref() == Some(&theme) {
            return;
        }
        crate::theme::set_palette(palette_from(&theme));
        install_style(ctx);
        self.applied_theme = Some(theme);
    }
}

impl Gui {
    /// Builds the interface around an [`App`].
    ///
    /// `cookie` is the `d` session cookie for downloading workspace files, and
    /// `rt` the runtime handle used to spawn background work from the graphics
    /// thread. Shared by [`run`] and the UI snapshot tests, so both exercise
    /// exactly the same construction.
    fn new(app: App, cookie: Option<String>, rt: tokio::runtime::Handle) -> Self {
        Gui {
            app,
            images: Images::with_cookie(cookie),
            textures: HashMap::new(),
            texture_order: std::collections::VecDeque::new(),
            anim: HashMap::new(),
            anim_order: std::collections::VecDeque::new(),
            frame_seq: 0,
            committed_sidebar_w: None,
            icon_textures: HashMap::new(),
            rt,
            snap: Snapshot::default(),
            zoom_applied: 0.0,
            input: String::new(),
            drafts: HashMap::new(),
            draft_channel: None,
            presence_sent: None,
            presence_manual: None,
            thread_input: String::new(),
            search: String::new(),
            emoji_search: String::new(),
            forwarding: None,
            forward_search: String::new(),
            msg_search: String::new(),
            new_channel: None,
            people_pick: None,
            manage: None,
            kick_pending: None,
            find_open: false,
            find: String::new(),
            find_cur: None,
            find_scroll: None,
            find_focus: false,
            sidebar_focus: false,
            status_seen: String::new(),
            status_at: None,
            complete_sel: 0,
            complete_items: Vec::new(),
            complete_span: None,
            pending_mentions: Vec::new(),
            composer_rect: egui::Rect::NOTHING,
            composer_id: None,
            complete_in_thread: false,
            thread_rect: egui::Rect::NOTHING,
            unread_marks: Vec::new(),
            unread_above: UnreadCount::default(),
            unread_below: UnreadCount::default(),
            scroll_to: None,
            older_anchor: None,
            msg_scroll_to: None,
            editing: None,
            toolbar: None,
            toolbar_menu_open: None,
            emoji_popup: None,
            viewing: None,
            scrolled_back: false,
            jump_target: None,
            jump_attempts: 0,
            jump_frames: 0,
            center_ts: None,
            center_frames: 0,
            highlight_ts: None,
            highlight_channel: None,
            highlight_offset: None,
            huddle_row_min: 0.0,
            thread_jump: None,
            screen_tex: None,
            share_picker: None,
            ctrl_before: false,
            waker_set: false,
            logging_in: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_open: false,
            profile_rect: egui::Rect::NOTHING,
            top_rect: egui::Rect::NOTHING,
            profile_status_text: String::new(),
            profile_status_emoji: String::new(),
            notif_open: false,
            notif_rect: egui::Rect::NOTHING,
            settings_open: false,
            #[cfg(feature = "mods")]
            views_open: false,
            #[cfg(feature = "mods")]
            running_open: false,
            #[cfg(feature = "mods")]
            access_ui: AccessUi::default(),
            #[cfg(feature = "mods")]
            applied_theme: None,
            #[cfg(feature = "mods")]
            settings_pending: Vec::new(),
            #[cfg(feature = "mods")]
            settings_save_pending: false,
            settings_rect: egui::Rect::NOTHING,
            saved_open: false,
            saved_tab: SavedTab::default(),
            threads_open: false,
            mentions_open: false,
            schedule_open: false,
            scheduled_open: false,
        }
    }

    /// Draws a 32×32 avatar: the downloaded picture when it is ready, the
    /// initials otherwise. Requests the download either way.
    fn avatar(&mut self, ui: &mut egui::Ui, user: &str, who: &str, author_icon: &str) {
        let url = if author_icon.is_empty() {
            self.snap.avatars.get(user).cloned().unwrap_or_default()
        } else {
            author_icon.to_string()
        };
        match self.texture(ui.ctx(), &url) {
            Some(tex) => {
                ui.add(
                    egui::Image::new(&tex)
                        .fit_to_exact_size(egui::vec2(AVATAR_SIZE, AVATAR_SIZE))
                        .rounding(AVATAR_ROUNDING),
                );
            }
            None => {
                // A placeholder of the same size, so the text does not jump
                // when the picture finishes downloading.
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(AVATAR_SIZE, AVATAR_SIZE),
                    egui::Sense::hover(),
                );
                paint_initials(ui.painter(), rect, user, who, AVATAR_ROUNDING);
            }
        }
    }

    /// Draws a message: avatar, header, formatted body, reactions.
    fn message(
        &mut self,
        ui: &mut egui::Ui,
        m: &MessageEntry,
        channel: &str,
        continuation: bool,
        panel: egui::Rect,
    ) {
        let who = self
            .snap
            .user_names
            .get(&m.user)
            .cloned()
            .unwrap_or_else(|| m.user.clone());

        // In a horizontal layout egui gives its children unlimited width:
        // inside here `wrap()` has nothing to wrap against, and a long line
        // blows through the window. We measure the space before entering and
        // impose it on the text column.
        let row_width = ui.available_width();

        // Avatar, the spacing after it, and a right margin, so text never
        // runs into the right edge of the list. The same reserve applies to
        // the thread panel, which draws through this method too.
        let text_width = (row_width - AVATAR_SIZE - ui.spacing().item_spacing.x - 12.0).max(120.0);
        let row = ui.allocate_ui_with_layout(
            egui::vec2(row_width, 0.0),
            egui::Layout::left_to_right(egui::Align::TOP),
            |ui| {
                if continuation {
                    // The same width as the avatar, so a grouped message lines up
                    // with the one above it instead of jumping left.
                    ui.add_space(AVATAR_SIZE);
                } else {
                    self.avatar(ui, &m.user, &who, &m.author_icon);
                }
                // `set_max_width` does not reduce `available_width`: the children
                // go on believing they have infinite space and `horizontal_wrapped`
                // never wraps. An allocated region, on the other hand, really
                // constrains them.
                ui.allocate_ui(egui::vec2(text_width, 0.0), |ui| {
                    ui.vertical(|ui| {
                        // A continuation of the message above shares its header: no
                        // avatar, no repeated name and time.
                        if !continuation {
                            ui.horizontal(|ui| {
                                // The name opens the conversation with that person. Bots do
                                // not have one: there it stays text.
                                let is_person = self.snap.user_names.contains_key(&m.user);
                                if is_person {
                                    let r = ui
                                        .add(
                                            egui::Label::new(egui::RichText::new(&who).strong())
                                                .sense(egui::Sense::click())
                                                .selectable(false),
                                        )
                                        .on_hover_text(t!("gui.write_to", who = who).to_string())
                                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                                    if r.clicked() {
                                        self.app.open_dm(m.user.clone(), who.clone());
                                    }
                                } else {
                                    ui.strong(&who);
                                }
                                ui.weak(crate::app::format_ts(&m.ts))
                                    .on_hover_text(crate::app::format_ts_full(&m.ts));
                            });
                        }
                        if !m.text.is_empty() {
                            self.body(ui, &m.text, text_width);
                        }
                        // Decorations attached by mods, under the message.
                        if let Some(items) = self
                            .snap
                            .decorations
                            .get(channel)
                            .and_then(|by_ts| by_ts.get(&m.ts))
                        {
                            for t in items {
                                ui.add_space(2.0);
                                ui.weak(egui::RichText::new(t).italics());
                            }
                        }
                        for f in &m.files {
                            self.file(ui, f, text_width);
                        }
                        for a in &m.attachments {
                            self.attachment(ui, a, text_width);
                        }
                        self.reactions(ui, m);
                        self.action_buttons(ui, m);
                        let replies = m.reply_count.unwrap_or(0);
                        if replies > 0 {
                            let label = if replies == 1 {
                                t!("gui.reply_one").to_string()
                            } else {
                                t!("gui.replies", n = replies).to_string()
                            };
                            if ui
                                .link(label)
                                .on_hover_text(t!("gui.open_thread_hover").to_string())
                                .clicked()
                            {
                                self.open_thread(m);
                            }
                        }
                    });
                });
            },
        );
        let row_rect = row.response.rect;
        // The row response is the content's used rect: for a short message its
        // right edge is mid-panel. The toolbar must hug the panel's right edge,
        // so it is placed from the panel (scroll-area) rect, not the row's.
        // The hover band and the toolbar span the **panel** (the scroll-area
        // viewport), not `ui.max_rect()` — inside the conversation's inset
        // `Frame` that is the inset content, and the band stopped 12px short.
        let panel_rect = panel;
        // The hover band spans the whole panel, not just the used content: for a
        // short message the text ends mid-panel, and the pointer would leave it
        // before it could reach the toolbar at the right edge.
        let band = egui::Rect::from_min_max(
            egui::pos2(panel_rect.left(), row_rect.top()),
            egui::pos2(panel_rect.right(), row_rect.bottom()),
        );
        let hover_id = egui::Id::new("message_row_hover").with(ui.id()).with(&m.ts);
        let row_hover = ui.interact(band, hover_id, egui::Sense::hover());
        let row_hovered = row_hover.hovered();
        if continuation {
            // A grouped message carries no header of its own: its time is still
            // reachable on hover, so the information is not lost.
            row_hover.on_hover_text(crate::app::format_ts_full(&m.ts));
        }
        // A faint wash marks the row under the pointer, so the action toolbar
        // does not appear out of nowhere. egui paints in call order, so this is
        // drawn over the row: the alpha is low enough that the text is untouched.
        if row_hovered {
            ui.painter()
                .rect_filled(band, 0.0, pal().tint(pal().bg_hover, 0x40));
        }
        self.message_toolbar(ui, m, row_rect, panel_rect.right(), row_hovered, band);
        // A right-click context menu: the mods' `ui.contextMenu` items (they reuse
        // the `action` callback, with this message current).
        #[cfg(feature = "mods")]
        {
            let actions = self.snap.mod_context_actions.clone();
            if !actions.is_empty() {
                let mut invoked: Option<(String, String)> = None;
                row.response.context_menu(|ui| {
                    for a in actions.iter() {
                        let text = crate::mods::sanitize(&match &a.icon {
                            Some(ic) if !ic.is_empty() => format!("{ic} {}", a.label),
                            _ => a.label.clone(),
                        });
                        if ui
                            .button(text)
                            .on_hover_text(crate::mods::sanitize(&a.mod_name))
                            .clicked()
                        {
                            ui.close_menu();
                            invoked = Some((a.mod_id.clone(), a.action_id.clone()));
                        }
                    }
                });
                if let Some((mid, aid)) = invoked {
                    App::invoke_mod_action(
                        self.app.state().clone(),
                        mid,
                        aid,
                        channel.to_string(),
                        m.ts.clone(),
                    );
                }
            }
        }
        ui.add_space(8.0);
    }

    /// The reaction pills, clickable. An empty row draws nothing: the action
    /// that adds one lives in the hover toolbar.
    fn reactions(&mut self, ui: &mut egui::Ui, m: &MessageEntry) {
        let Some(channel) = self.current_channel() else {
            return;
        };
        if m.reactions.is_empty() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            for r in &m.reactions {
                // Your own are told apart by both the background and the
                // outline, as in Slack: the fill alone was easy to miss, and
                // without it you cannot tell whether a click adds or removes.
                let (fill, stroke) = if r.mine {
                    (
                        pal().accent_tint(0x50),
                        egui::Stroke::new(1.0_f32, pal().accent),
                    )
                } else {
                    (pal().reaction_bg, egui::Stroke::NONE)
                };
                let resp = egui::Frame::none()
                    .fill(fill)
                    .stroke(stroke)
                    .rounding(10.0)
                    .inner_margin(egui::Margin::symmetric(6.0, 1.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 3.0;
                            self.emoji(ui, &r.name);
                            ui.small(format!("{}", r.count));
                        });
                    })
                    .response
                    .interact(egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(self.who_reacted(r));
                if resp.clicked() {
                    self.app
                        .toggle_reaction(channel.clone(), m.ts.clone(), r.name.clone(), r.mine);
                }
            }
        });
    }

    /// The buttons an app put in an `actions` block, clickable. A link button
    /// opens its address; the others tell the app which button was pressed.
    fn action_buttons(&mut self, ui: &mut egui::Ui, m: &MessageEntry) {
        if m.actions.is_empty() {
            return;
        }
        let Some(channel) = self.current_channel() else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            for a in &m.actions {
                // `accent` is the primary action (never a status colour);
                // `danger_fill` is the red that carries white text. The label is
                // ink-on-fill, so it overrides the inherited text colour.
                let (fill, stroke, ink) = match a.style.as_str() {
                    "primary" => (pal().accent, egui::Stroke::NONE, Some(pal().text_on_accent)),
                    "danger" => (pal().danger_fill, egui::Stroke::NONE, Some(pal().on_danger)),
                    _ => (
                        ui.visuals().widgets.inactive.bg_fill,
                        egui::Stroke::new(1.0_f32, ui.visuals().widgets.inactive.bg_stroke.color),
                        None,
                    ),
                };
                let resp = egui::Frame::none()
                    .fill(fill)
                    .stroke(stroke)
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(10.0, 4.0))
                    .show(ui, |ui| {
                        if let Some(c) = ink {
                            ui.visuals_mut().override_text_color = Some(c);
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 3.0;
                            self.spans(ui, &mrkdwn::parse_spans(&a.text));
                        });
                    })
                    .response
                    .interact(egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(if a.url.is_some() {
                        t!("gui.open_link_hover").to_string()
                    } else {
                        t!("gui.send_action_hover").to_string()
                    });
                if resp.clicked() {
                    match a.url.as_deref().filter(|u| !u.is_empty()) {
                        Some(url) => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
                        None => self.app.trigger_block_action(
                            channel.clone(),
                            m.ts.clone(),
                            a.clone(),
                            m.bot_id.clone(),
                            m.app_id.clone(),
                        ),
                    }
                }
            }
        });
    }

    /// The emoji picker: the most used emoji, plus a search across all of them.
    ///
    /// It is the body of the standalone emoji popup. Picking one toggles the
    /// reaction and leaves the popup open.
    fn emoji_picker(&mut self, ui: &mut egui::Ui, channel: &str, ts: &str) {
        ui.set_max_height(280.0);
        // Pin the width. The popup is a content-sized `Area`, so without a fixed
        // width the `horizontal_wrapped` below wraps at *last frame's* width —
        // and that width then changes with the wrap, which changes the width
        // again: a one-emoji-per-row oscillation. It stays frozen while the UI is
        // idle (egui stops repainting), but hovering the popup's edge keeps
        // repainting and the wobble shows as a flicker. A fixed wrap width breaks
        // the loop (wide enough for the search field and a stable reaction grid).
        ui.set_width(260.0);
        ui.horizontal_wrapped(|ui| {
            for name in QUICK_REACTIONS {
                if self.emoji_button(ui, name) {
                    self.app.toggle_reaction(
                        channel.to_string(),
                        ts.to_string(),
                        name.to_string(),
                        false,
                    );
                }
            }
        });
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut self.emoji_search)
                .hint_text(t!("gui.emoji_search_hint").to_string())
                .desired_width(160.0),
        );
        let q = self.emoji_search.trim().to_lowercase();
        if q.len() >= 2 {
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        // The workspace's custom ones first, which are what the
                        // user looks for most often, then the standard ones.
                        let custom: Vec<(String, String)> = self
                            .snap
                            .emoji
                            .iter()
                            .filter(|(n, _)| n.contains(&q))
                            .map(|(n, u)| (n.clone(), u.clone()))
                            .take(20)
                            .collect();
                        for (name, url) in custom {
                            let clicked = match self.texture(ui.ctx(), &url) {
                                Some(tex) => ui
                                    .add(egui::ImageButton::new(
                                        egui::Image::new(&tex)
                                            .fit_to_exact_size(egui::vec2(20.0, 20.0)),
                                    ))
                                    .on_hover_text(format!(":{name}:"))
                                    .clicked(),
                                None => false,
                            };
                            if clicked {
                                self.app.toggle_reaction(
                                    channel.to_string(),
                                    ts.to_string(),
                                    name,
                                    false,
                                );
                            }
                        }
                        let found: Vec<String> = mrkdwn::all_emoji()
                            .filter(|(n, _)| n.contains(&q) && !n.contains("::"))
                            .map(|(n, _)| n.to_string())
                            .take(60)
                            .collect();
                        for name in found {
                            if self.emoji_button(ui, &name) {
                                self.app.toggle_reaction(
                                    channel.to_string(),
                                    ts.to_string(),
                                    name,
                                    false,
                                );
                            }
                        }
                    });
                });
        }
    }

    /// The floating toolbar shown while the pointer is over a message row.
    ///
    /// It carries the three actions that used to sit permanently in the header:
    /// react, reply and more. It is a foreground area, so the pointer on it is
    /// no longer on the row: the row's rect from the last frame and the open
    /// menu are what keep it from flickering.
    fn message_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        m: &MessageEntry,
        row_rect: egui::Rect,
        panel_right: f32,
        row_hovered: bool,
        band: egui::Rect,
    ) {
        let id = egui::Id::new("message_toolbar").with(ui.id()).with(&m.ts);
        let pointer = ui.ctx().pointer_hover_pos();
        let on_toolbar = self
            .toolbar
            .as_ref()
            .is_some_and(|(i, r)| *i == id && pointer.is_some_and(|p| r.contains(p)));
        let popup_open = self
            .emoji_popup
            .as_ref()
            .is_some_and(|e| e.toolbar_id == id);
        // The mod-actions menu's dropdown is its own `Area`; keep the toolbar
        // alive while that menu is open, or moving onto the dropdown closes both
        // (the flicker that made the mod action impossible to click).
        let menu_open = self.toolbar_menu_open == Some(id);
        // Once this toolbar is open, keep it open while the pointer stays anywhere
        // in the message's hover band — geometrically, against the plain `band`
        // rect. The toolbar is a foreground `Area` drawn over the band, so egui
        // reports the band as *not hovered* the moment the pointer is on the
        // toolbar (or its menu dropdown); `row_hovered` then drops and the toolbar
        // flickered in and out. Containment in a rect has no such layering lag.
        let open_already = self.toolbar.as_ref().is_some_and(|(i, _)| *i == id);
        let over_band = open_already && pointer.is_some_and(|p| band.contains(p));
        if !(row_hovered || on_toolbar || popup_open || menu_open || over_band) {
            if self.toolbar.as_ref().is_some_and(|(i, _)| *i == id) {
                self.toolbar = None;
            }
            if self.toolbar_menu_open == Some(id) {
                self.toolbar_menu_open = None;
            }
            return;
        }

        // A message scrolled partly above the list has a `row_rect.top()` that
        // is above the visible panel — its toolbar would land over the top bar
        // and steal the clicks from the profile and saved buttons. Clamp it to
        // the visible top of the list.
        let top = (row_rect.top() + 2.0).max(ui.clip_rect().top() + 2.0);
        let pos = egui::pos2(panel_right - 8.0, top);
        const ICON: f32 = 18.0;
        const BUTTON: f32 = 24.0;
        let area = egui::Area::new(id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::RIGHT_TOP)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let icon = egui::vec2(ICON, ICON);
                            let box_ = egui::vec2(BUTTON, BUTTON);
                            let tint = ui.visuals().text_color();
                            #[cfg(feature = "mods")]
                            if !self.snap.mod_actions.is_empty() {
                                let actions = self.snap.mod_actions.clone();
                                let mut chosen: Option<(String, String)> = None;
                                let menu = ui.push_id("mods", |ui| {
                                    ui.menu_button(t!("gui.mods_actions").to_string(), |ui| {
                                        for a in actions.iter() {
                                            let text = match &a.icon {
                                                Some(ic) if !ic.is_empty() => {
                                                    format!("{ic} {}", a.label)
                                                }
                                                _ => a.label.clone(),
                                            };
                                            if ui
                                                .button(text)
                                                .on_hover_text(crate::mods::sanitize(&a.mod_name))
                                                .clicked()
                                            {
                                                chosen =
                                                    Some((a.mod_id.clone(), a.action_id.clone()));
                                                ui.close_menu();
                                            }
                                        }
                                    })
                                });
                                // Keep this toolbar alive while its mod-actions
                                // menu is open (the dropdown is a separate Area).
                                if menu.inner.inner.is_some() {
                                    self.toolbar_menu_open = Some(id);
                                } else if self.toolbar_menu_open == Some(id) {
                                    self.toolbar_menu_open = None;
                                }
                                if let Some((mid, aid)) = chosen {
                                    if let Some(ch) = self.current_channel() {
                                        App::invoke_mod_action(
                                            self.app.state().clone(),
                                            mid,
                                            aid,
                                            ch,
                                            m.ts.clone(),
                                        );
                                    }
                                }
                            }
                            let react = ui
                                .push_id("react", |ui| {
                                    let tex = self.icon_texture(ui.ctx(), "smile-plus");
                                    let button = egui::Button::image(
                                        egui::Image::new(&tex).fit_to_exact_size(icon).tint(tint),
                                    )
                                    .min_size(box_);
                                    ui.add(button)
                                })
                                .inner;
                            let react_rect = react.rect;
                            if react
                                .on_hover_text(t!("gui.add_reaction_hover").to_string())
                                .clicked()
                            {
                                if let Some(ch) = self.current_channel() {
                                    self.emoji_popup = Some(EmojiPopup {
                                        toolbar_id: id,
                                        channel: ch,
                                        ts: m.ts.clone(),
                                        anchor: react_rect,
                                    });
                                }
                            }
                            let reply = ui
                                .push_id("reply", |ui| {
                                    let tex = self.icon_texture(ui.ctx(), "reply");
                                    let button = egui::Button::image(
                                        egui::Image::new(&tex).fit_to_exact_size(icon).tint(tint),
                                    )
                                    .min_size(box_);
                                    ui.add(button)
                                })
                                .inner;
                            if reply
                                .on_hover_text(t!("gui.reply_in_thread_hover").to_string())
                                .clicked()
                            {
                                self.open_thread(m);
                            }
                            // Save for later and pin are toggles: the icon is
                            // tinted when the message is already in that list.
                            let saved = self.snap.saved.iter().any(|r| r.ts == m.ts);
                            let save = ui
                                .push_id("save", |ui| {
                                    let tex = self.icon_texture(ui.ctx(), "bookmark");
                                    let col = if saved {
                                        ui.visuals().hyperlink_color
                                    } else {
                                        tint
                                    };
                                    ui.add(
                                        egui::Button::image(
                                            egui::Image::new(&tex)
                                                .fit_to_exact_size(icon)
                                                .tint(col),
                                        )
                                        .min_size(box_),
                                    )
                                })
                                .inner;
                            let save_hover = if saved {
                                t!("gui.unsave_hover").to_string()
                            } else {
                                t!("gui.save_for_later_hover").to_string()
                            };
                            if save.on_hover_text(save_hover).clicked() {
                                if let Some(ch) = self.current_channel() {
                                    if saved {
                                        self.app.unsave_message(ch, m.ts.clone());
                                    } else {
                                        self.app.save_message(ch, m.ts.clone());
                                    }
                                }
                                self.toolbar = None;
                            }
                            let pinned = self.snap.pins.iter().any(|p| p.ts == m.ts);
                            let pin = ui
                                .push_id("pin", |ui| {
                                    let tex = self.icon_texture(ui.ctx(), "pin");
                                    let col = if pinned {
                                        ui.visuals().hyperlink_color
                                    } else {
                                        tint
                                    };
                                    ui.add(
                                        egui::Button::image(
                                            egui::Image::new(&tex)
                                                .fit_to_exact_size(icon)
                                                .tint(col),
                                        )
                                        .min_size(box_),
                                    )
                                })
                                .inner;
                            let pin_hover = if pinned {
                                t!("gui.unpin_hover").to_string()
                            } else {
                                t!("gui.pin_hover").to_string()
                            };
                            if pin.on_hover_text(pin_hover).clicked() {
                                if let Some(ch) = self.current_channel() {
                                    if pinned {
                                        self.app.unpin_message(ch, m.ts.clone());
                                    } else {
                                        self.app.pin_message(ch, m.ts.clone());
                                    }
                                }
                                self.toolbar = None;
                            }
                            // The actions are direct buttons, not a dropdown: a
                            // submenu hanging off the hover toolbar fights it
                            // and flickers, and the user never asked for one.
                            let copy = ui
                                .push_id("copy", |ui| {
                                    ui.add(egui::Button::new("\u{1f4cb}").min_size(box_))
                                })
                                .inner;
                            if copy
                                .on_hover_text(t!("gui.copy_link_hover").to_string())
                                .clicked()
                            {
                                if let Some(ch) = self.current_channel() {
                                    let link =
                                        self.app.permalink(&ch, &m.ts, m.thread_ts.as_deref());
                                    ui.ctx().copy_text(link);
                                    self.snap.status = t!("gui.link_copied").to_string();
                                }
                                self.toolbar = None;
                            }
                            let forward = ui
                                .push_id("forward", |ui| {
                                    ui.add(egui::Button::new("\u{2197}").min_size(box_))
                                })
                                .inner;
                            if forward
                                .on_hover_text(t!("gui.forward_hover").to_string())
                                .clicked()
                            {
                                if let Some(ch) = self.current_channel() {
                                    self.forwarding = Some((ch, m.ts.clone(), m.text.clone()));
                                    self.forward_search.clear();
                                }
                                self.toolbar = None;
                            }
                            if m.user == self.snap.me {
                                let edit = ui
                                    .push_id("edit", |ui| {
                                        ui.add(egui::Button::new("\u{270f}").min_size(box_))
                                    })
                                    .inner;
                                if edit.on_hover_text(t!("gui.edit").to_string()).clicked() {
                                    self.editing = Some((
                                        m.ts.clone(),
                                        crate::app::mrkdwn::to_editable(&m.text),
                                    ));
                                }
                                let del = ui
                                    .push_id("del", |ui| {
                                        ui.add(egui::Button::new("\u{1f5d1}").min_size(box_))
                                    })
                                    .inner;
                                if del
                                    .on_hover_text(t!("gui.delete_hover").to_string())
                                    .clicked()
                                {
                                    if let Some(ch) = self.current_channel() {
                                        self.app.delete_message(ch, m.ts.clone());
                                    }
                                }
                            }
                        });
                    });
            });
        self.toolbar = Some((id, area.response.rect));
    }

    /// The "send later" popup: a few presets, anchored above the composer.
    fn schedule_popup(&mut self, ctx: &egui::Context) {
        if !self.schedule_open {
            return;
        }
        let pos = egui::pos2(
            self.composer_rect.right() - 8.0,
            self.composer_rect.top() - 8.0,
        );
        let mut chosen: Option<i64> = None;
        let area = egui::Area::new(egui::Id::new("schedule_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::RIGHT_BOTTOM)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(180.0);
                    ui.label(egui::RichText::new(t!("gui.schedule_title").to_string()).strong());
                    ui.add_space(4.0);
                    for (label, secs) in [
                        ("+15 min", 15 * 60),
                        ("+1 h", 3600),
                        ("+3 h", 3 * 3600),
                        ("+1 g", 24 * 3600),
                    ] {
                        if ui.button(label).clicked() {
                            chosen = Some(chrono::Utc::now().timestamp() + secs);
                        }
                    }
                    if ui.button(t!("gui.schedule_tomorrow").to_string()).clicked() {
                        chosen = Some(tomorrow_morning());
                    }
                });
            });
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx
                .pointer_interact_pos()
                .is_some_and(|p| !area.response.rect.expand(4.0).contains(p));
        if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.schedule_open = false;
        }
        if let Some(post_at) = chosen {
            if let Some(ch) = self.current_channel() {
                let name = self.conversation_name(&ch);
                let raw = std::mem::take(&mut self.input).trim_end().to_string();
                let text = self.resolve_mentions(&raw);
                if !text.trim().is_empty() {
                    self.app.schedule_message(ch, name, text, post_at);
                }
            }
            self.schedule_open = false;
        }
    }

    /// Draws the emoji popup, once per frame, in its own foreground area under
    /// the react button. It is what keeps the popup from being tied to the
    /// hover toolbar: the pointer can move into it without the toolbar
    /// vanishing, and an emoji click does not close it.
    fn emoji_popup(&mut self, ctx: &egui::Context) {
        let Some((channel, ts, anchor)) = self
            .emoji_popup
            .as_ref()
            .map(|e| (e.channel.clone(), e.ts.clone(), e.anchor))
        else {
            return;
        };
        let pos = anchor.left_bottom() + egui::vec2(0.0, 4.0);
        let area = egui::Area::new(egui::Id::new("emoji_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    self.emoji_picker(ui, &channel, &ts);
                });
            });
        let popup_rect = area.response.rect;
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx.pointer_interact_pos().is_some_and(|p| {
                !popup_rect.expand(4.0).contains(p) && !anchor.expand(4.0).contains(p)
            });
        if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.emoji_popup = None;
        }
    }

    /// The `ts` of the loaded messages whose text contains the query,
    /// case-insensitively, in conversation order.
    fn find_matches(&self) -> Vec<String> {
        matching_timestamps(&self.snap.messages, &self.find)
    }

    /// Position of the current match among `matches`, if it is still loaded.
    fn find_position(&self, matches: &[String]) -> Option<usize> {
        let cur = self.find_cur.as_deref()?;
        matches.iter().position(|ts| ts.as_str() == cur)
    }

    /// Moves the current match by `delta` (wrapping) and asks the list to
    /// scroll there. It works on `ts`, so a page loading mid-jump cannot make
    /// it pick a different message.
    fn find_move(&mut self, matches: &[String], delta: isize) {
        if matches.is_empty() {
            self.find_cur = None;
            return;
        }
        let n = matches.len() as isize;
        let base = self.find_position(matches).unwrap_or(0) as isize;
        let idx = (base + delta).rem_euclid(n) as usize;
        let ts = matches[idx].clone();
        self.find_cur = Some(ts.clone());
        self.find_scroll = Some(ts.clone());
        self.set_highlight(Some(ts));
    }

    /// Sets (or clears) the single "current highlighted" message.
    ///
    /// The find bar and a search-result jump go through here, so both scroll
    /// and tint refer to the same `ts` and the wash looks the same.
    fn set_highlight(&mut self, ts: Option<String>) {
        self.highlight_channel = self.current_channel();
        self.highlight_offset = None;
        self.highlight_ts = ts;
    }

    /// Drops a find-bar highlight, without touching one a search jump set.
    fn clear_find_highlight(&mut self) {
        if self.highlight_ts.is_some() && self.highlight_ts == self.find_cur {
            self.set_highlight(None);
        }
    }

    /// The thin find bar at the top of the conversation: query, counter and
    /// previous/next.
    fn find_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(t!("gui.find_label").to_string());
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.find)
                    // Fixed id: the call bar is drawn before the find bar in the
                    // same panel, and it appears/disappears — the auto id would
                    // shift with it.
                    .id(egui::Id::new("find_input"))
                    .hint_text(t!("gui.find_hint").to_string())
                    .desired_width(220.0),
            );
            if self.find_focus {
                r.request_focus();
                self.find_focus = false;
            }

            let changed = r.changed();
            let matches = self.find_matches();
            let n = matches.len();
            if changed {
                // A new query restarts from the first match.
                self.find_cur = matches.first().cloned();
                self.find_scroll = self.find_cur.clone();
                self.set_highlight(self.find_cur.clone());
            }

            // A single-line field loses focus on Enter: that is how the key is
            // caught, then the focus is handed straight back.
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let back = ui.input(|i| i.modifiers.shift);
                self.find_move(&matches, if back { -1 } else { 1 });
                r.request_focus();
            }

            if n == 0 {
                ui.weak(if self.find.trim().is_empty() {
                    String::new()
                } else {
                    t!("gui.no_results").to_string()
                });
            } else {
                // The current match may have shifted as older pages arrived:
                // recompute its position from the `ts`.
                let pos = self.find_position(&matches).unwrap_or(0);
                ui.label(format!("{}/{}", pos + 1, n));
                if ui
                    .button(t!("gui.find_up").to_string())
                    .on_hover_text(t!("gui.find_prev_hover").to_string())
                    .clicked()
                {
                    self.find_move(&matches, -1);
                }
                if ui
                    .button(t!("gui.find_down").to_string())
                    .on_hover_text(t!("gui.find_next_hover").to_string())
                    .clicked()
                {
                    self.find_move(&matches, 1);
                }
            }
            if self.snap.thread_parent.is_some() {
                ui.weak(t!("gui.find_main_only").to_string());
            }
            if ui
                .button(t!("gui.close_find").to_string())
                .on_hover_text(t!("gui.close_esc_hover").to_string())
                .clicked()
            {
                self.clear_find_highlight();
                self.find_open = false;
                self.find_cur = None;
                self.find_scroll = None;
            }
        });
        ui.add_space(2.0);
        ui.separator();
    }

    /// The "Scheduled" view: messages queued to be sent later, with a cancel.
    /// Closes every centre view — threads, mentions, scheduled, saved,
    /// notifications. They all draw in the same place, so only one is ever
    /// open, and opening a conversation always lands on the conversation.
    fn close_views(&mut self) {
        self.threads_open = false;
        self.mentions_open = false;
        self.scheduled_open = false;
        self.saved_open = false;
        self.notif_open = false;
    }

    /// Opens `view` (loading it), or closes it if it was the open one. Either
    /// way the others are closed, so switching between two of them swaps
    /// instead of stacking.
    fn toggle_center_view(&mut self, view: CenterView) {
        let was_open = match view {
            CenterView::Threads => self.threads_open,
            CenterView::Mentions => self.mentions_open,
            CenterView::Scheduled => self.scheduled_open,
            CenterView::Saved => self.saved_open,
            CenterView::Notifications => self.notif_open,
        };
        self.close_views();
        match view {
            CenterView::Threads => {
                self.threads_open = !was_open;
                if self.threads_open {
                    self.app.load_threads();
                }
            }
            CenterView::Mentions => {
                self.mentions_open = !was_open;
                if self.mentions_open {
                    self.app.load_mentions();
                }
            }
            CenterView::Scheduled => self.scheduled_open = !was_open,
            CenterView::Saved => {
                self.saved_open = !was_open;
                if self.saved_open {
                    self.app.load_saved();
                }
            }
            CenterView::Notifications => self.notif_open = !was_open,
        }
    }

    fn scheduled_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(t!("gui.scheduled").to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✕")
                    .on_hover_text(t!("gui.close_esc_hover").to_string())
                    .clicked()
                {
                    self.scheduled_open = false;
                }
            });
        });
        ui.separator();
        ui.weak(t!("gui.scheduled_note").to_string());
        ui.add_space(4.0);
        if self.snap.scheduled.is_empty() {
            ui.weak(t!("gui.scheduled_empty").to_string());
            return;
        }
        let rows: Vec<(String, String, String, String)> = self
            .snap
            .scheduled
            .iter()
            .map(|m| {
                (
                    m.channel_name.clone(),
                    m.text.clone(),
                    format_datetime(m.post_at),
                    m.id.clone(),
                )
            })
            .collect();
        let mut cancel: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (where_, text, when, id) in &rows {
                    ui.horizontal(|ui| {
                        ui.strong(when);
                        ui.weak(where_);
                        if ui.link(t!("gui.cancel").to_string()).clicked() {
                            cancel = Some(id.clone());
                        }
                    });
                    let w = ui.available_width();
                    self.body(ui, text, w);
                    ui.separator();
                }
            });
        if let Some(id) = cancel {
            self.app.cancel_scheduled(id);
        }
    }

    /// The "Mentions" view: messages that mention the user, newest first.
    fn mentions_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(t!("gui.mentions").to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✕")
                    .on_hover_text(t!("gui.close_esc_hover").to_string())
                    .clicked()
                {
                    self.mentions_open = false;
                }
            });
        });
        ui.separator();
        if self.snap.mentions_loading && self.snap.mentions.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("app.loading_mentions").to_string());
            });
            return;
        }
        if self.snap.mentions.is_empty() {
            ui.weak(t!("gui.mentions_empty").to_string());
            return;
        }
        let names = self.snap.user_names.clone();
        let channels = self.snap.channels.clone();
        // (channel, ts, thread_ts, who, where_, text, user)
        let rows: Vec<(
            String,
            String,
            Option<String>,
            String,
            String,
            String,
            Option<String>,
        )> = self
            .snap
            .mentions
            .iter()
            .map(|h| {
                let who = h
                    .username
                    .clone()
                    .or_else(|| h.user.as_ref().and_then(|u| names.get(u).cloned()))
                    .unwrap_or_else(|| "?".to_string());
                let channel = channels.iter().find(|c| c.id == h.channel.id);
                let where_ =
                    search_hit_label(&h.channel.id, &h.channel.name, &who, channel, &names);
                (
                    h.channel.id.clone(),
                    h.ts.clone(),
                    h.thread_ts.clone().filter(|t| !t.is_empty()),
                    who,
                    where_,
                    h.text.clone(),
                    h.user.clone(),
                )
            })
            .collect();
        let mut open: Option<(String, String, Option<String>)> = None;
        let mut dm: Option<(String, String)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                for (channel, ts, thread_ts, who, where_, text, user) in &rows {
                    ui.horizontal_top(|ui| {
                        match &user {
                            Some(u) => self.avatar(ui, u, who, ""),
                            None => {
                                let (r, _) = ui.allocate_exact_size(
                                    egui::vec2(32.0, 32.0),
                                    egui::Sense::hover(),
                                );
                                paint_initials(ui.painter(), r, "?", who, 8.0);
                            }
                        }
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                match &user {
                                    Some(u) => {
                                        let r = ui
                                            .add(
                                                egui::Label::new(egui::RichText::new(who).strong())
                                                    .sense(egui::Sense::click())
                                                    .selectable(false),
                                            )
                                            .on_hover_text(
                                                t!("gui.write_to", who = who).to_string(),
                                            )
                                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                                        if r.clicked() {
                                            dm = Some((u.clone(), who.clone()));
                                        }
                                    }
                                    None => {
                                        ui.strong(who);
                                    }
                                }
                                ui.weak(where_);
                                ui.weak(crate::app::format_ts(ts))
                                    .on_hover_text(crate::app::format_ts_full(ts));
                            });
                            let w = ui.available_width();
                            self.body(ui, text, w);
                            if ui.link(t!("gui.open_thread").to_string()).clicked() {
                                open = Some((channel.clone(), ts.clone(), thread_ts.clone()));
                            }
                        });
                    });
                    ui.separator();
                }
            });
        if let Some((u, who)) = dm {
            self.app.open_dm(u, who);
            self.mentions_open = false;
        }
        if let Some((channel, ts, thread_ts)) = open {
            self.app.open_notification(channel, ts, thread_ts);
            self.mentions_open = false;
        }
    }

    /// The "Threads" inbox: the threads the user is subscribed to, newest
    /// first, each with its latest replies.
    fn threads_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(t!("gui.threads").to_string());
            if self.snap.threads_unread > 0 {
                ui.weak(t!("gui.threads_unread", n = self.snap.threads_unread).to_string());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✕")
                    .on_hover_text(t!("gui.close_esc_hover").to_string())
                    .clicked()
                {
                    self.threads_open = false;
                }
            });
        });
        ui.separator();
        if self.snap.threads_loading && self.snap.threads.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("app.loading_threads").to_string());
            });
            return;
        }
        if self.snap.threads.is_empty() {
            ui.weak(t!("gui.threads_empty").to_string());
            return;
        }
        let names = self.snap.user_names.clone();
        // Everything the closure needs, cloned out of `self`: the body is drawn
        // with `&mut self`.
        let rows: Vec<ThreadRowView> = self
            .snap
            .threads
            .iter()
            .map(|t| ThreadRowView {
                channel: t.channel.clone(),
                where_: self.conversation_name(&t.channel),
                root_ts: t.root.ts.clone(),
                root_user: t.root.user.clone(),
                root_who: display_name(&names, &t.root.user),
                text: t.root.text.clone(),
                icon: t.root.author_icon.clone(),
                replies: t
                    .replies
                    .iter()
                    .map(|r| ReplyView {
                        user: r.user.clone(),
                        who: display_name(&names, &r.user),
                        text: r.text.clone(),
                        icon: r.author_icon.clone(),
                    })
                    .collect(),
                count: t.reply_count,
            })
            .collect();
        let mut open: Option<(String, String)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                for row in &rows {
                    ui.horizontal_top(|ui| {
                        self.avatar(ui, &row.root_user, &row.root_who, &row.icon);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&row.root_who);
                                ui.weak(&row.where_);
                                ui.weak(crate::app::format_ts(&row.root_ts))
                                    .on_hover_text(crate::app::format_ts_full(&row.root_ts));
                                if row.count == 1 {
                                    ui.weak(t!("gui.reply_one").to_string());
                                } else if row.count > 1 {
                                    ui.weak(t!("gui.replies", n = row.count).to_string());
                                }
                                if ui.link(t!("gui.open_thread").to_string()).clicked() {
                                    open = Some((row.channel.clone(), row.root_ts.clone()));
                                }
                            });
                            let w = ui.available_width();
                            self.body(ui, &row.text, w);
                            for rep in &row.replies {
                                ui.horizontal_top(|ui| {
                                    ui.add_space(16.0);
                                    self.avatar(ui, &rep.user, &rep.who, &rep.icon);
                                    ui.vertical(|ui| {
                                        ui.strong(&rep.who);
                                        let w = ui.available_width();
                                        self.body(ui, &rep.text, w);
                                    });
                                });
                            }
                        });
                    });
                    ui.separator();
                }
            });
        if let Some((channel, ts)) = open {
            self.app.open_thread(channel, ts);
            self.threads_open = false;
        }
    }

    /// The saved view: two tabs, Slack's "Later" list and the open
    /// conversation's pins, so the pins get the whole height instead of a
    /// cramped box in the channel info panel.
    fn saved_view(&mut self, ui: &mut egui::Ui) {
        let counts = self.snap.saved_counts.clone();
        ui.horizontal(|ui| {
            if selectable(
                ui,
                self.saved_tab == SavedTab::Saved,
                t!("gui.saved_title").to_string(),
            )
            .clicked()
            {
                self.saved_tab = SavedTab::Saved;
            }
            if selectable(
                ui,
                self.saved_tab == SavedTab::Pinned,
                t!("gui.pinned_title").to_string(),
            )
            .clicked()
            {
                self.saved_tab = SavedTab::Pinned;
            }
            if self.saved_tab == SavedTab::Saved {
                ui.weak(
                    t!(
                        "gui.saved_counts",
                        open = counts.uncompleted_count,
                        done = counts.completed_count,
                        total = counts.total_count
                    )
                    .to_string(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✕")
                    .on_hover_text(t!("gui.close_esc_hover").to_string())
                    .clicked()
                {
                    self.saved_open = false;
                }
            });
        });
        ui.separator();
        match self.saved_tab {
            SavedTab::Saved => self.saved_list(ui),
            SavedTab::Pinned => self.pinned_list(ui),
        }
    }

    /// The saved rows (Slack's "Later" list).
    fn saved_list(&mut self, ui: &mut egui::Ui) {
        // A spinner while the list is being fetched, instead of the misleading
        // "nothing saved" that showed for the few seconds the calls take.
        if self.snap.saved_loading && self.snap.saved.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("app.loading_saved").to_string());
            });
            return;
        }
        if self.snap.saved.is_empty() {
            ui.weak(t!("gui.saved_empty").to_string());
            return;
        }
        // Everything the closure needs is cloned out of `self` first: the body
        // is drawn with `&mut self`.
        let names = self.snap.user_names.clone();
        let rows: Vec<(String, String, String, String, String, String, String)> = self
            .snap
            .saved
            .iter()
            .map(|r| {
                let who = names
                    .get(&r.message.user)
                    .cloned()
                    .unwrap_or_else(|| r.message.user.clone());
                let where_ = self.conversation_name(&r.channel);
                (
                    r.channel.clone(),
                    r.ts.clone(),
                    who,
                    where_,
                    r.message.text.clone(),
                    r.message.user.clone(),
                    r.message.author_icon.clone(),
                )
            })
            .collect();
        let mut open: Option<(String, String)> = None;
        let mut unsave: Option<(String, String)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                for (channel, ts, who, where_, text, user, icon) in &rows {
                    ui.horizontal_top(|ui| {
                        self.avatar(ui, user, who, icon);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(who);
                                ui.weak(where_);
                                ui.weak(crate::app::format_ts(ts))
                                    .on_hover_text(crate::app::format_ts_full(ts));
                                if ui.link(t!("gui.go_to_conversation").to_string()).clicked() {
                                    open = Some((channel.clone(), ts.clone()));
                                }
                                if ui.link(t!("gui.unsave_hover").to_string()).clicked() {
                                    unsave = Some((channel.clone(), ts.clone()));
                                }
                            });
                            let w = ui.available_width();
                            self.body(ui, text, w);
                        });
                    });
                    ui.separator();
                }
            });
        if let Some((channel, ts)) = open {
            self.app.open_notification(channel, ts, None);
            self.saved_open = false;
        }
        if let Some((channel, ts)) = unsave {
            self.app.unsave_message(channel, ts);
        }
    }

    /// The open conversation's pinned messages.
    fn pinned_list(&mut self, ui: &mut egui::Ui) {
        if self.snap.pins_loading && self.snap.pins.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("app.loading_pinned").to_string());
            });
            return;
        }
        if self.snap.pins.is_empty() {
            ui.weak(t!("gui.pinned_empty").to_string());
            return;
        }
        let names = self.snap.user_names.clone();
        let pins: Vec<(String, String, String, String, String)> = self
            .snap
            .pins
            .iter()
            .map(|p| {
                let who = names
                    .get(&p.user)
                    .cloned()
                    .unwrap_or_else(|| p.user.clone());
                (
                    p.user.clone(),
                    who,
                    p.ts.clone(),
                    p.text.clone(),
                    p.author_icon.clone(),
                )
            })
            .collect();
        let mut open: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                for (user, who, ts, text, icon) in &pins {
                    ui.horizontal_top(|ui| {
                        self.avatar(ui, user, who, icon);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(who);
                                ui.weak(crate::app::format_ts(ts))
                                    .on_hover_text(crate::app::format_ts_full(ts));
                                if ui.link(t!("gui.go_to_conversation").to_string()).clicked() {
                                    open = Some(ts.clone());
                                }
                            });
                            let w = ui.available_width();
                            self.body(ui, text, w);
                        });
                    });
                    ui.separator();
                }
            });
        if let Some(ts) = open {
            if let Some(ch) = self.current_channel() {
                self.app.open_notification(ch, ts, None);
            }
            self.saved_open = false;
        }
    }

    fn search_view(&mut self, ui: &mut egui::Ui) {
        if self.snap.searching {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
            return;
        }
        let hits = self.snap.search_results.clone();
        let names = self.snap.user_names.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                for h in hits.iter() {
                    let who = h
                        .username
                        .clone()
                        .or_else(|| h.user.as_ref().and_then(|u| names.get(u).cloned()))
                        .unwrap_or_else(|| "?".to_string());
                    // Decide by the channel id kind, not by name emptiness: a DM
                    // hit carries the peer's raw user id in `name`, and the old
                    // check would print `#U…`.
                    let channel = self
                        .snap
                        .channels
                        .iter()
                        .find(|c| c.id == h.channel.id)
                        .cloned();
                    let where_ = search_hit_label(
                        &h.channel.id,
                        &h.channel.name,
                        &who,
                        channel.as_ref(),
                        &names,
                    );
                    ui.horizontal(|ui| {
                        // A hit from a person (not a bot) opens the DM with them;
                        // a channel hit stays plain text.
                        match h.user.clone() {
                            Some(uid) => {
                                let r = ui
                                    .add(
                                        egui::Label::new(egui::RichText::new(&who).strong())
                                            .sense(egui::Sense::click())
                                            .selectable(false),
                                    )
                                    .on_hover_text(t!("gui.write_to", who = who).to_string())
                                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                                if r.clicked() {
                                    self.app.open_dm(uid, who.clone());
                                }
                            }
                            None => {
                                ui.strong(&who);
                            }
                        }
                        ui.weak(where_);
                        ui.weak(crate::app::format_ts(&h.ts))
                            .on_hover_text(crate::app::format_ts_full(&h.ts));
                    });
                    let w = ui.available_width();
                    self.body(ui, &h.text, w);
                    ui.horizontal(|ui| {
                        // The hit's conversation may be a DM whose `name` is the
                        // peer's user id: resolve it so the header and the sidebar
                        // show a person, not an id.
                        let is_dm = h.channel.id.starts_with('D') || h.channel.id.starts_with('G');
                        let peer = if is_dm
                            && (h.channel.name.starts_with('U') || h.channel.name.starts_with('W'))
                        {
                            Some(h.channel.name.clone())
                        } else {
                            None
                        };
                        let hit_name = if is_dm {
                            peer.as_ref()
                                .and_then(|p| names.get(p).cloned())
                                .unwrap_or_else(|| who.clone())
                        } else {
                            h.channel.name.clone()
                        };
                        // "apri" opens **the thread of the hit** — the message
                        // plus all its replies — and does nothing else: it never
                        // touches or scrolls the conversation list.
                        if ui.link(t!("gui.open_hit").to_string()).clicked() {
                            let root = h
                                .thread_ts
                                .clone()
                                .filter(|t| !t.is_empty())
                                .unwrap_or_else(|| h.ts.clone());
                            self.app.open_thread(h.channel.id.clone(), root);
                            // The thread panel scrolls to and tints the exact
                            // message; out of `jump_ts`, so the main list stays put.
                            self.thread_jump = Some(h.ts.clone());
                            self.app.clear_search();
                        }
                        // "go to the conversation" selects the conversation and
                        // scrolls the list back — as far as it takes — until the
                        // message shows up, tinted. Abortable with Esc / "Annulla".
                        if !h.channel.id.is_empty()
                            && ui.link(t!("gui.go_to_conversation").to_string()).clicked()
                        {
                            self.app.jump_in_conversation(
                                h.channel.id.clone(),
                                hit_name.clone(),
                                is_dm,
                                peer.clone(),
                                h.ts.clone(),
                            );
                            self.highlight_ts = Some(h.ts.clone());
                            self.highlight_channel = Some(h.channel.id.clone());
                            self.highlight_offset = None;
                            // If the hit is a reply the thread opens for it; this
                            // scrolls the panel to and tints the reply there.
                            self.thread_jump = Some(h.ts.clone());
                            self.app.clear_search();
                        }
                    });
                    ui.separator();
                }
            });
    }

    /// Choosing the conversation to forward to.
    fn forward_dialog(&mut self, ctx: &egui::Context) {
        let Some((from, ts, preview)) = self.forwarding.clone() else {
            return;
        };
        let modal_resp = modal(ctx, "forward").show(ctx, |ui| {
            ui.set_min_width(340.0);
            ui.heading(t!("gui.forward_title").to_string());
            ui.weak(preview.chars().take(120).collect::<String>());
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.forward_search)
                    .hint_text(t!("gui.forward_hint").to_string())
                    .desired_width(320.0),
            );
            let q = self.forward_search.to_lowercase();
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    for c in self
                        .snap
                        .channels
                        .iter()
                        .filter(|c| q.is_empty() || c.name.to_lowercase().contains(&q))
                        .take(30)
                    {
                        let sigil = if c.is_dm { "@" } else { "#" };
                        if ui.button(format!("{sigil}  {}", c.name)).clicked() {
                            self.app.forward_message(
                                from.clone(),
                                ts.clone(),
                                c.id.clone(),
                                String::new(),
                            );
                            self.forwarding = None;
                        }
                    }
                });
            ui.add_space(4.0);
            // A visible way out: a modal has no title-bar X, and closing by
            // backdrop or Esc alone is not discoverable.
            if ui.button(t!("gui.cancel").to_string()).clicked() {
                self.forwarding = None;
            }
        });
        if modal_resp.should_close() {
            self.forwarding = None;
        }
    }

    /// The "new channel" dialog: a name and whether it is private.
    fn new_channel_dialog(&mut self, ctx: &egui::Context) {
        if self.new_channel.is_none() {
            return;
        }
        let app = self.app.clone();
        let mut create = false;
        let mut cancel = false;
        let modal_resp = {
            let state = self.new_channel.as_mut().expect("checked above");
            modal(ctx, "new_channel").show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.heading(t!("gui.new_channel").to_string());
                ui.add(
                    egui::TextEdit::singleline(&mut state.name)
                        .hint_text(t!("gui.channel_name_hint").to_string())
                        .desired_width(300.0),
                );
                ui.checkbox(&mut state.private, t!("gui.private_channel").to_string());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !state.name.trim().is_empty(),
                            egui::Button::new(t!("gui.create").to_string()),
                        )
                        .clicked()
                    {
                        create = true;
                    }
                    if ui.button(t!("gui.cancel").to_string()).clicked() {
                        cancel = true;
                    }
                });
            })
        };
        if modal_resp.should_close() || cancel {
            self.new_channel = None;
            return;
        }
        if create {
            let state = self.new_channel.as_ref().expect("checked above");
            let name = state.name.trim().to_string();
            let private = state.private;
            if !name.is_empty() {
                app.create_channel(name, private);
                self.new_channel = None;
            }
        }
    }

    /// The people picker: it starts a group DM, or — with `invite_to` set —
    /// invites the chosen people to that channel. People are user ids; the list
    /// is the whole workspace, filtered by the search box.
    fn people_pick_dialog(&mut self, ctx: &egui::Context) {
        if self.people_pick.is_none() {
            return;
        }
        let app = self.app.clone();
        let names = self.snap.user_names.clone();
        let me = self.snap.me.clone();
        let mut confirm = false;
        let mut cancel = false;
        let modal_resp = {
            let state = self.people_pick.as_mut().expect("checked above");
            let invite = state.invite_to.is_some();
            let title = if invite {
                t!("gui.add_people").to_string()
            } else {
                t!("gui.new_group_dm").to_string()
            };
            modal(ctx, "people_pick").show(ctx, |ui| {
                ui.set_min_width(340.0);
                ui.heading(title);
                ui.add(
                    egui::TextEdit::singleline(&mut state.search)
                        .hint_text(t!("gui.group_search_hint").to_string())
                        .desired_width(320.0),
                );
                let q = state.search.to_lowercase();
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| {
                        let mut people: Vec<(&String, &String)> = names
                            .iter()
                            .filter(|(id, _)| id.as_str() != me)
                            .filter(|(id, n)| {
                                q.is_empty()
                                    || n.to_lowercase().contains(&q)
                                    || id.to_lowercase().contains(&q)
                            })
                            .collect();
                        people.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
                        for (id, name) in people {
                            let on = state.selected.contains(id);
                            if selectable(ui, on, name).clicked() {
                                if on {
                                    state.selected.remove(id);
                                } else {
                                    state.selected.insert(id.clone());
                                }
                            }
                        }
                    });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let count = state.selected.len();
                    // A group needs someone else; an invite needs at least
                    // one name.
                    let enough = if invite { count >= 1 } else { count >= 2 };
                    let label = if invite {
                        t!("gui.invite").to_string()
                    } else {
                        t!("gui.create").to_string()
                    };
                    if ui.add_enabled(enough, egui::Button::new(label)).clicked() {
                        confirm = true;
                    }
                    if ui.button(t!("gui.cancel").to_string()).clicked() {
                        cancel = true;
                    }
                    ui.weak(t!("gui.selected_count", n = count).to_string());
                });
            })
        };
        if modal_resp.should_close() || cancel {
            self.people_pick = None;
            return;
        }
        if confirm {
            let state = self.people_pick.as_ref().expect("checked above");
            let users: Vec<String> = state.selected.iter().cloned().collect();
            match state.invite_to.clone() {
                Some(channel) if !users.is_empty() => {
                    app.invite_to_channel(channel, users);
                    self.people_pick = None;
                }
                None if !users.is_empty() => {
                    app.create_group_dm(users);
                    self.people_pick = None;
                }
                _ => {}
            }
        }
    }

    /// Confirms removing a member from a channel before it happens.
    fn kick_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some((channel, user, name)) = self.kick_pending.clone() else {
            return;
        };
        let app = self.app.clone();
        let where_ = self.conversation_name(&channel);
        let mut cancel = false;
        let mut confirm = false;
        let modal_resp = modal(ctx, "remove_member").show(ctx, |ui| {
            ui.set_min_width(320.0);
            ui.heading(t!("gui.remove_member").to_string());
            ui.label(t!("gui.remove_member_confirm", name = name, channel = where_).to_string());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button(t!("gui.remove").to_string()).clicked() {
                    confirm = true;
                }
                if ui.button(t!("gui.cancel").to_string()).clicked() {
                    cancel = true;
                }
            });
        });
        if modal_resp.should_close() || cancel {
            self.kick_pending = None;
            return;
        }
        if confirm {
            app.kick_from_channel(channel, user);
            self.kick_pending = None;
        }
    }

    /// Opens the picker, enumerating the monitors and starting their previews.
    #[cfg(feature = "vp8")]
    fn open_share_picker(&mut self) {
        let screens = match screenshots::Screen::all() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("monitors could not be enumerated: {e}");
                return;
            }
        };
        if screens.is_empty() {
            tracing::warn!("no monitor to share");
            return;
        }
        let infos: Vec<MonitorInfo> = screens
            .iter()
            .map(|s| MonitorInfo {
                id: s.display_info.id,
                width: s.display_info.width,
                height: s.display_info.height,
                is_primary: s.display_info.is_primary,
            })
            .collect();
        let ids: Vec<u32> = infos.iter().map(|m| m.id).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        // Capturing is blocking: it stays off the graphics thread, and the
        // downscaled previews arrive while the window is already drawn.
        std::thread::spawn(move || {
            let Ok(screens) = screenshots::Screen::all() else {
                return;
            };
            for screen in screens {
                if !ids.contains(&screen.display_info.id) {
                    continue;
                }
                let Ok(img) = screen.capture() else {
                    continue;
                };
                let (w, h) = (img.width(), img.height());
                let Some(thumb) = downscale_thumbnail(img.rgba(), w, h) else {
                    continue;
                };
                if tx
                    .send(ShareThumb {
                        id: screen.display_info.id,
                        width: thumb.0,
                        height: thumb.1,
                        rgba: thumb.2,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.share_picker = Some(SharePicker {
            screens: infos,
            thumbs: HashMap::new(),
            rx,
        });
    }

    /// The monitor picker shown before a screen share starts.
    fn share_picker_dialog(&mut self, ctx: &egui::Context) {
        let mut got_new = false;
        let mut pending = false;
        if let Some(p) = &mut self.share_picker {
            loop {
                match p.rx.try_recv() {
                    Ok(t) => {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [t.width as usize, t.height as usize],
                            &t.rgba,
                        );
                        let tex = ctx.load_texture(
                            format!("share_thumb_{}", t.id),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        p.thumbs.insert(t.id, tex);
                        got_new = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        pending = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }
        if got_new {
            ctx.request_repaint();
        } else if pending {
            // The previews are still being captured on their own thread. Without
            // a frame request the poll would only run again on the next input,
            // and the thumbnails would appear late or not at all.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let mut chosen: Option<u32> = None;
        let mut cancel = false;
        let modal_resp = modal(ctx, "share_picker").show(ctx, |ui| {
            let Some(p) = &self.share_picker else { return };
            ui.set_min_width(440.0);
            ui.heading(t!("gui.share_screen_title").to_string());
            egui::Grid::new("share_grid")
                .num_columns(2)
                .spacing([12.0, 12.0])
                .show(ui, |ui| {
                    for (i, m) in p.screens.iter().enumerate() {
                        let card = ui.group(|ui| {
                            ui.set_width(200.0);
                            ui.vertical(|ui| {
                                match p.thumbs.get(&m.id) {
                                    Some(tex) => {
                                        ui.add(
                                            egui::Image::new(tex)
                                                .max_size(egui::vec2(190.0, 110.0)),
                                        );
                                    }
                                    None => {
                                        ui.add_space(30.0);
                                        ui.label(
                                            egui::RichText::new(
                                                t!("gui.preview_loading").to_string(),
                                            )
                                            .weak(),
                                        );
                                        ui.add_space(30.0);
                                    }
                                }
                                let primary = if m.is_primary {
                                    t!("gui.screen_primary").to_string()
                                } else {
                                    String::new()
                                };
                                ui.label(
                                    t!(
                                        "gui.screen_label",
                                        n = i + 1,
                                        w = m.width,
                                        h = m.height,
                                        primary = primary
                                    )
                                    .to_string(),
                                );
                            });
                        });
                        if card
                            .response
                            .interact(egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            chosen = Some(m.id);
                        }
                        if i % 2 == 1 {
                            ui.end_row();
                        }
                    }
                });
            ui.add_space(8.0);
            if ui.button(t!("gui.cancel").to_string()).clicked() {
                cancel = true;
            }
        });

        if modal_resp.should_close() || cancel {
            self.share_picker = None;
        }
        if let Some(id) = chosen {
            self.app.toggle_call_screen_share(Some(id));
            self.share_picker = None;
        }
    }

    /// Removes from the events the keys the completion needs.
    ///
    /// It has to be called **before** drawing the composer: `TextEdit` consumes
    /// Enter and Tab, and after it there is nothing left to intercept.
    fn completion_keys(&mut self, ui: &mut egui::Ui) -> Option<CompletionKey> {
        if self.complete_items.is_empty() {
            return None;
        }
        ui.input_mut(|i| {
            use egui::{Key, Modifiers};
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                return Some(CompletionKey::Cancel);
            }
            if i.consume_key(Modifiers::NONE, Key::Enter)
                || i.consume_key(Modifiers::NONE, Key::Tab)
            {
                return Some(CompletionKey::Choose);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                return Some(CompletionKey::Down);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                return Some(CompletionKey::Up);
            }
            None
        })
    }

    /// Recomputes the entries from what is being typed.
    fn completion_update(&mut self, cursor: usize, focused: bool) {
        self.complete_items.clear();
        self.complete_span = None;
        if !focused {
            return;
        }
        // The cursor is in characters, the text in bytes. Work on whichever
        // composer currently has focus.
        let text: &str = if self.complete_in_thread {
            &self.thread_input
        } else {
            &self.input
        };
        let byte_cursor = text
            .char_indices()
            .nth(cursor)
            .map(|(i, _)| i)
            .unwrap_or(text.len());

        // A slash command at the very start: a small static list, filtered by
        // prefix. The label is also its "slack form", so choosing it is a plain
        // text insertion (nothing for `resolve_mentions` to substitute).
        if let Some(frag) = slash_command_fragment(text, byte_cursor) {
            let q = frag.to_lowercase();
            let mut items: Vec<(String, String)> = SLASH_COMMANDS
                .iter()
                // The typed fragment has no leading `/` (it is consumed as the
                // sigil), but the commands all carry it.
                .filter(|c| c.strip_prefix('/').unwrap_or(c).starts_with(&q))
                .map(|c| ((*c).to_string(), (*c).to_string()))
                .collect();
            items.truncate(8);
            if items.is_empty() {
                return;
            }
            self.complete_sel = self.complete_sel.min(items.len() - 1);
            self.complete_span = Some((0, 1 + frag.len()));
            self.complete_items = items;
            return;
        }

        let (sigil, at, frag) = match crate::app::mention_prefix(text, byte_cursor, '@') {
            Some((a, f)) => ('@', a, f),
            None => match crate::app::mention_prefix(text, byte_cursor, '#') {
                Some((a, f)) => ('#', a, f),
                None => return,
            },
        };

        let q = frag.to_lowercase();
        let mut items: Vec<(String, String)> = if sigil == '@' {
            let mut v: Vec<_> = self
                .snap
                .user_names
                .iter()
                .filter(|(_, n)| n.to_lowercase().contains(&q))
                // The label is already what ends up in the composer: `@Name`
                // for people, `#channel` for channels.
                .map(|(id, n)| (format!("@{n}"), format!("<@{id}>")))
                .collect();
            v.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
            v
        } else {
            self.snap
                .channels
                .iter()
                .filter(|c| !c.is_dm && c.name.to_lowercase().contains(&q))
                .map(|c| (format!("#{}", c.name), format!("<#{}|{}>", c.id, c.name)))
                .collect()
        };
        items.truncate(8);
        if items.is_empty() {
            return;
        }
        self.complete_sel = self.complete_sel.min(items.len() - 1);
        self.complete_span = Some((at, at + sigil.len_utf8() + frag.len()));
        self.complete_items = items;
    }

    /// Draws the popup and applies the choice. True if it consumed the Enter.
    fn completion_draw(&mut self, ui: &mut egui::Ui, action: Option<CompletionKey>) -> bool {
        if self.complete_items.is_empty() {
            return false;
        }
        match action {
            Some(CompletionKey::Cancel) => {
                self.complete_items.clear();
                return true;
            }
            Some(CompletionKey::Down) => {
                self.complete_sel = (self.complete_sel + 1) % self.complete_items.len();
            }
            Some(CompletionKey::Up) => {
                let n = self.complete_items.len();
                self.complete_sel = (self.complete_sel + n - 1) % n;
            }
            _ => {}
        }

        let items = self.complete_items.clone();
        let mut chosen = matches!(action, Some(CompletionKey::Choose)).then_some(self.complete_sel);

        // Anchored above the composer: inside the bottom panel the
        // layout coordinates are not enough to place it.
        let top = self.composer_rect.left_top();
        egui::Area::new(egui::Id::new("completamento"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(top.x, top.y - 8.0 - 22.0 * items.len() as f32))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(240.0);
                    for (i, (label, _)) in items.iter().enumerate() {
                        if selectable(ui, i == self.complete_sel, label).clicked() {
                            chosen = Some(i);
                        }
                    }
                });
            });

        if let Some(i) = chosen {
            if let Some((a, b)) = self.complete_span {
                let (visible, slack_form) = &items[i];
                // The readable text goes in the composer; the `<@U123>` form is
                // applied on send.
                let inserted = format!("{visible} ");
                let at_char = {
                    let buf = if self.complete_in_thread {
                        &mut self.thread_input
                    } else {
                        &mut self.input
                    };
                    buf.replace_range(a..b, &inserted);
                    buf[..a + inserted.len()].chars().count()
                };
                self.pending_mentions
                    .push((visible.clone(), slack_form.clone()));

                // The cursor goes after what was inserted, otherwise it stays
                // where it was and you go on typing in the middle of the mention.
                self.set_cursor(ui.ctx(), at_char);
            }
            self.complete_items.clear();
            self.complete_span = None;
            self.complete_sel = 0;
            return true;
        }
        false
    }

    /// Brings the composer's cursor to a precise position.
    fn set_cursor(&self, ctx: &egui::Context, char_index: usize) {
        let Some(id) = self.composer_id else { return };
        if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
            let c = egui::text::CCursor::new(char_index);
            st.cursor
                .set_char_range(Some(egui::text::CCursorRange::one(c)));
            st.store(ctx, id);
        }
    }

    /// Translates the chosen mentions into the form Slack understands.
    fn resolve_mentions(&mut self, text: &str) -> String {
        let out = crate::app::resolve_mentions(text, &self.pending_mentions);
        self.pending_mentions.clear();
        out
    }

    /// Intercepts `Ctrl+V` when there is an image on the clipboard.
    ///
    /// If there is, it uploads it and removes the paste event, otherwise the
    /// composer would write any leftover text into itself. If there is not, it
    /// touches nothing and the text paste goes on normally.
    ///
    /// It watches the **release** of `V`, not the press: `egui_winit` recognises
    /// `Ctrl+V` as a clipboard command and returns without emitting the key event,
    /// so nothing arrives here on the way down. The release does not go through
    /// that check and reaches us whole.
    fn handle_paste(&mut self, ui: &mut egui::Ui, thread: Option<String>) {
        let (released, ctrl) = ui.input(|i| {
            (
                i.key_released(egui::Key::V),
                i.modifiers.command || i.modifiers.ctrl,
            )
        });
        if !(released && (ctrl || self.ctrl_before)) {
            return;
        }
        self.paste_image(thread);
    }

    /// Uploads the image on the clipboard, if there is one.
    ///
    /// egui only exposes the clipboard's text, so `Ctrl+V` on a screenshot did
    /// nothing. We read it from the system directly and save a temporary PNG,
    /// because uploading to Slack starts from a file.
    ///
    /// True if something was uploaded: in that case the text paste must not
    /// happen, or the file path would be added to the composer.
    fn paste_image(&mut self, thread: Option<String>) -> bool {
        let Some(channel) = self.current_channel() else {
            return false;
        };
        let mut board = match arboard::Clipboard::new() {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!("clipboard not readable: {e}");
                return false;
            }
        };
        let img = match board.get_image() {
            Ok(i) => i,
            // No image: this is the normal case, the paste goes on with the text.
            Err(_) => return false,
        };

        let (w, h) = (img.width as u32, img.height as u32);
        let Some(buf) = image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) else {
            tracing::warn!("clipboard image with inconsistent size");
            return false;
        };

        // The name carries the date so several pastes in one conversation are
        // distinguishable, plus a short id so it is unique.
        let name = format!(
            "incollata-{}-{}.png",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().simple().to_string()[..8],
        );
        let path = std::env::temp_dir().join(&name);
        // `create_new` (O_EXCL) refuses to follow or overwrite an existing
        // entry, so a symlink planted in /tmp cannot redirect the write; and
        // the bytes go through the handle we just opened, not a second by-path
        // open, so there is no window to swap the file.
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("clipboard image not saved: {e}");
                return false;
            }
        };
        if let Err(e) =
            image::DynamicImage::ImageRgba8(buf).write_to(&mut file, image::ImageFormat::Png)
        {
            tracing::warn!("image not saved: {e}");
            let _ = std::fs::remove_file(&path);
            return false;
        }
        drop(file);

        let comment = if thread.is_some() {
            std::mem::take(&mut self.thread_input)
        } else {
            std::mem::take(&mut self.input)
        };
        self.app.upload(channel, path, comment, thread);
        true
    }

    /// Opens the system file picker.
    ///
    /// The dialog is blocking, so it runs in a separate task: opening it on the
    /// graphics thread would freeze the window until a choice is made.
    ///
    /// With `thread` set the attachment ends up inside that thread, and the
    /// comment comes from the thread's box rather than the channel's: they are two
    /// distinct compositions and must not be mixed.
    fn pick_file(&mut self, thread: Option<String>) {
        // An attachment aimed at a thread goes to that thread's conversation,
        // which may not be the one currently selected.
        let channel = if thread.is_some() {
            self.snap
                .thread_channel
                .clone()
                .or_else(|| self.current_channel())
        } else {
            self.current_channel()
        };
        let Some(channel) = channel else {
            return;
        };
        let comment = if thread.is_some() {
            std::mem::take(&mut self.thread_input)
        } else {
            std::mem::take(&mut self.input)
        };
        let app = self.app.clone();
        self.rt.spawn(async move {
            if let Some(files) = rfd::AsyncFileDialog::new().pick_files().await {
                let mut comment = comment;
                for f in files {
                    app.upload(
                        channel.clone(),
                        f.path().to_path_buf(),
                        std::mem::take(&mut comment),
                        thread.clone(),
                    );
                }
            }
        });
    }

    /// True while a modal dialog is on screen.
    ///
    /// Click and keyboard input to the UI underneath is blocked by the modal
    /// itself, but **drag-and-drop is `InputState::raw`**, not an `Event`, so it
    /// reaches the window anyway. Callers that act on raw input must check this.
    fn modal_open(&self) -> bool {
        if self.snap.session_expired
            || self.forwarding.is_some()
            || self.new_channel.is_some()
            || self.people_pick.is_some()
            || self.kick_pending.is_some()
        {
            return true;
        }
        #[cfg(feature = "mods")]
        if self.snap.mod_dialog.is_some() {
            return true;
        }
        #[cfg(feature = "vp8")]
        if self.share_picker.is_some() {
            return true;
        }
        false
    }

    /// Files dragged onto the window: they are uploaded to the open channel.
    ///
    /// The text already written in the composer becomes the comment, as Slack
    /// does.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        // A modal is on screen: a file dropped behind it must not be sent (drag
        // and drop is `InputState::raw`, not an `Event`, so the modal's own
        // input blocking does not cover it).
        if self.modal_open() {
            return;
        }
        // Feedback during the drag, otherwise there is no telling that the
        // window accepts the file.
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering {
            let r = ctx.screen_rect();
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop"),
            ));
            p.rect_filled(r, 0.0, pal().tint(pal().scrim, 160));
            p.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                t!("gui.drop_to_send").to_string(),
                egui::FontId::proportional(24.0),
                pal().text_primary,
            );
        }

        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        // A file dropped on the thread panel belongs to the thread: having
        // it land in the channel would be a silent mistake, and one everybody sees.
        let on_thread = ctx
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|p| self.thread_rect.contains(p));
        let thread = on_thread.then(|| self.snap.thread_parent.clone()).flatten();
        let channel = if thread.is_some() {
            self.snap
                .thread_channel
                .clone()
                .or_else(|| self.current_channel())
        } else {
            self.current_channel()
        };
        let Some(channel) = channel else {
            return;
        };

        // The comment goes with the first file only: repeating it on each
        // would fill the channel.
        let mut comment = if thread.is_some() {
            std::mem::take(&mut self.thread_input)
        } else {
            std::mem::take(&mut self.input)
        };
        for path in dropped {
            self.app.upload(
                channel.clone(),
                path,
                std::mem::take(&mut comment),
                thread.clone(),
            );
        }
    }

    /// The full-resolution image above everything else.
    ///
    /// It closes with Esc or a click outside, as you expect from a lightbox; the
    /// button opens the original on Slack.
    fn image_viewer(&mut self, ctx: &egui::Context) {
        let Some((url, name, permalink)) = self.viewing.clone() else {
            return;
        };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.viewing = None;
            return;
        }

        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("visore"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                // A dark background over the whole window: it also serves to
                // intercept the clicks outside the image.
                let bg = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, pal().tint(pal().scrim, 230));

                let tex = self.texture(ctx, &url);
                let mut hit_image = false;

                ui.scope_builder(egui::UiBuilder::new().max_rect(screen.shrink(24.0)), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&name).strong());
                            if ui.button(t!("gui.open_on_slack").to_string()).clicked()
                                && !permalink.is_empty()
                            {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(&permalink));
                            }
                            if ui.button(t!("gui.close_esc").to_string()).clicked() {
                                self.viewing = None;
                            }
                        });
                        match &tex {
                            Some(t) => {
                                let avail = screen.size() - egui::vec2(48.0, 90.0);
                                let sz = t.size_vec2();
                                let k = (avail.x / sz.x).min(avail.y / sz.y).min(1.0);
                                hit_image = ui
                                    .add(
                                        egui::Image::new(t)
                                            .fit_to_exact_size(sz * k)
                                            .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .clicked();
                            }
                            None => {
                                ui.add_space(40.0);
                                ui.weak(t!("gui.loading").to_string());
                            }
                        }
                    });
                });

                // A click on the image does not close; one on the background does.
                if bg.clicked() && !hit_image {
                    self.viewing = None;
                }
            });
    }

    /// A shared file: a thumbnail if it is an image, otherwise a row with
    /// the name that opens the file on Slack.
    fn file(&mut self, ui: &mut egui::Ui, f: &crate::app::FileEntry, w: f32) {
        if f.is_image() {
            if let Some(tex) = self.texture(ui.ctx(), &f.thumb) {
                // The thumbnail keeps its proportions inside a box: Slack's
                // images go up to a few thousand pixels.
                let max = egui::vec2(w.min(420.0), 300.0);
                let size = tex.size_vec2();
                let k = (max.x / size.x).min(max.y / size.y).min(1.0);
                let r = ui
                    .add(
                        egui::Image::new(&tex)
                            .fit_to_exact_size(size * k)
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_text(&f.name)
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if r.clicked() {
                    let full = if f.url.is_empty() { &f.thumb } else { &f.url };
                    self.viewing = Some((full.clone(), f.name.clone(), f.permalink.clone()));
                }
                return;
            }
            // Not downloaded yet: the space is reserved so the text does
            // not jump when it arrives.
            ui.add_sized([120.0, 90.0], egui::Label::new("…"));
            return;
        }

        let label = if f.size > 0 {
            format!("\u{1f4ce} {}  ({})", f.name, human_size(f.size))
        } else {
            format!("\u{1f4ce} {}", f.name)
        };
        let r = ui
            .add(
                egui::Label::new(egui::RichText::new(label).color(ui.visuals().hyperlink_color))
                    .wrap()
                    .sense(egui::Sense::click())
                    .selectable(false),
            )
            .on_hover_text(t!("gui.download_and_open_hover").to_string())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if r.clicked() {
            let url = if f.download_url.is_empty() {
                &f.url
            } else {
                &f.download_url
            };
            if !url.is_empty() {
                self.app.download_and_open(url.clone(), f.name.clone());
            }
        }
        r.context_menu(|ui| {
            if ui.button(t!("gui.open_on_slack").to_string()).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&f.permalink));
                ui.close_menu();
            }
        });
    }

    /// An attachment: a coloured bar on the left, a title, a body and a preview.
    ///
    /// Bots put all the content in it leaving `text` empty, so without this an
    /// alerts channel is a list of empty rows. An attachment with nothing to
    /// show at all paints nothing — not even its bar (see below).
    fn attachment(&mut self, ui: &mut egui::Ui, a: &crate::app::Attachment, w: f32) {
        // Nothing to show — no title, no text, no picture — so nothing is drawn.
        // An app's card can arrive carrying only a `color` (its fields come to
        // us as the message text), and drawing the bar alone left a stray
        // coloured line hanging under the message: the reported "side line".
        if a.title.is_empty() && a.text.is_empty() && a.thumb.is_empty() {
            return;
        }
        let stripe = parse_hex(&a.color).unwrap_or(ui.visuals().weak_text_color());
        // The column to the right of the bar has to be limited by hand:
        // inside a horizontal layout egui would give it unlimited width, and a
        // long title — alerts have 300-character ones — blows through the window
        // dragging the whole message list with it.
        let inner = (w - 10.0).max(80.0);
        ui.allocate_ui(egui::vec2(w, 0.0), |ui| {
            ui.horizontal_top(|ui| {
                // The bar is drawn after measuring the content: with a guessed
                // height it ended halfway up the box and the preview did not read as
                // one.
                let stripe_top = ui.cursor().min;
                // The space covers the bar plus the margin.
                ui.add_space(ATTACHMENT_STRIPE_W + ATTACHMENT_STRIPE_GAP);
                let content = ui.vertical(|ui| {
                    ui.set_max_width(inner);
                    if !a.title.is_empty() {
                        let f = egui::TextStyle::Body.resolve(ui.style());
                        if a.title_link.is_empty() {
                            ui.add(wrapped(
                                a.title.clone(),
                                f.clone(),
                                ui.visuals().text_color(),
                                Wrap::Words,
                            ));
                        } else {
                            // `hyperlink_to` does not wrap: a long title widened
                            // the box and the whole view with it. We draw a label that
                            // wraps and open the URL by hand.
                            let r = ui
                                .add(
                                    wrapped(
                                        a.title.clone(),
                                        f,
                                        ui.visuals().hyperlink_color,
                                        Wrap::Words,
                                    )
                                    .sense(egui::Sense::click())
                                    .selectable(false),
                                )
                                .on_hover_text(&a.title_link)
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if r.clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(&a.title_link));
                            }
                        }
                    }
                    if !a.text.is_empty() {
                        self.body(ui, &a.text, inner);
                    }
                    if !a.thumb.is_empty() {
                        // A link preview's picture — never fetched before, so a
                        // preview showed as text only. Same treatment as a file's
                        // image: click to open the full one.
                        if let Some(tex) = self.texture(ui.ctx(), &a.thumb) {
                            let max = egui::vec2(inner.min(420.0), 320.0);
                            let size = tex.size_vec2();
                            let k = (max.x / size.x).min(max.y / size.y).min(1.0);
                            let r = ui
                                .add(
                                    egui::Image::new(&tex)
                                        .fit_to_exact_size(size * k)
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if r.clicked() {
                                let full = if a.image.is_empty() {
                                    a.thumb.clone()
                                } else {
                                    a.image.clone()
                                };
                                self.viewing = Some((full, a.title.clone(), a.title_link.clone()));
                            }
                        } else {
                            // Not downloaded yet: reserve the space so the text does
                            // not jump when it arrives.
                            ui.add_sized([120.0, 90.0], egui::Label::new("\u{2026}"));
                        }
                    }
                });
                let h = content.response.rect.height().max(14.0);
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(stripe_top, egui::vec2(ATTACHMENT_STRIPE_W, h)),
                    1.5,
                    stripe,
                );
            });
        });
    }

    /// The message body, interpreted as mrkdwn.
    fn body(&mut self, ui: &mut egui::Ui, text: &str, width: f32) {
        for block in mrkdwn::parse(text) {
            match block {
                mrkdwn::Block::CodeBlock(code) => {
                    // Kept on a horizontal scroll, not wrapped (wrapping would
                    // mangle indentation-sensitive code). The scroll area is
                    // given an explicit MAX size: a non-wrapping label reports
                    // its full content width, and inside a resizable panel that
                    // made the panel grow a sliver every frame.
                    let lines = code.lines().count().max(1) as f32;
                    let h = lines * ui.text_style_height(&egui::TextStyle::Monospace) + 12.0;
                    egui::Frame::none()
                        .fill(pal().code_bg)
                        .inner_margin(6.0)
                        .rounding(4.0)
                        .show(ui, |ui| {
                            // Solid bar here (the rest of the UI uses floating
                            // ones): an overflow must be visibly scrollable.
                            ui.style_mut().spacing.scroll.floating = false;
                            egui::ScrollArea::horizontal()
                                .max_width((width - 12.0).max(40.0))
                                .max_height(h)
                                .id_salt(("code", code.len()))
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(code).monospace())
                                            .extend(),
                                    );
                                });
                        });
                }
                mrkdwn::Block::Quote(spans) => {
                    // A quote gets its own faint background and a left bar that
                    // spans the whole block, not a fixed-height stub: the bar is
                    // drawn over the frame once its height is known.
                    let bar = pal().quote_border;
                    let frame = egui::Frame::none()
                        .fill(ui.visuals().faint_bg_color)
                        .rounding(4.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 3.0))
                        .show(ui, |ui| {
                            ui.set_min_width((width - 16.0).max(0.0));
                            self.spans(ui, &spans);
                        });
                    let r = frame.response.rect;
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(r.left_top(), egui::vec2(3.0, r.height())),
                        1.0,
                        bar,
                    );
                }
                mrkdwn::Block::Paragraph(spans) => {
                    ui.allocate_ui(egui::vec2(width, 0.0), |ui| self.spans(ui, &spans));
                }
            }
        }
    }

    fn spans(&mut self, ui: &mut egui::Ui, spans: &[Span]) {
        // The click is collected and acted on afterwards: inside the
        // closure `self` is already borrowed by the rendering.
        let mut clicked_user: Option<(String, String)> = None;
        let mut clicked_channel: Option<(String, String)> = None;
        let mut clicked_link: Option<(String, String)> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            self.span_list(
                ui,
                spans,
                Emph::default(),
                &mut clicked_user,
                &mut clicked_channel,
                &mut clicked_link,
            );
        });
        if let Some((id, name)) = clicked_user {
            self.app.open_dm(id, name);
        } else if let Some((id, name)) = clicked_channel {
            // A `#channel` reference opens the channel, as in the official
            // client, when it is one of ours.
            self.app.open_channel(id, name);
        } else if let Some((channel, ts)) = clicked_link {
            // A message permalink opens in the app, not in the browser.
            let (name, is_dm, peer) = self
                .snap
                .channels
                .iter()
                .find(|c| c.id == channel)
                .map(|c| (c.name.clone(), c.is_dm, c.user.clone()))
                .unwrap_or_else(|| (channel.clone(), channel.starts_with('D'), None));
            self.app
                .jump_in_conversation(channel.clone(), name, is_dm, peer, ts.clone());
            self.highlight_ts = Some(ts.clone());
            self.highlight_channel = Some(channel);
            self.highlight_offset = None;
            self.thread_jump = Some(ts);
        }
    }

    /// Draws a list of spans carrying the inherited emphasis along.
    ///
    /// Emphasis is not a span type of its own but a property of the ones it
    /// contains: a link inside a bold stays a link, and without this it ended up
    /// on screen as raw markup.
    fn span_list(
        &mut self,
        ui: &mut egui::Ui,
        spans: &[Span],
        emph: Emph,
        clicked_user: &mut Option<(String, String)>,
        clicked_channel: &mut Option<(String, String)>,
        clicked_link: &mut Option<(String, String)>,
    ) {
        use egui::Widget as _;
        {
            for s in spans {
                match s {
                    Span::Text(t) => {
                        let f = egui::TextStyle::Body.resolve(ui.style());
                        // Bold keeps the brighter colour; the heavy face is
                        // selected inside `wrapped_emph` from `emph`.
                        let col = if emph.bold {
                            ui.visuals().strong_text_color()
                        } else {
                            ui.visuals().text_color()
                        };
                        // A `Label` that wraps while it is not at the start of
                        // its row is given a leading space equal to the offset
                        // from the row start (`Label::layout_in_ui` sets
                        // `first_row_indentation` to that offset) and its
                        // *continuation* rows come out at `-offset`. A wide span
                        // can therefore extend left of the column and, through
                        // `expand_to_include_rect`, drag the whole list region
                        // left: the following rows lose their margin and their
                        // avatars are clipped. Sending such a span to a fresh row
                        // first zeroes the offset, so it wraps in place.
                        //
                        // Deliberately broad: any span wider than the column gets
                        // its own line (it could not share one anyway). We do not
                        // first ask epaint whether it would spill, because that
                        // depends on the exact glyphs.
                        let mut meas = f.clone();
                        meas.family = emph.family();
                        let avail = ui.available_width();
                        let at_row_start = ui.cursor().min.x <= ui.max_rect().left() + 0.5;
                        let wider_than_col = ui.fonts(|fo| {
                            fo.layout_no_wrap(t.clone(), meas, egui::Color32::PLACEHOLDER)
                                .size()
                                .x
                        }) > avail;
                        if wider_than_col && !at_row_start {
                            ui.end_row();
                        }
                        ui.add(wrapped_emph(t.clone(), f, col, emph, Wrap::Words));
                    }
                    Span::Bold(v) => {
                        self.span_list(
                            ui,
                            v,
                            emph.bold(),
                            clicked_user,
                            clicked_channel,
                            clicked_link,
                        );
                    }
                    Span::Italic(v) => {
                        self.span_list(
                            ui,
                            v,
                            emph.italic(),
                            clicked_user,
                            clicked_channel,
                            clicked_link,
                        );
                    }
                    Span::Strike(v) => {
                        self.span_list(
                            ui,
                            v,
                            emph.strike(),
                            clicked_user,
                            clicked_channel,
                            clicked_link,
                        );
                    }
                    Span::Code(t) => {
                        // Inline code needs the box Slack gives it: monospace
                        // alone barely differs from body text, which is exactly
                        // what "not highlighted" looks like. The background goes
                        // in the text format so it follows the text across a wrap.
                        let f = egui::TextStyle::Monospace.resolve(ui.style());
                        let mut job = egui::text::LayoutJob::simple(
                            t.clone(),
                            f,
                            ui.visuals().text_color(),
                            f32::INFINITY,
                        );
                        job.wrap.break_anywhere = true;
                        if let Some(sec) = job.sections.first_mut() {
                            sec.format.background = pal().code_bg;
                        }
                        ui.add(egui::Label::new(job));
                    }
                    Span::Link { url, text } => {
                        // `hyperlink_to` does not wrap: we draw a label that
                        // does and open the URL by hand.
                        let f = egui::TextStyle::Body.resolve(ui.style());
                        let r = ui
                            .add(
                                wrapped_emph(
                                    text.clone(),
                                    f,
                                    ui.visuals().hyperlink_color,
                                    emph,
                                    Wrap::Anywhere,
                                )
                                .sense(egui::Sense::click())
                                .selectable(false),
                            )
                            .on_hover_text(url)
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if r.clicked() {
                            match crate::app::parse_message_permalink(&url) {
                                Some((channel, ts)) => *clicked_link = Some((channel, ts)),
                                None => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
                            }
                        }
                    }
                    Span::User(id) => {
                        let known = self.snap.user_names.get(id).cloned();
                        let name = known.clone().unwrap_or_else(|| id.clone());
                        let mut mention = egui::RichText::new(format!("@{name}"));
                        if !self.snap.me.is_empty() && id == &self.snap.me {
                            // A mention of yourself is highlighted, as in the
                            // official client.
                            mention = mention
                                .background_color(pal().mention_self_bg)
                                .color(pal().mention_self_fg)
                                .strong();
                        } else {
                            mention = mention.color(pal().mention_other_fg);
                        }
                        let label = egui::Label::new(mention);
                        // A mention leads to the conversation with that person,
                        // as in the official client.
                        if known.is_some() {
                            let r = label
                                .sense(egui::Sense::click())
                                .selectable(false)
                                .ui(ui)
                                .on_hover_text(t!("gui.write_to", who = name).to_string())
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if r.clicked() {
                                *clicked_user = Some((id.clone(), name));
                            }
                        } else {
                            ui.add(label);
                        }
                    }
                    Span::Channel { id, label } => {
                        // A channel reference opens that channel, as in the
                        // official client.
                        let text = egui::RichText::new(format!("#{label}"))
                            .color(ui.visuals().hyperlink_color);
                        let r = ui
                            .add(
                                egui::Label::new(text)
                                    .sense(egui::Sense::click())
                                    .selectable(false),
                            )
                            .on_hover_text(t!("gui.go_to_channel", name = label).to_string())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if r.clicked() {
                            *clicked_channel = Some((id.clone(), label.clone()));
                        }
                    }
                    Span::Broadcast(b) => {
                        ui.label(
                            egui::RichText::new(format!("@{b}"))
                                .strong()
                                .color(pal().mention_other_fg),
                        );
                    }
                    Span::Emoji(name) => self.emoji(ui, name),
                }
            }
        }
    }

    /// An emoji: an image if it is a workspace custom one, a Unicode
    /// character if it is standard, and `:name:` only when it is neither.
    fn emoji(&mut self, ui: &mut egui::Ui, name: &str) {
        // The workspace's first: a custom `:deploy:` is in no standard
        // set.
        let url = self.snap.emoji.get(name).cloned().unwrap_or_default();
        if let Some(tex) = self.texture(ui.ctx(), &url) {
            ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(18.0, 18.0)));
            return;
        }

        // Then the colour image. An image covers the whole codepoint
        // sequence, so it resolves by itself what a font does not compose: ZWJ
        // sequences, skin tones, flags.
        if let Some(seq) = mrkdwn::emoji_sequence(name) {
            let url = format!("{EMOJI_CDN}/{seq}@2x.png");
            if let Some(tex) = self.texture(ui.ctx(), &url) {
                ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(18.0, 18.0)));
                return;
            }
        }

        // A glyph the font does not have is drawn as an empty box, which
        // says nothing: the short name is uglier but readable.
        match mrkdwn::unicode_emoji(name).filter(|ch| drawable(ui, ch)) {
            Some(ch) => {
                ui.label(ch).on_hover_text(format!(":{name}:"));
            }
            None => {
                ui.label(
                    egui::RichText::new(format!(":{name}:"))
                        .small()
                        .color(pal().text_muted),
                );
            }
        }
    }

    /// Who put a reaction, for the hover.
    ///
    /// A count says how many agreed; the question being asked is usually who.
    /// Slack stops at a few names and counts the rest, and so do we: a
    /// tooltip with forty names is no longer a tooltip.
    fn who_reacted(&self, r: &Reaction) -> String {
        const SHOWN: usize = 8;
        if r.users.is_empty() {
            return format!(":{}:", r.name);
        }
        let mut names: Vec<String> = r
            .users
            .iter()
            .take(SHOWN)
            .map(|u| {
                if *u == self.snap.me {
                    t!("gui.you").to_string()
                } else {
                    self.snap
                        .user_names
                        .get(u)
                        .cloned()
                        .unwrap_or_else(|| u.clone())
                }
            })
            .collect();
        let rest = r.users.len().saturating_sub(SHOWN);
        if rest > 0 {
            names.push(t!("gui.and_others", n = rest).to_string());
        }
        t!("gui.reacted", names = names.join(", "), emoji = r.name).to_string()
    }

    /// The texture for a bundled monochrome icon, decoded and uploaded once.
    ///
    /// These are local PNGs, not URLs: they never touch the download cache.
    /// The cache key is the icon's static name.
    fn icon_texture(&mut self, ctx: &egui::Context, name: &'static str) -> egui::TextureHandle {
        if let Some(tex) = self.icon_textures.get(name) {
            return tex.clone();
        }
        let bytes: &[u8] = match name {
            "smile-plus" => include_bytes!("data/icons/smile-plus.png"),
            "paperclip" => include_bytes!("data/icons/paperclip.png"),
            "search" => include_bytes!("data/icons/search.png"),
            "reply" => include_bytes!("data/icons/reply.png"),
            "ellipsis" => include_bytes!("data/icons/ellipsis.png"),
            "send" => include_bytes!("data/icons/send.png"),
            "check" => include_bytes!("data/icons/check.png"),
            "bookmark" => include_bytes!("data/icons/bookmark.png"),
            "pin" => include_bytes!("data/icons/pin.png"),
            "clock" => include_bytes!("data/icons/clock.png"),
            _ => &[],
        };
        let image = image::load_from_memory(bytes)
            .map(|img| {
                let img = img.to_rgba8();
                egui::ColorImage::from_rgba_unmultiplied(
                    [img.width() as usize, img.height() as usize],
                    img.as_raw(),
                )
            })
            .unwrap_or_else(|_| egui::ColorImage::new([1, 1], egui::Color32::TRANSPARENT));
        let tex = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
        self.icon_textures.insert(name, tex.clone());
        tex
    }

    /// The uploaded texture for an emoji, custom workspace ones first.
    ///
    /// `None` while it is still downloading, or without a network.
    fn emoji_texture(&mut self, ctx: &egui::Context, name: &str) -> Option<egui::TextureHandle> {
        let url = match self.snap.emoji.get(name) {
            Some(u) => u.clone(),
            None => mrkdwn::emoji_sequence(name)
                .map(|seq| format!("{EMOJI_CDN}/{seq}@2x.png"))
                .unwrap_or_default(),
        };
        self.texture(ctx, &url)
    }

    /// A clickable emoji icon, drawn like the ones in the messages, without a
    /// tooltip.
    ///
    /// The picker used the font glyph, so it showed grey squares next to the
    /// colour emoji of the conversation — and the ones the font does not have
    /// were a blank button you could not tell apart from the others.
    fn emoji_icon_button(&mut self, ui: &mut egui::Ui, name: &str) -> egui::Response {
        const SIZE: f32 = 20.0;
        if let Some(tex) = self.emoji_texture(ui.ctx(), name) {
            return ui.add(egui::ImageButton::new(
                egui::Image::new(&tex).fit_to_exact_size(egui::vec2(SIZE, SIZE)),
            ));
        }
        // While the image is downloading, or without a network: the glyph is
        // better than an empty button.
        let label = mrkdwn::unicode_emoji(name)
            .filter(|ch| drawable(ui, ch))
            .map(str::to_string)
            .unwrap_or_else(|| format!(":{name}:"));
        ui.button(label)
    }

    /// A clickable emoji, drawn like the ones in the messages.
    fn emoji_button(&mut self, ui: &mut egui::Ui, name: &str) -> bool {
        self.emoji_icon_button(ui, name)
            .on_hover_text(format!(":{name}:"))
            .clicked()
    }

    /// Evicts the oldest cached textures until the cache is back within
    /// `TEXTURE_CACHE_MAX`. Called at the start of a frame, so the textures
    /// used while drawing the current frame are never evicted before it ends.
    fn evict_textures(&mut self) {
        while self.textures.len() > TEXTURE_CACHE_MAX {
            match self.texture_order.pop_front() {
                Some(url) => {
                    self.textures.remove(&url);
                    // Allow it to be loaded again from the disk cache; without
                    // this the URL stays "seen" and the image is gone for good.
                    self.images.unsee(&url);
                }
                None => break,
            }
        }
    }

    /// Tell Slack we are active while the app runs.
    ///
    /// `users.setPresence` was only ever called from the profile popover, so a
    /// client that never opened it stayed **offline** for everyone else. It now
    /// runs as a keepalive for as long as the app is up — *not* only while the
    /// window is focused: working in another window was making the account drop
    /// offline again. The result is read back and logged rather than assumed (see
    /// `docs/presence.md`: `auto` clears a manual away, it cannot force active).
    fn drive_presence(&mut self) {
        // An explicit Away (the popover) is the user's and is respected. Without
        // one, presence is asserted while the app runs — the window being in the
        // background is not being away, and gating on focus was the bug.
        let manual_away = self.presence_manual == Some(false);
        let now = std::time::Instant::now();
        if !presence_keepalive_due(self.presence_sent, now, manual_away) {
            return;
        }
        self.presence_sent = Some(now);
        self.app.keepalive_presence();
    }

    /// The texture for a URL, uploading it on first use.
    ///
    /// Textures can only be created from the graphics thread, so the download
    /// lives elsewhere ([`Images`]) and here we convert what has already
    /// arrived.
    fn texture(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        if url.is_empty() {
            return None;
        }
        // An animated GIF is held as its own set of frames; return the one due now.
        if self.anim.contains_key(url) {
            return self.anim_frame(ctx, url);
        }
        if let Some(t) = self.textures.get(url) {
            let t = t.clone();
            // Refresh the insertion order: a texture in use must not be the
            // next one evicted just because it was created a while ago.
            if let Some(pos) = self.texture_order.iter().position(|u| u == url) {
                if let Some(u) = self.texture_order.remove(pos) {
                    self.texture_order.push_back(u);
                }
            }
            return Some(t);
        }

        self.images.request(url);

        let bytes = {
            let loaded = match self.images.loaded().lock() {
                Ok(l) => l,
                Err(_) => return None,
            };
            match loaded.get(url) {
                Some(b) => b.clone(),
                None => {
                    // The download happens in a separate task. Without asking
                    // for a repaint the image would appear only the next time
                    // something else moved the interface — and that is why a
                    // folder's emoji stayed the font's grey one until you touched
                    // something.
                    ctx.request_repaint_after(std::time::Duration::from_millis(150));
                    return None;
                }
            }
        };
        let decoded = match decode_image(&bytes) {
            Some(d) => d,
            None => {
                // A corrupt or partial file must not be decoded again every
                // frame: drop it so it is fetched afresh next time.
                self.images.unsee(url);
                return None;
            }
        };
        // The bytes are no longer needed: the texture(s) are the only copy that counts.
        self.images.forget(url);
        match decoded {
            Decoded::Static(ci) => {
                let tex = ctx.load_texture(url, ci, egui::TextureOptions::LINEAR);
                self.textures.insert(url.to_string(), tex.clone());
                self.texture_order.push_back(url.to_string());
                Some(tex)
            }
            Decoded::Animated(frames) => {
                let mut handles = Vec::with_capacity(frames.len());
                let mut durations = Vec::with_capacity(frames.len());
                let mut total = std::time::Duration::ZERO;
                let mut bytes = 0usize;
                for (i, (ci, d)) in frames.into_iter().enumerate() {
                    bytes += ci.width() * ci.height() * 4;
                    handles.push(ctx.load_texture(
                        format!("{url}#{i}"),
                        ci,
                        egui::TextureOptions::LINEAR,
                    ));
                    durations.push(d);
                    total += d;
                }
                let first = handles[0].clone();
                self.anim.insert(
                    url.to_string(),
                    AnimatedTexture {
                        frames: handles,
                        durations,
                        total,
                        bytes,
                        start: std::time::Instant::now(),
                        // Seen this frame: it was just decoded because it is being
                        // drawn now, so stamp the current frame.
                        last_seen: self.frame_seq,
                    },
                );
                self.anim_order.push_back(url.to_string());
                // Start the clock: without a repaint the next frame would wait
                // for some unrelated event to move the interface.
                ctx.request_repaint();
                Some(first)
            }
        }
    }

    /// The frame of an animated GIF due now, and schedule the repaint for the
    /// next one. Derived from elapsed-time-modulo-total, so a tab left open does
    /// not drift. Refreshes the LRU order so a GIF in view is not evicted first.
    fn anim_frame(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        let seq = self.frame_seq;
        let frame = {
            let a = self.anim.get_mut(url)?;
            if a.frames.is_empty() {
                return None;
            }
            // Mark it drawn this frame so `evict_anim` spares it.
            a.last_seen = seq;
            if a.total.is_zero() {
                a.frames[0].clone()
            } else {
                let mut t = a.start.elapsed().as_nanos() % a.total.as_nanos();
                let mut idx = a.frames.len() - 1;
                for (i, d) in a.durations.iter().enumerate() {
                    let dn = d.as_nanos();
                    if t < dn {
                        idx = i;
                        let left = (dn - t).min(u64::MAX as u128) as u64;
                        ctx.request_repaint_after(std::time::Duration::from_nanos(left));
                        break;
                    }
                    t -= dn;
                }
                a.frames[idx].clone()
            }
        };
        if let Some(pos) = self.anim_order.iter().position(|u| u == url) {
            if let Some(u) = self.anim_order.remove(pos) {
                self.anim_order.push_back(u);
            }
        }
        Some(frame)
    }

    /// Evicts the oldest animated GIFs until the animated set is back under
    /// `ANIM_TOTAL_BYTES_MAX`. Called at the **end** of a frame, after
    /// [`Self::anim_frame`] has moved every GIF shown this frame to the back of
    /// `anim_order`: evicting at the *start* (like `evict_textures`) could drop a
    /// GIF just scrolled into view — its LRU slot is only refreshed during the
    /// draw — and force an immediate, expensive multi-frame re-decode.
    ///
    /// A GIF drawn *this* frame (`last_seen == frame_seq`) is never evicted, even
    /// if that leaves the set over budget: dropping an on-screen GIF only re-
    /// decodes it (120 frames) on the very next frame, a thrash loop. This only
    /// bites when the currently-visible GIFs alone exceed the budget — a rare
    /// pathology the byte caps otherwise keep from arising.
    fn evict_anim(&mut self) {
        // The common case is no animated GIFs at all: skip the sum entirely.
        if self.anim.is_empty() {
            return;
        }
        let seq = self.frame_seq;
        let mut total: usize = self.anim.values().map(|a| a.bytes).sum();
        // Oldest first, but skip (leave in place) any GIF drawn this frame. Once
        // every front entry is on-screen there is nothing left to drop, so stop.
        let mut skipped = 0usize;
        while total > ANIM_TOTAL_BYTES_MAX && skipped < self.anim_order.len() {
            let Some(url) = self.anim_order.pop_front() else {
                break;
            };
            match self.anim.get(&url) {
                Some(a) if a.last_seen == seq => {
                    // On screen now: keep it, move it to the back, and count it
                    // as skipped so we do not loop on it forever.
                    self.anim_order.push_back(url);
                    skipped += 1;
                }
                Some(a) => {
                    total -= a.bytes;
                    self.anim.remove(&url);
                    // Let it reload from the disk cache if it comes back on
                    // screen, exactly like `evict_textures`.
                    self.images.unsee(&url);
                }
                // In `anim_order` but not in `anim` (should not happen): drop the
                // stale order entry.
                None => {}
            }
        }
    }

    /// Search results: the conversations that already exist first, then
    /// the colleagues you have not spoken to yet.
    /// Opens the first match of the sidebar filter — the quick switcher's "go"
    /// on Enter.
    ///
    /// The first channel whose name contains the query, otherwise the first
    /// person, sorted by name as the list shows them. Returns whether anything
    /// was opened (so the caller knows if the filter can be cleared).
    fn open_first_search_match(&mut self) -> bool {
        let q = self.search.to_lowercase();
        if let Some(i) = self
            .snap
            .channels
            .iter()
            .position(|c| c.name.to_lowercase().contains(&q))
        {
            self.close_views();
            self.app.select_channel(i);
            return true;
        }
        let mut people: Vec<(&String, &String)> = self
            .snap
            .user_names
            .iter()
            .filter(|(_, n)| n.to_lowercase().contains(&q))
            .collect();
        people.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        if let Some((id, name)) = people.first() {
            let (id, name) = ((*id).clone(), (*name).clone());
            self.close_views();
            self.app.open_dm(id, name);
            return true;
        }
        false
    }

    fn search_results(&mut self, ui: &mut egui::Ui, q: &str) {
        let channels = self.snap.channels.clone();
        let mut shown_names = std::collections::HashSet::new();

        let hits: Vec<(usize, ChannelRow)> = channels
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.to_lowercase().contains(q))
            .map(|(i, c)| (i, c.clone()))
            .take(40)
            .collect();
        for (i, c) in &hits {
            shown_names.insert(c.name.to_lowercase());
            self.channel_row(ui, *i, c);
        }

        // Someone with no DM open yet does not appear among the
        // conversations: without this section they would be unreachable.
        let mut people: Vec<(String, String)> = self
            .snap
            .user_names
            .iter()
            .filter(|(_, n)| n.to_lowercase().contains(q))
            .filter(|(_, n)| !shown_names.contains(&n.to_lowercase()))
            .map(|(id, n)| (id.clone(), n.clone()))
            .collect();
        people.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        people.truncate(20);

        if !people.is_empty() {
            ui.add_space(6.0);
            ui.weak(t!("gui.write_to_short").to_string());
            for (id, name) in people {
                if selectable(ui, false, format!("@  {name}")).clicked() {
                    self.close_views();
                    self.app.open_dm(id, name.clone());
                    self.search.clear();
                }
            }
        }

        if hits.is_empty() && self.snap.user_names.is_empty() {
            ui.weak(t!("gui.loading_users").to_string());
        }
    }

    /// Notes an unread row and where it is.
    fn note_unread(&mut self, rect: egui::Rect, unread: bool, mention: bool) {
        if unread || mention {
            self.unread_marks.push(UnreadMark {
                top: rect.top(),
                bottom: rect.bottom(),
                mention,
            });
        }
    }

    /// Counts the unreads left outside the visible part.
    ///
    /// Every row is drawn, including those off screen, so each one's position is
    /// known: it is enough to compare it with the window.
    fn recount_unread(
        &mut self,
        ctx: &egui::Context,
        out: &egui::scroll_area::ScrollAreaOutput<()>,
    ) {
        let (above, below) = split_unread(
            &self.unread_marks,
            out.inner_rect.top(),
            out.inner_rect.bottom(),
            out.state.offset.y,
        );

        // The strips use the previous frame's values: if they change,
        // without a repaint they would stay wrong until you touch something.
        if above != self.unread_above || below != self.unread_below {
            self.unread_above = above;
            self.unread_below = below;
            ctx.request_repaint();
        }
    }

    /// The height reserved for a strip, text plus margins.
    const STRIP_H: f32 = 22.0;

    /// The strip that flags the unreads out of sight.
    ///
    /// It appears only when there are any: taking up space to say "nothing new"
    /// on an already long bar helps nobody.
    fn unread_strip(&mut self, ui: &mut egui::Ui, top: bool) {
        let c = if top {
            self.unread_above
        } else {
            self.unread_below
        };
        if !c.any() {
            return;
        }
        // Red is reserved for mentions: they need an answer, the rest can
        // wait and must not shout in the same way.
        let color = if c.mentions > 0 {
            pal().danger
        } else {
            pal().link
        };
        let dir = if top { Dir::Up } else { Dir::Down };
        let text = egui::RichText::new(c.label()).small().strong().color(color);
        let r = ui
            .horizontal(|ui| {
                let a = triangle(ui, dir, color);
                a | ui.add(
                    egui::Label::new(text)
                        .sense(egui::Sense::click())
                        .selectable(false),
                )
            })
            .inner;
        if r.clicked() {
            // It brings the list to the nearest one without opening it:
            // opening would mark it read, and someone who is only checking did
            // not ask for that.
            self.scroll_to = Some((c.nearest - 8.0).max(0.0));
        }
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    fn favorites_section(&mut self, ui: &mut egui::Ui) {
        if self.snap.favorites.is_empty() {
            return;
        }
        let rows: Vec<(usize, ChannelRow)> = self
            .snap
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| self.snap.favorites.contains(&c.id))
            .map(|(i, c)| (i, c.clone()))
            .collect();
        if rows.is_empty() {
            return;
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(STAR).strong().color(pal().favorite));
            ui.label(egui::RichText::new(t!("gui.favorites").to_string()).strong());
        });
        for (i, c) in &rows {
            let r = ui
                .horizontal(|ui| {
                    ui.add_space(10.0);
                    self.channel_row(ui, *i, c);
                })
                .response
                .rect;
            self.note_unread(r, c.unread, c.mentions > 0);
        }
        ui.add_space(4.0);
    }

    /// The virtual "Threads" entry at the top of the sidebar: like a channel
    /// that does not exist, it opens a view of the threads the user is in.
    fn threads_entry(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let r = selectable(
                ui,
                self.threads_open,
                egui::RichText::new(t!("gui.threads").to_string()).strong(),
            );
            if self.snap.threads_unread > 0 {
                pill(
                    ui,
                    &self.snap.threads_unread.to_string(),
                    pal().unread_badge_fg,
                    pal().unread_badge_bg,
                );
            }
            if r.clicked() {
                self.toggle_center_view(CenterView::Threads);
            }
        });
    }

    /// The "Mentions" entry, next to "Threads".
    fn mentions_entry(&mut self, ui: &mut egui::Ui) {
        if selectable(
            ui,
            self.mentions_open,
            egui::RichText::new(t!("gui.mentions").to_string()).strong(),
        )
        .clicked()
        {
            self.toggle_center_view(CenterView::Mentions);
        }
        ui.separator();
    }

    /// The "Scheduled" entry: queued messages (client-side). Shown only when
    /// there is at least one.
    fn scheduled_entry(&mut self, ui: &mut egui::Ui) {
        if self.snap.scheduled.is_empty() {
            return;
        }
        ui.horizontal(|ui| {
            let r = selectable(
                ui,
                self.scheduled_open,
                egui::RichText::new(t!("gui.scheduled").to_string()).strong(),
            );
            pill(
                ui,
                &self.snap.scheduled.len().to_string(),
                pal().unread_badge_fg,
                pal().unread_badge_bg,
            );
            if r.clicked() {
                self.toggle_center_view(CenterView::Scheduled);
            }
        });
    }

    /// Shortcuts to the conversations with unread activity, at the top of the
    /// sidebar. They are a copy: the conversation also stays where the user put
    /// it, and a shortcut disappears once it has been read.
    fn notif_shortcuts(&mut self, ui: &mut egui::Ui) {
        let mut rows: Vec<(usize, ChannelRow)> = self
            .snap
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| c.unread || c.mentions > 0)
            .map(|(i, c)| (i, c.clone()))
            .collect();
        if rows.is_empty() {
            return;
        }
        // Mentions first, then by name: the loudest conversations are on top.
        rows.sort_by(|(_, a), (_, b)| {
            b.mentions
                .cmp(&a.mentions)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!("gui.notif_shortcuts").to_string()).strong())
                .on_hover_text(t!("gui.notif_shortcuts_tip").to_string());
            ui.label(
                egui::RichText::new(t!("gui.notif_shortcuts_hint").to_string())
                    .small()
                    .weak(),
            );
        });
        for (i, c) in &rows {
            let r = ui
                .horizontal(|ui| {
                    ui.add_space(10.0);
                    self.channel_row(ui, *i, c);
                })
                .response
                .rect;
            self.note_unread(r, c.unread, c.mentions > 0);
        }
        ui.add_space(4.0);
        ui.separator();
    }

    fn agents_section(&mut self, ui: &mut egui::Ui) {
        let rows: Vec<(usize, ChannelRow)> = self
            .snap
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.is_dm
                    && c.user
                        .as_deref()
                        .is_some_and(|u| self.snap.bot_users.contains(u))
            })
            .map(|(i, c)| (i, c.clone()))
            .collect();
        if rows.is_empty() {
            return;
        }
        ui.horizontal(|ui| {
            self.emoji(ui, "robot_face");
            ui.label(egui::RichText::new(t!("gui.agents_apps").to_string()).strong());
        });
        for (i, c) in &rows {
            let r = ui
                .horizontal(|ui| {
                    ui.add_space(10.0);
                    self.channel_row(ui, *i, c);
                })
                .response
                .rect;
            self.note_unread(r, c.unread, c.mentions > 0);
        }
        ui.add_space(4.0);
    }

    fn channel_row(&mut self, ui: &mut egui::Ui, i: usize, c: &ChannelRow) {
        let favorited = self.snap.favorites.contains(&c.id);
        let row = ui.horizontal(|ui| {
            // DMs are not channels: giving them the hash makes them look like
            // ones. For a person the symbol's place is taken by the presence, which
            // says far more: whether writing to them now makes sense.
            match c.user.as_deref().filter(|_| c.is_dm) {
                Some(u) => draw_presence(ui, presence_of(&self.snap, u)),
                None if c.is_dm => draw_presence(ui, Presence::Unknown),
                None => {
                    ui.label(egui::RichText::new("#").color(pal().text_muted));
                }
            }
            // The text has to be truncated here: `SelectableLabel` has no
            // `truncate`, and a long name would widen the row (and the panel with it).
            // The limit is dynamic: a fixed one was still too wide for a row with
            // an avatar and a badge, and a row wider than the panel widened the
            // side panel's frame, pushing the central panel right and leaving an
            // unpainted column between them.
            let max_chars = (((ui.available_width() - 70.0) / 8.6).floor() as usize).clamp(6, 24);
            let truncated = c.name.chars().count() > max_chars;
            let name = if truncated {
                let short: String = c.name.chars().take(max_chars.saturating_sub(1)).collect();
                format!("{short}\u{2026}")
            } else {
                c.name.clone()
            };
            let text = egui::RichText::new(name);
            // Read conversations are muted; an unread is primary and bold (a
            // real bold face, not just a brighter colour), so the eye lands on
            // what is new without reading every badge. The open one is primary
            // too, but not bold unless it also has something new.
            let text = if c.unread {
                text.strong()
                    .family(egui::FontFamily::Name("bold".into()))
                    .color(pal().text_primary)
            } else if i == self.snap.selected {
                text.color(pal().text_primary)
            } else {
                text.color(pal().text_secondary)
            };
            let sel = selectable(ui, i == self.snap.selected, text);
            if truncated {
                // The name is cut to keep the row inside the panel (see above), so
                // reveal the full one on hover — using `c.name`, not the truncated
                // string. Only when it was actually cut: a redundant tooltip on a
                // short name is just noise.
                sel.clone().on_hover_text(c.name.as_str());
            }
            if sel.clicked() {
                self.close_views();
                self.app.select_channel(i);
            }
            if favorited {
                ui.label(egui::RichText::new(STAR).small().color(pal().favorite))
                    .on_hover_text(t!("gui.in_favorites_hover").to_string());
            }
            // The badge sits right after the name: aligning it to the right
            // with `right_to_left` made the row claim all the available width,
            // and the sidebar grew on every frame.
            // The status emoji is often worth more than the name: it says they are
            // in a meeting, on holiday, travelling.
            if let Some(name) = c
                .user
                .as_deref()
                .and_then(|u| self.snap.user_status.get(u))
                .map(|st| st.emoji.trim_matches(':').to_string())
                .filter(|n| !n.is_empty())
            {
                let text = c
                    .user
                    .as_deref()
                    .and_then(|u| self.snap.user_status.get(u))
                    .map(|st| st.text.clone())
                    .unwrap_or_default();
                let r = ui.scope(|ui| self.emoji(ui, &name)).response;
                if !text.is_empty() {
                    r.on_hover_text(text);
                }
            }
            // A call running in here: it is the only way to notice without
            // the official client open alongside.
            if let Some(h) = self.snap.huddles.get(&c.id) {
                let quante = if h.people() == 1 {
                    t!("gui.people_one").to_string()
                } else {
                    t!("gui.people_many", n = h.people()).to_string()
                };
                let testo = if h.ringing {
                    t!("gui.being_called").to_string()
                } else {
                    t!("gui.huddle_ongoing", people = quante).to_string()
                };
                draw_huddle(ui, h.ringing).on_hover_text(testo);
            }
            if c.mentions > 0 {
                pill(
                    ui,
                    &c.mentions.to_string(),
                    pal().on_danger,
                    pal().danger_fill,
                );
            }
        });
        let id = c.id.clone();
        row.response.context_menu(|ui| {
            let label = if favorited {
                t!("gui.remove_favorite").to_string()
            } else {
                t!("gui.add_favorite").to_string()
            };
            if ui.button(label).clicked() {
                self.app.toggle_favorite(id.clone());
                ui.close_menu();
            }
        });
    }

    /// Side panel with the open thread's replies.
    fn thread_panel(&mut self, ctx: &egui::Context) {
        let out = egui::SidePanel::right("thread")
            .default_width(380.0)
            .max_width(680.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal().bg_panel))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(t!("gui.thread").to_string());
                    if let Some(id) = self.snap.thread_channel.clone() {
                        ui.label(egui::RichText::new(self.conversation_name(&id)).weak());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if close_button(ui)
                            .on_hover_text(t!("gui.close_thread_hover").to_string())
                            .clicked()
                        {
                            self.thread_jump = None;
                            self.highlight_ts = None;
                            self.highlight_offset = None;
                            self.app.close_thread();
                        }
                    });
                });
                ui.separator();

                let msgs = std::mem::take(&mut self.snap.thread_messages);
                egui::TopBottomPanel::bottom("thread_input").show_inside(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        // See the main composer: the height comes from the **wrapped**
                        // text, measured with the box width estimated before the
                        // buttons (which anchor to it).
                        let est = (ui.available_width()
                            - 2.0 * COMPOSER_BTN_W
                            - 2.0 * ui.spacing().item_spacing.x
                            - 8.0)
                            .max(60.0);
                        let (h, bounded) = composer_box(ui, &self.thread_input, est);
                        // The thread wants its own button too: without it, an
                        // attachment could not end up in there by any means.
                        self.handle_paste(ui, self.snap.thread_parent.clone());
                        let attach_tex = self.icon_texture(ui.ctx(), "paperclip");
                        let attach_icon = egui::Image::new(&attach_tex)
                            .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                            .tint(ui.visuals().text_color());
                        if composer_button(ui, h, |ui| {
                            ui.add_sized(
                                [COMPOSER_BTN_W, COMPOSER_BTN_W],
                                egui::Button::image(attach_icon),
                            )
                            .on_hover_text(t!("gui.attach_to_thread_hover").to_string())
                            .clicked()
                        }) {
                            let parent = self.snap.thread_parent.clone();
                            self.pick_file(parent);
                        }
                        let w = (ui.available_width()
                            - COMPOSER_BTN_W
                            - ui.spacing().item_spacing.x
                            - 8.0)
                            .max(60.0);
                        // Same mention (`@`) and channel (`#`) completion as the
                        // main composer: the keys are taken before the TextEdit
                        // so Enter/Tab pick an entry instead of sending.
                        let action = self.completion_keys(ui);
                        // Bound the box past the row cap (see the main composer):
                        // the thread panel is narrow, so a long reply wraps into
                        // more lines and used to make the row taller than the panel,
                        // pushing Send off it. A normal reply is unchanged.
                        let lock = !self.complete_items.is_empty();
                        let hint = t!("gui.reply_in_thread_hint").to_string();
                        let mut text_edit = |ui: &mut egui::Ui| {
                            egui::TextEdit::multiline(&mut self.thread_input)
                                // Fixed id: see the main composer. The thread
                                // composer shares the frame with it, so the two
                                // must not collide.
                                .id(egui::Id::new("thread_composer_input"))
                                .desired_rows(1)
                                .desired_width(w)
                                .lock_focus(lock)
                                .min_size(egui::vec2(w, h))
                                .hint_text(hint.as_str())
                                .show(ui)
                        };
                        let (out, viewport) = if bounded {
                            let area = egui::ScrollArea::vertical()
                                .id_salt("thread_composer_scroll")
                                .max_height(h)
                                .show(ui, |ui| text_edit(ui));
                            (area.inner, area.inner_rect)
                        } else {
                            let out = text_edit(ui);
                            let rect = out.response.rect;
                            (out, rect)
                        };
                        let resp = out.response;
                        self.composer_rect = viewport;
                        self.composer_id = Some(resp.id);
                        let cursor = out
                            .cursor_range
                            .map(|c| c.primary.ccursor.index)
                            .unwrap_or_else(|| self.thread_input.chars().count());
                        self.complete_in_thread = true;
                        self.completion_update(cursor, resp.has_focus());
                        let consumed = self.completion_draw(ui, action);
                        self.complete_in_thread = false;
                        let send_tex = self.icon_texture(ui.ctx(), "send");
                        let send_icon = egui::Image::new(&send_tex)
                            .fit_to_exact_size(egui::vec2(COMPOSER_ICON, COMPOSER_ICON))
                            .tint(pal().text_on_accent);
                        let send_clicked = composer_button(ui, h, |ui| {
                            ui.add_sized(
                                [COMPOSER_BTN_W, COMPOSER_BTN_W],
                                egui::Button::image(send_icon)
                                    .fill(pal().accent)
                                    .rounding(8.0),
                            )
                            .on_hover_text(t!("gui.send_hover").to_string())
                            .clicked()
                        });
                        if (send_clicked || (!consumed && sent(&resp, ui)))
                            && !self.thread_input.trim().is_empty()
                        {
                            self.reply();
                            resp.request_focus();
                        }
                    });
                    ui.add_space(4.0);
                });

                let thread_scroll =
                    egui::ScrollArea::vertical()
                        .id_salt("thread_scroll")
                        .show(ui, |ui| {
                            if msgs.is_empty() {
                                ui.weak(t!("gui.loading").to_string());
                            }
                            // A search hit that is a reply has its own one-shot
                            // target; `jump_ts` stays for the other paths.
                            let jump = self.thread_jump.clone();
                            let highlight = self.highlight_ts.clone();
                            let deco_channel = self.snap.thread_channel.clone().unwrap_or_default();
                            let mut jumped = false;
                            let panel = ui.max_rect();
                            let mut prev: Option<&MessageEntry> = None;
                            for m in &msgs {
                                let continuation = is_continuation(prev, m);
                                let new_day = match prev
                                    .and_then(|p| ts_secs(&p.ts))
                                    .zip(ts_secs(&m.ts))
                                {
                                    Some((a, b)) => !same_day_in(&chrono::Local, a, b),
                                    None => true,
                                };
                                if new_day {
                                    day_divider(ui, &m.ts);
                                }
                                let rect = ui
                                    .scope(|ui| {
                                        self.message(ui, m, &deco_channel, continuation, panel)
                                    })
                                    .response
                                    .rect;
                                if highlight.as_deref() == Some(m.ts.as_str()) {
                                    ui.painter().rect_filled(
                                        rect.expand(2.0),
                                        3.0,
                                        pal().accent_tint(0x5A),
                                    );
                                }
                                if jump.as_deref() == Some(m.ts.as_str()) {
                                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                    jumped = true;
                                }
                                prev = Some(m);
                            }
                            // Only the thread's own target is cleared here. The
                            // conversation list's jump is separate: clearing it
                            // here would drop the jump that is still bringing the
                            // root into view, and the list would wander off.
                            if jumped {
                                self.thread_jump = None;
                            } else if jump.is_some() && !msgs.is_empty() {
                                // Threads are not paged: once the replies are all
                                // loaded and the target is not among them there is
                                // nothing left to find, so the jump is dropped
                                // instead of leaving the panel waiting.
                                self.thread_jump = None;
                            }
                        });
                // Same "you are here" rule as the conversation list: drop the
                // reply's highlight once the panel is scrolled away from it.
                if self.highlight_ts.is_some() && !msgs.is_empty() {
                    if self.highlight_channel.as_deref() != self.current_channel().as_deref() {
                        self.highlight_ts = None;
                        self.highlight_offset = None;
                    } else if let Some(prev) = self.highlight_offset {
                        if (thread_scroll.state.offset.y - prev).abs() > 24.0 {
                            self.highlight_ts = None;
                            self.highlight_offset = None;
                        }
                    } else {
                        self.highlight_offset = Some(thread_scroll.state.offset.y);
                    }
                }
                self.snap.thread_messages = msgs;
            });
        self.thread_rect = out.response.rect;
    }

    /// Right panel with a channel's topic, purpose and member list.
    fn channel_info_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("channel_info")
            .default_width(320.0)
            .resizable(true)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal().bg_panel))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(t!("gui.channel_info_title").to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if close_button(ui)
                            .on_hover_text(t!("gui.close_esc_hover").to_string())
                            .clicked()
                        {
                            self.app.close_channel_info();
                        }
                    });
                });
                ui.separator();

                let Some(info) = self.snap.channel_info.clone() else {
                    ui.weak(t!("gui.loading").to_string());
                    return;
                };

                let is_direct =
                    info.is_im || info.is_mpim || channel_kind(&info.id) == ChannelKind::Direct;
                let title = if is_direct {
                    info.user
                        .as_deref()
                        .and_then(|u| self.snap.user_names.get(u))
                        .cloned()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| self.conversation_name(&info.id))
                } else if !info.name.is_empty() && info.name != info.id {
                    format!("#{}", info.name)
                } else {
                    self.conversation_name(&info.id)
                };
                ui.label(egui::RichText::new(title).heading());
                ui.add_space(4.0);

                // The topic is editable below (with the name), so it is not
                // repeated here read-only; only the purpose stays static.
                info_block(
                    ui,
                    t!("gui.purpose").as_ref(),
                    info.purpose.value.as_deref(),
                );

                // Management, for channels only: a DM's name is the person, not
                // something to rename, and you do not leave your own DMs.
                if !is_direct {
                    // The buffers belong to one channel: reseed them when the
                    // panel switches.
                    if self.manage.as_ref().map(|m| m.id.as_str()) != Some(info.id.as_str()) {
                        self.manage = Some(Manage {
                            id: info.id.clone(),
                            name: info.name.clone(),
                            topic: info.topic.value.clone().unwrap_or_default(),
                        });
                    }
                    let mut do_rename = None;
                    let mut do_topic = None;
                    let mut do_add = false;
                    let mut do_leave = false;
                    if let Some(m) = self.manage.as_mut() {
                        ui.add_space(6.0);
                        // Each field is a small form: the label on its own
                        // line, the input full width below it, and a save
                        // button that stays put but enables only once the value
                        // differs from the current one — so the input has room
                        // to read the whole name/topic.
                        let field_w = ui.available_width();
                        ui.label(egui::RichText::new(t!("gui.name").to_string()).strong());
                        ui.add(egui::TextEdit::singleline(&mut m.name).desired_width(field_w));
                        let name_changed =
                            m.name.trim() != info.name.trim() && !m.name.trim().is_empty();
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(
                                            name_changed,
                                            egui::Button::new(t!("gui.save").to_string()),
                                        )
                                        .clicked()
                                    {
                                        do_rename =
                                            Some((info.id.clone(), m.name.trim().to_string()));
                                    }
                                },
                            );
                        });
                        ui.add_space(8.0);
                        let topic_now = info.topic.value.clone().unwrap_or_default();
                        ui.label(egui::RichText::new(t!("gui.topic").to_string()).strong());
                        ui.add(egui::TextEdit::singleline(&mut m.topic).desired_width(field_w));
                        let topic_changed = m.topic.trim() != topic_now.trim();
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(
                                            topic_changed,
                                            egui::Button::new(t!("gui.save").to_string()),
                                        )
                                        .clicked()
                                    {
                                        do_topic =
                                            Some((info.id.clone(), m.topic.trim().to_string()));
                                    }
                                },
                            );
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(t!("gui.add_people").to_string()).clicked() {
                                do_add = true;
                            }
                            if ui.button(t!("gui.leave_channel").to_string()).clicked() {
                                do_leave = true;
                            }
                        });
                    }
                    if let Some((id, name)) = do_rename {
                        self.app.rename_channel(id, name);
                    }
                    if let Some((id, topic)) = do_topic {
                        self.app.set_channel_topic(id, topic);
                    }
                    if do_add {
                        self.people_pick = Some(PeoplePick {
                            invite_to: Some(info.id.clone()),
                            search: String::new(),
                            selected: std::collections::HashSet::new(),
                        });
                    }
                    if do_leave {
                        self.app.leave_channel(info.id.clone());
                    }
                }

                ui.separator();

                let loaded = self.snap.channel_members.len();
                let total = if info.num_members > 0 {
                    info.num_members as usize
                } else {
                    loaded
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t!("gui.members_count", n = total).to_string())
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.snap.channel_filter)
                                .hint_text(t!("gui.filter_hint").to_string())
                                .desired_width(130.0),
                        );
                    });
                });
                if info.num_members as usize > loaded && loaded > 0 {
                    ui.weak(
                        t!(
                            "gui.members_first",
                            loaded = loaded,
                            total = info.num_members
                        )
                        .to_string(),
                    );
                }
                ui.add_space(4.0);

                let filter = self.snap.channel_filter.trim().to_lowercase();
                let me = self.snap.me.clone();
                let names = self.snap.user_names.clone();
                let avatars = self.snap.avatars.clone();
                let mut members = self.snap.channel_members.clone();
                members.sort_by_key(|id| {
                    names
                        .get(id)
                        .map(|n| n.to_lowercase())
                        .unwrap_or_else(|| id.to_lowercase())
                });

                let mut do_kick: Option<(String, String)> = None;
                egui::ScrollArea::vertical()
                    .id_salt("channel_info_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut shown = 0;
                        for id in &members {
                            let name = names.get(id).cloned().unwrap_or_else(|| id.clone());
                            if !filter.is_empty()
                                && !name.to_lowercase().contains(&filter)
                                && !id.to_lowercase().contains(&filter)
                            {
                                continue;
                            }
                            shown += 1;
                            ui.horizontal(|ui| {
                                let url = avatars.get(id).cloned().unwrap_or_default();
                                match self.texture(ui.ctx(), &url) {
                                    Some(tex) => {
                                        ui.add(
                                            egui::Image::new(&tex)
                                                .fit_to_exact_size(egui::vec2(24.0, 24.0)),
                                        );
                                    }
                                    None => {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(24.0, 24.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            rect,
                                            4.0,
                                            ui.visuals().widgets.inactive.bg_fill,
                                        );
                                    }
                                }
                                let mut label = name.clone();
                                if *id == me {
                                    label.push_str(t!("gui.you_suffix").as_ref());
                                }
                                ui.label(label);
                                // Remove the member: never yourself, and not
                                // in a DM.
                                if !is_direct && *id != me {
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if close_button(ui)
                                                .on_hover_text(
                                                    t!("gui.remove_member_hover").to_string(),
                                                )
                                                .clicked()
                                            {
                                                do_kick = Some((id.clone(), name.clone()));
                                            }
                                        },
                                    );
                                }
                            });
                        }
                        if shown == 0 {
                            ui.weak(if filter.is_empty() {
                                t!("gui.no_members").to_string()
                            } else {
                                t!("gui.no_members_filter").to_string()
                            });
                        }
                    });
                if let Some((uid, uname)) = do_kick {
                    self.kick_pending = Some((info.id.clone(), uid, uname));
                }
            });
    }

    /// Opens the thread `m` is the root of (or belongs to).
    fn open_thread(&self, m: &MessageEntry) {
        let Some(channel) = self.current_channel() else {
            return;
        };
        // For a root message `thread_ts` coincides with `ts`; for a reply
        // it points at the root. In both cases it is the right key.
        let ts = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
        self.app.open_thread(channel, ts);
    }

    fn reply(&mut self) {
        let (Some(channel), Some(parent)) = (
            self.snap.thread_channel.clone(),
            self.snap.thread_parent.clone(),
        ) else {
            return;
        };
        let raw = std::mem::take(&mut self.thread_input)
            .trim_end()
            .to_string();
        let text = self.resolve_mentions(&raw);
        self.app.send_message(channel, text, Some(parent));
    }

    fn current_channel(&self) -> Option<String> {
        self.snap
            .channels
            .get(self.snap.selected)
            .map(|c| c.id.clone())
    }

    /// "Ana Rossi is typing…" for the open conversation, or nothing. Schedules
    /// a repaint for when the indicator goes stale, so the line clears itself.
    fn typing_label(&self, ctx: &egui::Context) -> Option<String> {
        let channel = self.current_channel()?;
        let users = self.snap.typing.get(&channel)?;
        let now = std::time::Instant::now();
        if let Some(at) = users.values().filter(|at| **at > now).min().copied() {
            ctx.request_repaint_after(at.saturating_duration_since(now));
        }
        let mut names: Vec<String> = users
            .iter()
            .filter(|(u, at)| **at > now && u.as_str() != self.snap.me)
            .map(|(u, _)| {
                self.snap
                    .user_names
                    .get(u)
                    .cloned()
                    .unwrap_or_else(|| u.clone())
            })
            .collect();
        names.sort();
        names.dedup();
        match names.len() {
            0 => None,
            1 => Some(t!("gui.typing_one", who = names[0]).to_string()),
            _ => Some(t!("gui.typing_many", who = names.join(", ")).to_string()),
        }
    }

    /// A human label for a conversation, by id, for headers and panels.
    fn conversation_name(&self, channel_id: &str) -> String {
        let channel = self.snap.channels.iter().find(|c| c.id == channel_id);
        conversation_label(channel, &self.snap.user_names, channel_id)
    }

    fn send(&mut self) {
        if let Some(id) = self.current_channel() {
            // The multiline `TextEdit` inserts the newline before the Enter
            // can be intercepted: it is stripped here, not in the core, because it
            // is an artefact of this interface.
            let raw = std::mem::take(&mut self.input).trim_end().to_string();
            let text = self.resolve_mentions(&raw);
            self.app.send_message(id, text, None);
        }
    }

    /// The incoming call bar.
    ///
    /// It sits above everything because it is the only thing with a deadline: the
    /// caller hangs up, and a system notification that goes by while you are
    /// looking elsewhere is not enough to avoid missing the call.
    fn ring_bar(&mut self, ctx: &egui::Context) {
        let Some((channel, caller)) = self
            .snap
            .huddles
            .iter()
            .find(|(_, h)| h.ringing)
            .map(|(id, h)| (id.clone(), h.caller.clone()))
        else {
            return;
        };
        let chi = caller
            .and_then(|u| self.snap.user_names.get(&u).cloned())
            .unwrap_or_else(|| t!("gui.someone").to_string());

        egui::TopBottomPanel::top("ring").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                draw_huddle(ui, true);
                ui.label(egui::RichText::new(t!("gui.is_calling", who = chi).to_string()).strong());
                if ui.button(t!("gui.answer").to_string()).clicked() {
                    self.app.start_call_in(Some(channel.clone()));
                }
                if ui.button(t!("gui.ignore").to_string()).clicked() {
                    self.app.dismiss_ring(channel.clone());
                }
            });
            ui.add_space(4.0);
        });
    }

    /// The full-screen login shown while there is no usable session.
    ///
    /// It is the only thing drawn when unauthenticated: no chat UI, no account
    /// menu. The button starts the browser flow, a spinner stands in while it
    /// runs, and the status line carries the outcome if the restart fails.
    fn login_screen(&mut self, ctx: &egui::Context) {
        let logging_in = self.logging_in.load(std::sync::atomic::Ordering::Relaxed);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() - 200.0).max(0.0) / 2.0);
                ui.label(egui::RichText::new("slaq").size(42.0).strong());
                ui.add_space(6.0);
                ui.label(egui::RichText::new(t!("gui.login_subtitle").to_string()).weak());
                ui.add_space(20.0);
                if logging_in {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(t!("gui.opening_browser").to_string());
                    });
                } else if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(t!("gui.login_with_browser").to_string())
                                .color(pal().text_on_accent)
                                .strong(),
                        )
                        .fill(pal().accent)
                        .rounding(8.0)
                        .min_size(egui::vec2(220.0, 34.0)),
                    )
                    .clicked()
                {
                    self.start_browser_login();
                }
                if !self.snap.status.is_empty() {
                    ui.add_space(12.0);
                    // Secondary: the button above is the only thing to do here.
                    ui.weak(&self.snap.status);
                }
            });
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }

    /// The "session expired" dialog: the chat stays visible and the user
    /// decides whether to sign in again. This is what stops a single failed
    /// request from yanking the whole client to the login screen.
    fn reauth_dialog(&mut self, ctx: &egui::Context) {
        let logging_in = self.logging_in.load(std::sync::atomic::Ordering::Relaxed);
        let mut retry = false;
        let mut later = false;
        let m = modal(ctx, "reauth").show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading(t!("gui.session_expired_title").to_string());
            ui.label(t!("gui.session_expired_body").to_string());
            if logging_in {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(t!("gui.opening_browser").to_string());
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .button(t!("gui.session_expired_retry").to_string())
                    .clicked()
                {
                    retry = true;
                }
                if ui
                    .button(t!("gui.session_expired_later").to_string())
                    .clicked()
                {
                    later = true;
                }
            });
        });
        if m.should_close() {
            later = true;
        }
        if retry && !logging_in {
            self.start_browser_login();
        } else if later {
            self.app.dismiss_session_expired();
        }
    }

    /// Runs the browser login in the background.
    ///
    /// The running `App` holds its credentials in immutable fields, so there is
    /// no hot swap: on success the process re-execs itself and the new session
    /// is loaded at startup. If the re-exec fails the user is told to restart
    /// by hand.
    fn start_browser_login(&mut self) {
        if self.logging_in.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.logging_in
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let state = self.app.state().clone();
        let ws = self.app.creds().workspace_url.clone();
        let logging_in = self.logging_in.clone();
        self.rt.spawn(async move {
            {
                let mut s = state.write().await;
                s.status = t!("gui.login_opening_browser").to_string();
                crate::app::wake(&s);
            }
            let ws_opt = if ws.is_empty() {
                None
            } else {
                Some(ws.as_str())
            };
            match crate::auth::login_via_browser(ws_opt).await {
                Ok(()) => {
                    let restart = std::env::current_exe().and_then(|exe| {
                        std::process::Command::new(&exe)
                            .args(std::env::args_os().skip(1))
                            .spawn()
                    });
                    match restart {
                        Ok(_) => std::process::exit(0),
                        Err(e) => {
                            let mut s = state.write().await;
                            s.status = t!("gui.login_done", err = e).to_string();
                            crate::app::wake(&s);
                            logging_in.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.status = t!("gui.login_failed", err = e).to_string();
                    crate::app::wake(&s);
                    logging_in.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }

    /// The top-bar control: our avatar, or our initials while it downloads,
    /// with a presence dot on the corner.
    fn profile_button(&mut self, ui: &mut egui::Ui) -> egui::Response {
        const SIZE: f32 = 28.0;
        let me = self.snap.me.clone();
        let avatar = self.snap.avatars.get(&me).cloned().unwrap_or_default();
        let button = match self.texture(ui.ctx(), &avatar) {
            Some(tex) => egui::Button::image(
                egui::Image::new(&tex)
                    .fit_to_exact_size(egui::vec2(22.0, 22.0))
                    .rounding(11.0),
            )
            .min_size(egui::vec2(SIZE, SIZE))
            .rounding(14.0),
            None => {
                let initials = self
                    .snap
                    .user_names
                    .get(&me)
                    .map(|n| initials_of(n))
                    .unwrap_or_else(|| "?".to_string());
                egui::Button::new(egui::RichText::new(initials).strong())
                    .min_size(egui::vec2(SIZE, SIZE))
                    .rounding(14.0)
            }
        };
        // A circular avatar: the button padding is dropped so the content
        // cannot stretch the button past its square — `min_size`/`add_sized`
        // only set a floor, and "DB" plus the default padding was wider than
        // `SIZE`, giving an oval.
        let response = ui
            .scope(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
                ui.add_sized(egui::vec2(SIZE, SIZE), button)
            })
            .inner
            .on_hover_text(t!("gui.profile_hover").to_string());

        // Our own presence is subscribed to as well now (`refresh_presence`), but an
        // unknown state still means active by default, never a dot-less button.
        let color = match presence_of(&self.snap, &me) {
            Presence::Away => pal().text_muted,
            Presence::Quiet => pal().presence_quiet,
            Presence::Active | Presence::Unknown => pal().success,
        };
        let c = response.rect.right_bottom() + egui::vec2(-4.0, -4.0);
        let painter = ui.painter();
        painter.circle_filled(c, 4.5, color);
        painter.circle_stroke(c, 4.5, egui::Stroke::new(1.5_f32, ui.visuals().window_fill));
        response
    }

    /// The bell with the unread badge.
    ///
    /// It is painted rather than written as a glyph: the fonts egui loads
    /// have no bell, and a missing one would show as an empty box.
    fn notification_bell(&mut self, ui: &mut egui::Ui) -> egui::Response {
        const SIZE: f32 = 28.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(SIZE, SIZE), egui::Sense::click());
        let col = if resp.hovered() {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().text_color()
        };
        let c = rect.center() + egui::vec2(0.0, 1.0);
        let p = ui.painter();
        p.circle_filled(c + egui::vec2(0.0, -3.0), 6.0, col);
        p.rect_filled(
            egui::Rect::from_min_max(c + egui::vec2(-6.0, -3.0), c + egui::vec2(6.0, 2.0)),
            0.0,
            col,
        );
        p.add(egui::Shape::convex_polygon(
            vec![
                c + egui::vec2(-6.0, 1.0),
                c + egui::vec2(6.0, 1.0),
                c + egui::vec2(8.0, 3.0),
                c + egui::vec2(-8.0, 3.0),
            ],
            col,
            egui::Stroke::NONE,
        ));
        p.circle_filled(c + egui::vec2(0.0, 3.5), 1.8, col);

        if self.snap.notif_unread > 0 {
            let label = if self.snap.notif_unread > 9 {
                "9+".to_string()
            } else {
                self.snap.notif_unread.to_string()
            };
            let bc = rect.right_top() + egui::vec2(-5.0, 5.0);
            let p = ui.painter();
            p.circle_filled(bc, 8.0, pal().danger_fill);
            p.text(
                bc,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(10.0),
                pal().on_danger,
            );
        }
        resp.on_hover_text(t!("gui.notifications").to_string())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// The gear that opens the settings window.
    ///
    /// Painted rather than a glyph: the fonts egui loads have no gear, and a
    /// missing one would show as an empty box — the same reason as the bell.
    fn settings_button(&mut self, ui: &mut egui::Ui) -> egui::Response {
        const SIZE: f32 = 28.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(SIZE, SIZE), egui::Sense::click());
        let col = if self.settings_open || resp.hovered() {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().text_color()
        };
        let c = rect.center();
        let p = ui.painter();
        // A ring, eight teeth around it and a dot in the middle.
        p.circle_stroke(c, 6.0, egui::Stroke::new(2.0, col));
        for i in 0..8 {
            let a = std::f32::consts::TAU * (i as f32) / 8.0;
            let (s, co) = a.sin_cos();
            let inner = c + egui::vec2(co, s) * 5.0;
            let outer = c + egui::vec2(co, s) * 9.0;
            p.line_segment([inner, outer], egui::Stroke::new(2.0, col));
        }
        p.circle_filled(c, 1.6, col);
        resp.on_hover_text(t!("gui.settings").to_string())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// The mods consent dialog: what each installed mod asks for, as checkboxes,
    /// with Apply/Later. Shown automatically when something is undecided, or
    /// from the settings menu to review/revoke.
    #[cfg(feature = "mods")]
    fn mods_consent_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.snap.mods_consent.clone() else {
            // Closed: the next opening starts with the editors folded.
            if !self.access_ui.open.is_empty() || !self.access_ui.text.is_empty() {
                self.access_ui = AccessUi::default();
            }
            return;
        };
        let mut apply = false;
        let mut later = false;
        // The list can be long (up to `MAX_MODS` packages, each with several
        // capabilities and, for `filesystem`, one row per declared slot). Bound it
        // so the Apply/Later row below it always fits: 420 on a normal
        // window-sized screen (the historical value), shrinking on a short one — a
        // list taller than the window pushed the buttons off it.
        let list_h = (ctx.screen_rect().height() - 400.0).clamp(140.0, 420.0);
        egui::Window::new(t!("gui.mods_title").to_string())
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                // A **solid** scroll bar, not the default floating one: the list
                // can hold many mods, and a fading/absent bar made a clipped list
                // look like a broken dialog with no way to scroll. The bar also
                // gives a drag affordance when the wheel does not reach it.
                ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
                ui.label(egui::RichText::new(t!("gui.mods_intro").to_string()).weak());
                let mut toggled: Vec<(String, crate::mods::capability::Capability)> = Vec::new();
                // `filesystem` slot picks/clears, persisted immediately
                // (independently of Apply), collected like `toggled` and handed to
                // the shared state once the widgets are drawn.
                let mut browse: Vec<(String, String, crate::mods::fs::FsAccess)> = Vec::new();
                let mut clear: Vec<(String, String)> = Vec::new();
                // Channel/host edits, applied to the shared dialog like `toggled`
                // (saved on Apply).
                let mut access_edits: Vec<(String, crate::app::AccessEdit)> = Vec::new();
                // Keep the Apply/Later row reachable: the list is bounded (see
                // `list_h`) and shows a solid scroll bar when it overflows.
                egui::ScrollArea::vertical()
                    .id_salt("mods_consent_list")
                    .max_height(list_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for e in &dialog.entries {
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!("{} ({})", e.name, e.id)).strong(),
                            );
                            // Capabilities under their consent heading; each stays
                            // its own toggle.
                            let mut heading = "";
                            for c in &e.caps {
                                if c.cap.group() != heading {
                                    heading = c.cap.group();
                                    ui.add_space(2.0);
                                    ui.label(
                                        egui::RichText::new(group_label(c.cap)).italics().weak(),
                                    );
                                }
                                // The scope and hosts are on their own rows
                                // below (editable), not in the checkbox label.
                                let label = c.cap.name().to_string();
                                let mut on = c.selected;
                                if ui.checkbox(&mut on, label).changed() {
                                    toggled.push((e.id.clone(), c.cap));
                                }
                                match c.cap {
                                    crate::mods::capability::Capability::MessagesRead => {
                                        access_channels_ui(
                                            ui,
                                            e,
                                            &self.snap.channels,
                                            &mut self.access_ui,
                                            &mut access_edits,
                                        );
                                    }
                                    crate::mods::capability::Capability::NetEgress => {
                                        access_hosts_ui(ui, e, &mut self.access_ui, &mut access_edits);
                                    }
                                    _ => {}
                                }
                                // A `filesystem` slot is mapped from here: one
                                // indented row per declared slot, showing the
                                // current mapping with a native folder picker.
                                if c.cap == crate::mods::capability::Capability::Filesystem {
                                    for slot in &e.fs_slots {
                                        let mapped = slot
                                            .mapped
                                            .as_deref()
                                            .map(crate::mods::sanitize)
                                            .unwrap_or_else(|| {
                                                t!("gui.mods_slot_unmapped").to_string()
                                            });
                                        ui.horizontal(|ui| {
                                            ui.label(format!(
                                                "    {} ({}) \u{2014} {}",
                                                crate::mods::sanitize(&slot.name),
                                                slot.access.as_str(),
                                                mapped
                                            ));
                                            if ui
                                                .button(t!("gui.mods_slot_browse").to_string())
                                                .clicked()
                                            {
                                                browse.push((
                                                    e.id.clone(),
                                                    slot.name.clone(),
                                                    slot.access,
                                                ));
                                            }
                                            if slot.mapped.is_some()
                                                && ui
                                                    .button(t!("gui.mods_slot_clear").to_string())
                                                    .clicked()
                                            {
                                                clear.push((e.id.clone(), slot.name.clone()));
                                            }
                                        });
                                    }
                                }
                            }
                            if e.read_and_network() {
                                ui.colored_label(
                                    pal().warning,
                                    t!("gui.mods_danger_read_network").to_string(),
                                );
                            }
                            if e.files_and_network() {
                                ui.colored_label(
                                    pal().warning,
                                    t!("gui.mods_danger_files_network").to_string(),
                                );
                            }
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t!("gui.mods_apply").to_string()).clicked() {
                        apply = true;
                    }
                    if ui.button(t!("gui.mods_later").to_string()).clicked() {
                        later = true;
                    }
                });
                // Apply the toggles after the loop (the dialog is a per-frame
                // clone; the live one is in the shared state), by identity.
                for (id, cap) in toggled {
                    App::toggle_mod_consent_cap(self.app.state(), id, cap);
                }
                for (id, edit) in access_edits {
                    App::edit_mod_consent_access(self.app.state(), id, edit);
                }
                // Browse opens the system folder picker (off the graphics thread,
                // like `pick_file`) and seeds it with the slot's current mapping;
                // the pick is then persisted by `map_mod_consent_slot`.
                for (id, slot, access) in browse {
                    let mapped_dir = dialog
                        .entries
                        .iter()
                        .find(|e| e.id == id)
                        .and_then(|e| e.fs_slots.iter().find(|s| s.name == slot))
                        .and_then(|s| s.mapped.clone());
                    let app = self.app.state().clone();
                    self.rt.spawn(async move {
                        let mut dlg = rfd::AsyncFileDialog::new();
                        if let Some(m) = mapped_dir {
                            dlg = dlg.set_directory(m);
                        }
                        if let Some(dir) = dlg.pick_folder().await {
                            App::map_mod_consent_slot(
                                &app,
                                id,
                                slot,
                                dir.path().to_path_buf(),
                                access,
                            );
                        }
                    });
                }
                for (id, slot) in clear {
                    App::unmap_mod_consent_slot(self.app.state(), id, slot);
                }
            });
        if apply {
            App::apply_mods_consent(self.app.state().clone());
        } else if later {
            App::dismiss_mods_consent(self.app.state().clone());
        }
    }

    /// Flushes queued mod-settings edits (and a pending Save) in order. If the
    /// state lock is busy, the queue is kept for the next frame — an edit is
    /// never dropped and never reordered.
    #[cfg(feature = "mods")]
    fn flush_settings(&mut self) {
        if self.settings_pending.is_empty() && !self.settings_save_pending {
            return;
        }
        if self.snap.mods_settings.is_none() {
            // The form closed (e.g. a reload cleared it): drop the queue.
            self.settings_pending.clear();
            self.settings_save_pending = false;
            return;
        }
        {
            let Ok(mut s) = self.app.state().try_write() else {
                return; // busy: retry next frame, preserving order
            };
            if let Some(d) = s.mods_settings.as_mut() {
                for edit in self.settings_pending.drain(..) {
                    apply_settings_edit(d, edit);
                }
            } else {
                self.settings_pending.clear();
            }
        }
        if self.settings_save_pending {
            App::apply_mods_settings(self.app.state().clone());
            self.settings_save_pending = false;
        }
    }

    /// The mod settings form: one tab per mod, a row per field. Edits are pushed
    /// to the shared state (the form is a per-frame clone) and persisted on Save.
    #[cfg(feature = "mods")]
    fn mods_settings_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.snap.mods_settings.clone() else {
            return;
        };
        let mut edits: Vec<SettingsEdit> = Vec::new();
        let mut apply = false;
        let mut close = false;
        egui::Window::new(t!("gui.mods_settings_title").to_string())
            .collapsible(false)
            .resizable(false)
            .default_width(480.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if dialog.panes.is_empty() {
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    for (i, p) in dialog.panes.iter().enumerate() {
                        if selectable(ui, i == dialog.tab, p.name.clone()).clicked()
                            && i != dialog.tab
                        {
                            edits.push(SettingsEdit::Tab(i));
                        }
                    }
                });
                ui.separator();
                let Some(pane) = dialog.panes.get(dialog.tab) else {
                    return;
                };
                egui::ScrollArea::vertical()
                    .max_height(400.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for f in &pane.fields {
                            ui.horizontal(|ui| {
                                let label = ui.add_sized(
                                    [150.0, 18.0],
                                    egui::Label::new(f.display_label().to_string()),
                                );
                                if let Some(help) = &f.help {
                                    label.on_hover_text(help.clone());
                                }
                                match f.kind {
                                    crate::mods::settings::Kind::Bool => {
                                        let mut on = f.text == "true";
                                        if ui.checkbox(&mut on, "").changed() {
                                            edits.push(SettingsEdit::Toggle(
                                                pane.id.clone(),
                                                f.key.clone(),
                                            ));
                                        }
                                    }
                                    crate::mods::settings::Kind::Enum => {
                                        // A combo box, not a row: the manifest allows
                                        // up to 64 options, which a horizontal row
                                        // would clip.
                                        let mut current = f.text.clone();
                                        let selected = if current.is_empty() {
                                            t!("gui.mods_settings_unset").to_string()
                                        } else {
                                            current.clone()
                                        };
                                        egui::ComboBox::from_id_salt(format!(
                                            "{}/{}",
                                            pane.id, f.key
                                        ))
                                        .selected_text(selected)
                                        .show_ui(
                                            ui,
                                            |ui| {
                                                for opt in &f.options {
                                                    ui.selectable_value(
                                                        &mut current,
                                                        opt.clone(),
                                                        opt.as_str(),
                                                    );
                                                }
                                            },
                                        );
                                        if current != f.text {
                                            edits.push(SettingsEdit::Text(
                                                pane.id.clone(),
                                                f.key.clone(),
                                                current,
                                            ));
                                        }
                                    }
                                    crate::mods::settings::Kind::Secret => {
                                        let mut text = f.text.clone();
                                        let hint = if f.has_value {
                                            t!("gui.mods_settings_set").to_string()
                                        } else {
                                            t!("gui.mods_settings_unset").to_string()
                                        };
                                        // The buffer holds only a replacement; the
                                        // stored secret is never loaded into it.
                                        let resp = ui.add(
                                            egui::TextEdit::singleline(&mut text)
                                                .password(true)
                                                .desired_width(180.0)
                                                .hint_text(hint),
                                        );
                                        if resp.changed() {
                                            edits.push(SettingsEdit::Text(
                                                pane.id.clone(),
                                                f.key.clone(),
                                                text,
                                            ));
                                        }
                                        if f.has_value
                                            && ui
                                                .button(t!("gui.mods_settings_clear").to_string())
                                                .clicked()
                                        {
                                            edits.push(SettingsEdit::Clear(
                                                pane.id.clone(),
                                                f.key.clone(),
                                            ));
                                        }
                                    }
                                    _ => {
                                        let mut text = f.text.clone();
                                        let resp = ui.add(
                                            egui::TextEdit::singleline(&mut text)
                                                .desired_width(220.0),
                                        );
                                        if resp.changed() {
                                            edits.push(SettingsEdit::Text(
                                                pane.id.clone(),
                                                f.key.clone(),
                                                text,
                                            ));
                                        }
                                    }
                                }
                            });
                        }
                    });
                if let Some(err) = &dialog.error {
                    ui.colored_label(pal().danger, err.clone());
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !dialog.busy,
                            egui::Button::new(t!("gui.mods_save").to_string()),
                        )
                        .clicked()
                    {
                        apply = true;
                    }
                    if ui.button(t!("gui.mods_close").to_string()).clicked() {
                        close = true;
                    }
                });
            });
        self.settings_pending.extend(edits);
        if apply {
            // Flush the queue first (next frame), then Save — so it cannot persist
            // a pre-edit value if the state lock was busy this frame.
            self.settings_save_pending = true;
        } else if close {
            App::dismiss_mods_settings(self.app.state().clone());
            self.settings_pending.clear();
        }
    }

    /// "Mod updated": a mod's package changed since the user saved some of its
    /// secrets. Keep re-binds them to this package, Forget deletes them, Not
    /// now leaves the new package without them for this session.
    #[cfg(feature = "mods")]
    fn mods_secrets_window(&mut self, ctx: &egui::Context) {
        let entries = self.snap.mods_secrets.clone();
        let mut keep: Option<String> = None;
        let mut forget: Option<String> = None;
        let mut later = false;
        egui::Window::new(t!("gui.mods_secrets_title").to_string())
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(t!("gui.mods_secrets_intro").to_string()).weak());
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for e in &entries {
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} ({})",
                                    e.name,
                                    crate::mods::sanitize(&e.id)
                                ))
                                .strong(),
                            );
                            for k in &e.keys {
                                ui.label(format!("  \u{2022} {}", crate::mods::sanitize(k)));
                            }
                            let reach = if e.egress.is_empty() {
                                t!("gui.mods_access_none").to_string()
                            } else {
                                hosts_text(&e.egress)
                            };
                            ui.label(
                                egui::RichText::new(t!("gui.mods_secrets_reach", hosts = reach).to_string())
                                    .weak(),
                            );
                            ui.horizontal(|ui| {
                                if ui.button(t!("gui.mods_secrets_keep").to_string()).clicked() {
                                    keep = Some(e.id.clone());
                                }
                                if ui.button(t!("gui.mods_secrets_forget").to_string()).clicked() {
                                    forget = Some(e.id.clone());
                                }
                            });
                        }
                    });
                ui.separator();
                if ui.button(t!("gui.mods_secrets_later").to_string()).clicked() {
                    later = true;
                }
            });
        if let Some(id) = keep {
            App::keep_mod_secrets(self.app.state().clone(), id);
        } else if let Some(id) = forget {
            App::forget_mod_secrets(self.app.state().clone(), id);
        } else if later {
            App::later_mod_secrets(self.app.state());
        }
    }

    /// "Running mods": each loaded mod's live state, with Stop, plus Reload.
    /// Polls the host's lock-free counters, so it stays live while open.
    #[cfg(feature = "mods")]
    fn mods_running_window(&mut self, ctx: &egui::Context) {
        use crate::mods::ModState;
        let mut open = self.running_open;
        let statuses = self
            .snap
            .mods_host
            .as_ref()
            .map(|h| h.statuses())
            .unwrap_or_default();
        let mut stop: Option<String> = None;
        let mut reload = false;
        egui::Window::new(t!("gui.mods_running_title").to_string())
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if statuses.is_empty() {
                    ui.label(egui::RichText::new(t!("gui.mods_running_empty").to_string()).weak());
                }
                for st in &statuses {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&st.name).strong());
                        let (dot, label, col) = match st.state {
                            ModState::Running => (
                                "●",
                                t!("gui.mods_running_state_running").to_string(),
                                pal().success,
                            ),
                            ModState::Stopped => (
                                "■",
                                t!("gui.mods_running_state_stopped").to_string(),
                                pal().text_muted,
                            ),
                        };
                        ui.label(egui::RichText::new(format!("{dot} {label}")).color(col));
                        if st.state == ModState::Running {
                            let activity = match st.busy_for {
                                Some(d) => t!(
                                    "gui.mods_running_busy",
                                    secs = format!("{:.1}", d.as_secs_f32())
                                )
                                .to_string(),
                                None => t!("gui.mods_running_idle").to_string(),
                            };
                            let col = if st.busy_for.is_some() {
                                pal().warning
                            } else {
                                pal().text_muted
                            };
                            ui.label(egui::RichText::new(activity).color(col));
                        }
                    });
                    ui.horizontal(|ui| {
                        let mut line = t!(
                            "gui.mods_running_counts",
                            calls = st.calls,
                            ticks = st.ticks
                        )
                        .to_string();
                        if let Some(every) = st.background {
                            line.push_str(" · ");
                            line.push_str(
                                &t!(
                                    "gui.mods_running_every",
                                    secs = format!("{}", every.as_secs_f32())
                                )
                                .to_string(),
                            );
                        }
                        ui.label(egui::RichText::new(line).weak().small());
                        if st.state == ModState::Running {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(t!("gui.mods_running_stop").to_string()).clicked() {
                                    stop = Some(st.id.clone());
                                }
                            });
                        }
                    });
                    ui.separator();
                }
                ui.label(egui::RichText::new(t!("gui.mods_running_hint").to_string()).weak().small());
                // One row, right-aligned (a bare right-to-left layout would claim
                // all the remaining height and push the button to the bottom).
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 0.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.button(t!("gui.mods_running_reload").to_string()).clicked() {
                            reload = true;
                        }
                    },
                );
            });
        self.running_open = open;
        if let (Some(id), Some(host)) = (stop, self.snap.mods_host.as_ref()) {
            host.stop(&id);
        }
        if reload {
            App::reload_mods(self.app.state().clone());
        }
        if self.running_open {
            // Keep the busy timers and counters live while the window is open.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    /// The "Mod views…" picker: one row per declared view.
    #[cfg(feature = "mods")]
    fn mod_views_window(&mut self, ctx: &egui::Context) {
        let mut open = self.views_open;
        let views = self.snap.mod_views.clone();
        let mut chosen: Option<(String, String, String)> = None;
        egui::Window::new(t!("gui.mods_views_title").to_string())
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if views.is_empty() {
                    ui.label(egui::RichText::new(t!("gui.mods_view_empty").to_string()).weak());
                    return;
                }
                for v in views.iter() {
                    let text = match &v.icon {
                        Some(ic) if !ic.is_empty() => format!("{ic} {}", v.label),
                        _ => v.label.clone(),
                    };
                    if ui
                        .button(text)
                        .on_hover_text(crate::mods::sanitize(&v.mod_name))
                        .clicked()
                    {
                        chosen = Some((v.mod_id.clone(), v.view_id.clone(), v.label.clone()));
                    }
                }
            });
        self.views_open = open;
        if let Some((mid, vid, label)) = chosen {
            App::open_mod_view(self.app.state().clone(), mid, vid, label);
            self.views_open = false;
        }
    }

    /// The open mod-view panel: its title and `(kind, text)` content blocks.
    #[cfg(feature = "mods")]
    fn mod_view_window(&mut self, ctx: &egui::Context) {
        let Some(panel) = self.snap.mod_view_panel.clone() else {
            return;
        };
        let mut close = false;
        egui::Window::new(crate::mods::sanitize(&panel.title))
            .id(egui::Id::new((
                "mod-view",
                panel.mod_id.clone(),
                panel.view_id.clone(),
            )))
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if panel.blocks.is_empty() {
                            ui.label(
                                egui::RichText::new(t!("gui.mods_view_empty").to_string()).weak(),
                            );
                        }
                        for (kind, text) in panel.blocks.iter() {
                            match kind.as_str() {
                                "divider" => {
                                    ui.separator();
                                }
                                "context" => {
                                    ui.label(egui::RichText::new(text.clone()).weak().small());
                                }
                                _ => {
                                    ui.label(text.clone());
                                }
                            }
                        }
                    });
                ui.separator();
                if ui.button(t!("gui.mods_view_close").to_string()).clicked() {
                    close = true;
                }
            });
        if close {
            App::close_mod_view(self.app.state().clone());
        }
    }

    /// The open mod dialog, as a modal: title, `(kind, text)` blocks, an action per
    /// `(id, label)` and a Close. An action click is delivered to the mod's
    /// `on-dialog-action` (the dialog is dismissed first); Esc and Close dismiss it.
    #[cfg(feature = "mods")]
    fn mod_dialog_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.snap.mod_dialog.clone() else {
            return;
        };
        let mut close = false;
        let mut invoke: Option<String> = None;
        // A foreground modal: the dimming rect also **absorbs clicks**, so the UI
        // behind it is not live while the dialog is open, and the content is drawn
        // after it in the same layer (so its buttons win the hit test).
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("mod-dialog"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let _bg = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, pal().tint(pal().scrim, 140));

                // The modal: horizontally centered, a fixed fraction down from the
                // top (so it reads as a dialog without measuring the content).
                let inner = egui::Rect::from_min_max(
                    egui::pos2(screen.left() + 24.0, screen.top() + screen.height() * 0.16),
                    egui::pos2(screen.right() - 24.0, screen.bottom() - 24.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                    ui.vertical_centered(|ui| {
                        egui::Frame::window(ui.style()).show(ui, |ui| {
                            ui.set_max_width(460.0_f32.min(inner.width() - 8.0));
                            ui.heading(crate::mods::sanitize(&dialog.title));
                            egui::ScrollArea::vertical()
                                .max_height((screen.height() * 0.4).max(120.0))
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    if dialog.blocks.is_empty() {
                                        ui.label(
                                            egui::RichText::new(
                                                t!("gui.mods_dialog_empty").to_string(),
                                            )
                                            .weak(),
                                        );
                                    }
                                    for (kind, text) in dialog.blocks.iter() {
                                        match kind.as_str() {
                                            "divider" => {
                                                ui.separator();
                                            }
                                            "context" => {
                                                ui.label(
                                                    egui::RichText::new(text.clone())
                                                        .weak()
                                                        .small(),
                                                );
                                            }
                                            _ => {
                                                ui.label(text.clone());
                                            }
                                        }
                                    }
                                });
                            ui.separator();
                            ui.horizontal_wrapped(|ui| {
                                for (id, label) in dialog.actions.iter() {
                                    if ui.button(label.clone()).clicked() {
                                        invoke = Some(id.clone());
                                    }
                                }
                                // A Close is always available, so a mod cannot trap
                                // the user.
                                if ui.button(t!("gui.mods_dialog_close").to_string()).clicked() {
                                    close = true;
                                }
                            });
                        });
                    });
                });
            });
        // Esc also dismisses (and is consumed, so it does not also act elsewhere).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            close = true;
        }
        if let Some(id) = invoke {
            App::invoke_mod_dialog_action(self.app.state().clone(), dialog.mod_id.clone(), id);
        } else if close {
            App::close_mod_dialog(self.app.state().clone());
        }
    }

    /// The settings window: language, interface scale, desktop notifications
    /// and the version. Opened by the gear in the top bar.
    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let pos = self.settings_rect.right_bottom() + egui::vec2(0.0, 4.0);
        egui::Window::new(t!("gui.settings").to_string())
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(300.0)
            .fixed_pos(pos)
            .constrain(true)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(t!("gui.language").to_string()).strong());
                let current = rust_i18n::locale().to_string();
                ui.horizontal(|ui| {
                    if selectable(ui, current == "it", t!("gui.language_it").to_string()).clicked()
                        && current != "it"
                    {
                        self.app.set_language("it".to_string());
                    }
                    if selectable(ui, current == "en", t!("gui.language_en").to_string()).clicked()
                        && current != "en"
                    {
                        self.app.set_language("en".to_string());
                    }
                });

                ui.separator();
                ui.label(egui::RichText::new(t!("gui.interface").to_string()).strong());
                ui.horizontal(|ui| {
                    let scale = self.snap.ui_scale.max(0.1);
                    if ui.button("A-").clicked() {
                        self.app.set_ui_scale(scale - 0.1);
                    }
                    ui.label(format!("{}%", (scale * 100.0).round() as i32));
                    if ui.button("A+").clicked() {
                        self.app.set_ui_scale(scale + 0.1);
                    }
                    if ui.button(t!("gui.reset").to_string()).clicked() {
                        self.app.set_ui_scale(1.0);
                    }
                });

                ui.separator();
                ui.label(egui::RichText::new(t!("gui.notifications_desktop").to_string()).strong());
                let mut on = self.snap.desktop_notifications;
                if ui
                    .checkbox(&mut on, t!("gui.notifications_toggle").to_string())
                    .changed()
                {
                    self.app.set_notifications(on);
                }

                #[cfg(feature = "mods")]
                {
                    ui.separator();
                    if ui.button(t!("gui.mods_menu").to_string()).clicked() {
                        App::open_mods_consent(self.app.state().clone());
                    }
                    if ui
                        .button(t!("gui.mods_settings_open").to_string())
                        .clicked()
                    {
                        App::open_mods_settings(self.app.state().clone());
                    }
                    // Only when there is at least one view (empty → nothing to show).
                    if !self.snap.mod_views.is_empty()
                        && ui.button(t!("gui.mods_views").to_string()).clicked()
                    {
                        self.views_open = true;
                    }
                    if ui.button(t!("gui.mods_running_open").to_string()).clicked() {
                        self.running_open = true;
                    }
                }

                ui.separator();
                ui.label(egui::RichText::new(build_info()).weak().small());
            });
        self.settings_open = open;
    }

    /// Transient messages (errors) stacked in the bottom-right corner.
    ///
    /// They fade on their own — a toast that must be dismissed is a chore — and
    /// they do not take the pointer: the area is not interactable, so a click
    /// goes to the UI underneath.
    fn toasts(&mut self, ctx: &egui::Context) {
        use std::time::Duration;
        const TTL_ERROR: Duration = Duration::from_secs(7);
        const TTL_INFO: Duration = Duration::from_secs(4);
        let now = std::time::Instant::now();
        let alive: Vec<&crate::app::Toast> = self
            .snap
            .toasts
            .iter()
            .filter(|t| now.duration_since(t.at) < if t.error { TTL_ERROR } else { TTL_INFO })
            .collect();
        if alive.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
            .interactable(false)
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                    // Newest first: bottom-up puts it nearest the corner.
                    for t in alive.iter().rev() {
                        let fill = if t.error {
                            pal().danger_bg
                        } else {
                            ui.visuals().window_fill
                        };
                        egui::Frame::none()
                            .fill(fill)
                            .rounding(8.0)
                            .inner_margin(egui::Margin::symmetric(10.0, 7.0))
                            .stroke(ui.visuals().window_stroke)
                            .show(ui, |ui| {
                                ui.set_max_width(360.0);
                                // A leading dot separates an error from an
                                // informational toast without reading the text,
                                // so the toast is not a bare sentence.
                                let (dot, col) = if t.error {
                                    (pal().danger, pal().text_primary)
                                } else {
                                    (pal().info, ui.visuals().text_color())
                                };
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("●").color(dot).size(13.0));
                                    ui.label(egui::RichText::new(&t.text).color(col).size(13.0));
                                });
                            });
                        ui.add_space(6.0);
                    }
                });
            });
        // Come back before the soonest expiry: the caller's 2 s tick alone would
        // leave a toast up to two seconds too long.
        let soonest = alive
            .iter()
            .map(|t| {
                let ttl = if t.error { TTL_ERROR } else { TTL_INFO };
                ttl.saturating_sub(now.duration_since(t.at))
            })
            .min()
            .unwrap_or(TTL_INFO);
        ctx.request_repaint_after(soonest.min(Duration::from_millis(500)));
    }

    /// The notification centre: recent mentions, DMs and broadcasts.
    fn notification_popover(&mut self, ctx: &egui::Context) {
        let pos = self.notif_rect.right_bottom() + egui::vec2(0.0, 4.0);
        // The list grows to the space left below the popover, not a fixed 440 px:
        // on a tall window that cap wasted the room, and on a short one the fixed
        // height pushed the tail off the bottom of the screen. Reserve the heading
        // and separator (plus a small margin) above it.
        let list_h = (ctx.screen_rect().bottom() - pos.y - 90.0).clamp(120.0, 760.0);
        let mut to_open: Option<(String, String, Option<String>)> = None;
        let mut mark_all = false;
        let area = egui::Area::new(egui::Id::new("notifiche_popover"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::RIGHT_TOP)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(360.0);
                    // An `Area` hands its content the rect it measured on the
                    // **previous** frame, and it keeps that state after the popover
                    // closes — so a list that wants to grow is trapped at whatever
                    // it was when it was last shown (that is why a `max_height` on
                    // the scroll area never helped: the parent rect, not the list,
                    // was the ceiling). Raising the content's own limit lifts it
                    // regardless of the stored area rect.
                    ui.set_max_height(list_h + 60.0);
                    // A **solid** scroll bar, not the default floating one: an
                    // overflowing list with an auto-hiding bar looks clipped rather
                    // than scrollable.
                    ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(t!("gui.notifications").to_string()).heading(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(t!("gui.mark_all_read").to_string()).clicked() {
                                mark_all = true;
                            }
                        });
                    });
                    ui.separator();
                    if self.snap.notifications.is_empty() {
                        ui.weak(t!("gui.no_notifications").to_string());
                        return;
                    }
                    egui::ScrollArea::vertical()
                        .max_height(list_h)
                        .id_salt("notifiche_scroll")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for n in self.snap.notifications.clone() {
                                let title = if n.is_dm {
                                    if n.author.is_empty() {
                                        self.conversation_name(&n.channel_id)
                                    } else {
                                        n.author.clone()
                                    }
                                } else if !n.channel_name.is_empty()
                                    && n.channel_name != n.channel_id
                                {
                                    format!("#{}", n.channel_name)
                                } else {
                                    // An empty name must not leave the bare id:
                                    // resolve it.
                                    self.conversation_name(&n.channel_id)
                                };
                                let when = crate::app::format_ts(&n.ts);
                                let text = if n.is_dm {
                                    n.text.clone()
                                } else {
                                    format!("{}: {}", n.author, n.text)
                                };
                                let mut head = egui::RichText::new(format!("{title}   {when}"));
                                if n.read {
                                    head = head.weak();
                                } else {
                                    head = head.strong().color(pal().link);
                                }
                                let clicked = ui
                                    .push_id(
                                        ("notifica", n.channel_id.as_str(), n.ts.as_str()),
                                        |ui| {
                                            let resp = ui
                                                .add(
                                                    egui::Label::new(head)
                                                        .sense(egui::Sense::click())
                                                        .selectable(false),
                                                )
                                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                                            ui.label(egui::RichText::new(&text).weak());
                                            resp
                                        },
                                    )
                                    .inner
                                    .clicked();
                                if clicked {
                                    to_open = Some((
                                        n.channel_id.clone(),
                                        n.ts.clone(),
                                        n.thread_ts.clone(),
                                    ));
                                }
                                ui.separator();
                            }
                        });
                });
            });

        if mark_all {
            self.app.mark_all_notifications_read();
        }
        if let Some((channel_id, ts, thread_ts)) = to_open {
            self.app.open_notification(channel_id, ts, thread_ts);
            self.notif_open = false;
        }

        let popup = area.response.rect;
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx
                .pointer_interact_pos()
                .is_some_and(|p| !popup.expand(4.0).contains(p) && !self.notif_rect.contains(p));
        if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.notif_open = false;
        }
    }

    /// Copies the current status into the popover's edit fields, so it opens
    /// showing what is set and not an empty box.
    fn seed_profile_status(&mut self) {
        let st = self.snap.user_status.get(&self.snap.me);
        self.profile_status_text = st.map(|s| s.text.clone()).unwrap_or_default();
        self.profile_status_emoji = st
            .map(|s| s.emoji.trim_matches(':').to_string())
            .unwrap_or_default();
    }

    /// The profile popover: identity, status, presence and logout.
    fn profile_popover(&mut self, ctx: &egui::Context) {
        let me = self.snap.me.clone();
        let name = self
            .snap
            .user_names
            .get(&me)
            .cloned()
            .unwrap_or_else(|| me.clone());
        let pos = self.profile_rect.right_bottom() + egui::vec2(0.0, 4.0);
        let area = egui::Area::new(egui::Id::new("profilo_popover"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(egui::Align2::RIGHT_TOP)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(260.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&name).heading());

                        let current = self.snap.user_status.get(&me).cloned().unwrap_or_default();
                        ui.horizontal(|ui| {
                            if current.is_empty() {
                                ui.label(
                                    egui::RichText::new(t!("gui.no_status").to_string()).weak(),
                                );
                            } else {
                                let emoji = current.emoji.trim_matches(':').to_string();
                                if !emoji.is_empty() {
                                    self.emoji(ui, &emoji);
                                }
                                if !current.text.is_empty() {
                                    ui.label(&current.text);
                                }
                            }
                        });

                        ui.separator();
                        ui.label(egui::RichText::new(t!("gui.status").to_string()).strong());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.profile_status_text)
                                .hint_text(t!("gui.status_hint").to_string())
                                .desired_width(f32::INFINITY),
                        );

                        let mut picked: Option<String> = None;
                        ui.horizontal_wrapped(|ui| {
                            for &emoji in QUICK_STATUS {
                                let selected = self.profile_status_emoji.trim_matches(':') == emoji;
                                let resp = self.emoji_icon_button(ui, emoji);
                                if selected {
                                    ui.painter().rect_stroke(
                                        resp.rect.expand(2.0),
                                        4.0,
                                        egui::Stroke::new(1.5_f32, ui.visuals().selection.bg_fill),
                                    );
                                }
                                if resp.clicked() {
                                    picked = Some(emoji.to_string());
                                }
                            }
                            if !self.profile_status_emoji.is_empty()
                                && close_button(ui)
                                    .on_hover_text(t!("gui.remove_emoji_hover").to_string())
                                    .clicked()
                            {
                                picked = Some(String::new());
                            }
                        });
                        if let Some(p) = picked {
                            self.profile_status_emoji = p;
                        }

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button(t!("gui.set").to_string()).clicked() {
                                let emoji = if self.profile_status_emoji.trim().is_empty() {
                                    String::new()
                                } else {
                                    format!(":{}:", self.profile_status_emoji.trim_matches(':'))
                                };
                                self.app
                                    .set_status(emoji, self.profile_status_text.trim().to_string());
                                self.profile_open = false;
                            }
                            if ui.button(t!("gui.clear_status").to_string()).clicked() {
                                self.profile_status_text.clear();
                                self.profile_status_emoji.clear();
                                self.app.clear_status();
                                self.profile_open = false;
                            }
                        });

                        ui.separator();
                        ui.label(egui::RichText::new(t!("gui.presence").to_string()).strong());
                        // Away is the user's own choice and is respected; without
                        // it, presence is asserted regardless of whether the window
                        // is in the foreground (`drive_presence`).
                        // Render the user's own choice when they made one: the
                        // server's verdict can flap (Slack's own idle), and it must
                        // not move the selection under them.
                        let active = self.presence_manual.unwrap_or_else(|| {
                            self.snap.presence.get(&me).copied().unwrap_or(true)
                        });
                        ui.horizontal(|ui| {
                            if selectable(ui, active, t!("gui.active").to_string()).clicked()
                                && !active
                            {
                                self.presence_manual = Some(true);
                                self.app.set_presence(true);
                            }
                            if selectable(ui, !active, t!("gui.away").to_string()).clicked()
                                && active
                            {
                                self.presence_manual = Some(false);
                                self.app.set_presence(false);
                            }
                        });

                        ui.separator();
                        let esci = egui::Button::new(
                            egui::RichText::new(t!("gui.log_out").to_string())
                                .color(pal().on_danger),
                        )
                        .fill(pal().danger_fill)
                        .min_size(egui::vec2(ui.available_width(), 0.0));
                        if ui.add(esci).clicked() {
                            self.app.logout();
                            self.profile_open = false;
                        }

                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(build_info()).weak().small());
                    });
                });
            });

        let popup = area.response.rect;
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx
                .pointer_interact_pos()
                .is_some_and(|p| !popup.expand(4.0).contains(p) && !self.profile_rect.contains(p));
        if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.profile_open = false;
        }
    }

    /// Starts or ends the call on the open channel.
    fn toggle_call(&mut self) {
        if self.snap.call.is_some() {
            self.app.hang_up();
        } else {
            self.app.start_call();
        }
    }

    /// The bar for the call in progress, above the messages.
    fn call_bar(&mut self, ui: &mut egui::Ui) {
        let Some(c) = self.snap.call.clone() else {
            return;
        };
        ui.horizontal(|ui| {
            let (col, testo) = if let Some(e) = &c.error {
                (pal().danger, t!("gui.call_error", err = e).to_string())
            } else if c.connected {
                (
                    pal().success,
                    t!(
                        "gui.call_active",
                        min = c.seconds / 60,
                        sec = format!("{:02}", c.seconds % 60)
                    )
                    .to_string(),
                )
            } else {
                (pal().text_muted, t!("gui.connecting").to_string())
            };
            ui.label(egui::RichText::new("\u{25cf}").color(col));
            ui.label(testo);
            ui.separator();
            if ui
                .button(if c.muted {
                    t!("gui.unmute").to_string()
                } else {
                    t!("gui.mute").to_string()
                })
                .clicked()
            {
                self.app.toggle_call_mute();
            }
            if ui.button(t!("gui.hang_up").to_string()).clicked() {
                self.app.hang_up();
            }
            ui.separator();
            // We cannot share a screen yet: for that you need the
            // browser.
            if ui
                .button(t!("gui.open_in_browser").to_string())
                .on_hover_text(t!("gui.open_for_screen_share_hover").to_string())
                .clicked()
            {
                self.open_huddle();
            }
        });
        ui.separator();
    }

    /// The huddle window: who is in the call, and the controls.
    ///
    /// A window of its own, like the official client: a call is not part of
    /// the conversation you are reading, and while it is going you want to be
    /// able to look at something else without losing the controls.
    ///
    /// It is an immediate viewport rather than a deferred one because it
    /// draws from `self`: a deferred one runs on its own and would need a
    /// copy of the state.
    fn huddle_window(&mut self, ctx: &egui::Context) {
        let Some(call) = self.snap.call.clone() else {
            return;
        };
        let people = self
            .snap
            .huddles
            .get(&call.channel)
            .map(|h| h.participants.clone())
            .unwrap_or_default();

        let id = egui::ViewportId::from_hash_of("huddle");
        let builder = egui::ViewportBuilder::default()
            .with_title(t!("gui.huddle_title").to_string())
            .with_inner_size([640.0, 420.0]);

        ctx.show_viewport_immediate(id, builder, |ctx, _class| {
            egui::TopBottomPanel::bottom("huddle_controls").show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    if ui
                        .button(if call.muted {
                            t!("gui.unmute").to_string()
                        } else {
                            t!("gui.mute").to_string()
                        })
                        .clicked()
                    {
                        self.app.toggle_call_mute();
                    }
                    // Sharing needs the VP8 encoder, which is behind the `vp8`
                    // feature because libvpx costs a long first build. Without
                    // it the button stays disabled and says why, rather than
                    // vanishing.
                    #[cfg(feature = "vp8")]
                    if ui
                        .button(if call.sharing_own {
                            t!("gui.stop_sharing").to_string()
                        } else {
                            t!("gui.share_screen").to_string()
                        })
                        .clicked()
                    {
                        if call.sharing_own {
                            self.app.toggle_call_screen_share(None);
                        } else {
                            self.open_share_picker();
                        }
                    }
                    #[cfg(not(feature = "vp8"))]
                    ui.add_enabled(false, egui::Button::new(t!("gui.share_screen").to_string()))
                        .on_disabled_hover_text(t!("gui.vp8_hint").to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        if ui.button(t!("gui.log_out").to_string()).clicked() {
                            self.app.hang_up();
                        }
                        if ui
                            .button(t!("gui.open_in_browser").to_string())
                            .on_hover_text(t!("gui.open_to_share_hover").to_string())
                            .clicked()
                        {
                            self.open_huddle();
                        }
                    });
                });
                ui.add_space(6.0);
            });

            // The shared screen is what you are there for, so it takes the room
            // and the faces move to a strip along the bottom. That strip is a
            // panel of its own, declared **before** the central one, so it is given
            // its space first: drawn last, inside the central panel, it was clipped
            // the moment the shared image filled the height (a single tile row is
            // taller than the old fixed reserve).
            let sharing = self.screen_texture(ctx);
            if sharing.is_some() {
                self.huddle_people_strip(ctx, &people);
            }

            egui::CentralPanel::default().show(ctx, |ui| {
                let state = if let Some(e) = &call.error {
                    t!("gui.call_error", err = e).to_string()
                } else if call.connected {
                    t!(
                        "gui.call_active",
                        min = call.seconds / 60,
                        sec = format!("{:02}", call.seconds % 60)
                    )
                    .to_string()
                } else {
                    t!("gui.connecting").to_string()
                };
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(state);
                });
                ui.separator();
                match &sharing {
                    Some(tex) => {
                        // The strip above already reserved its space, so the screen
                        // takes whatever is left.
                        ui.vertical_centered(|ui| {
                            ui.add(
                                egui::Image::new(tex)
                                    .max_size(ui.available_size())
                                    .maintain_aspect_ratio(true),
                            );
                        });
                    }
                    None => {
                        if call.screen_on {
                            // The name only when it is readable: an unresolved
                            // id is a number on screen, which says less than
                            // saying nothing.
                            let who = call
                                .sharing
                                .as_ref()
                                .and_then(|u| self.snap.user_names.get(u).cloned())
                                .map(|n| t!("gui.who_is", who = n).to_string())
                                .unwrap_or_else(|| t!("gui.someone_is").to_string());
                            ui.add_space(6.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new(
                                        t!("gui.sharing_screen", who = who).to_string(),
                                    )
                                    .weak(),
                                );
                            });
                        }
                        self.huddle_tiles(ui, &people);
                    }
                }
            });

            // The picker belongs to the huddle's viewport, not the main one:
            // drawn from the main window it would appear behind the huddle.
            if self.share_picker.is_some() {
                self.share_picker_dialog(ctx);
            }

            // The window's close button leaves the call: a huddle window with
            // nobody in it would be a lie, and a call still running with no
            // window would be worse.
            if ctx.input(|i| i.viewport().close_requested()) {
                self.app.hang_up();
            }
        });
    }

    /// The shared screen as a texture, uploaded only when the frame changes.
    ///
    /// The picture arrives as RGBA from the decoder; turning it into a
    /// texture can only happen on the graphics thread, which is here.
    fn screen_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let pic = self.snap.screen.clone()?;
        if let Some((have, tex)) = &self.screen_tex {
            if Arc::ptr_eq(have, &pic) {
                return Some(tex.clone());
            }
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [pic.width as usize, pic.height as usize],
            &pic.rgba,
        );
        let tex = ctx.load_texture("huddle_screen", image, egui::TextureOptions::LINEAR);
        self.screen_tex = Some((pic, tex.clone()));
        Some(tex)
    }

    /// The participants strip shown while a screen is shared.
    ///
    /// A panel of its own, declared **before** the central one, so it is given
    /// its space first: drawn last, inside the central panel, it was clipped the
    /// moment the shared image filled the height. It must always show one full
    /// row of tiles — with no minimum the panel settled shorter than a tile and
    /// cut it (worse the more of the window the shared screen took).
    ///
    /// Returns `(needed, shown)`: the height the tiles asked for and the height
    /// the strip actually shows. Equal (or `needed` smaller) means nothing is
    /// cut; a test asserts that.
    fn huddle_people_strip(&mut self, ctx: &egui::Context, people: &[String]) -> (f32, f32) {
        // The minimum is the height the tiles needed **last frame** (egui panels
        // settle in one frame), so a row is never cut — a guessed constant could
        // be too small for a longer name or a larger type scale. The cap stops a
        // long list from eating the screen; past it the strip scrolls.
        let cap = (ctx.screen_rect().height() * 0.4).max(self.huddle_row_min);
        let mut needed = 0.0;
        let mut shown = 0.0;
        let mut row_h = 0.0;
        let out = egui::TopBottomPanel::bottom("huddle_people")
            .min_height(self.huddle_row_min)
            .max_height(cap)
            .show(ctx, |ui| {
                let sc = egui::ScrollArea::vertical()
                    .id_salt("huddle_people_scroll")
                    .max_height(cap)
                    .show(ui, |ui| {
                        row_h = self.huddle_tiles(ui, people);
                    });
                needed = sc.content_size.y;
                shown = sc.inner_rect.height();
            });
        // `min_height` sizes the *panel*; the `ScrollArea` viewport is that minus
        // the panel frame. Reserve exactly one tile row — the leading gap plus the
        // measured tile plus the frame — so a row is never cut. It is measured,
        // not a guessed constant, so a longer name or a larger type scale still
        // fits.
        let frame = (out.response.rect.height() - shown).max(0.0);
        self.huddle_row_min = HUDDLE_LEADING + row_h + frame;
        (needed, shown)
    }

    /// The participants, as tiles with their avatar.
    fn huddle_tiles(&mut self, ui: &mut egui::Ui, people: &[String]) -> f32 {
        if people.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(t!("gui.nobody_else").to_string()).weak());
            });
            return 0.0;
        }
        ui.add_space(HUDDLE_LEADING);
        let speaker = self.snap.call.as_ref().and_then(|c| c.speaker.clone());
        let self_level = self.snap.call.as_ref().map(|c| c.self_level).unwrap_or(0);
        let me = self.snap.me.clone();
        let mut row_h = 0.0;
        ui.horizontal_wrapped(|ui| {
            for u in people {
                let name = self
                    .snap
                    .user_names
                    .get(u)
                    .cloned()
                    .unwrap_or_else(|| u.clone());
                let avatar = self.snap.avatars.get(u).cloned().unwrap_or_default();
                let tex = self.texture(ui.ctx(), &avatar);
                let speaking =
                    speaker.as_deref() == Some(u.as_str()) || (u == &me && self_level >= 8);
                let tile = egui::Frame::none()
                    .fill(ui.visuals().faint_bg_color)
                    .rounding(8.0)
                    .inner_margin(HUDDLE_MARGIN)
                    .show(ui, |ui| {
                        ui.set_width(HUDDLE_TILE_W);
                        ui.vertical_centered(|ui| {
                            let avatar = egui::vec2(HUDDLE_AVATAR, HUDDLE_AVATAR);
                            let avatar_resp = match tex {
                                Some(t) => ui.add(
                                    egui::Image::new(&t)
                                        .fit_to_exact_size(avatar)
                                        .rounding(HUDDLE_AVATAR / 2.0),
                                ),
                                None => {
                                    let (rect, resp) =
                                        ui.allocate_exact_size(avatar, egui::Sense::hover());
                                    paint_initials(ui.painter(), rect, u, &name, HUDDLE_AVATAR / 2.0);
                                    resp
                                }
                            };
                            if speaking {
                                ui.painter().circle_stroke(
                                    avatar_resp.rect.center(),
                                    HUDDLE_AVATAR / 2.0,
                                    egui::Stroke::new(3.0_f32, pal().success),
                                );
                            }
                            ui.add_space(HUDDLE_GAP);
                            // One line, ellipsized: it bounds the row's height,
                            // so the strip below the screen can reserve exactly
                            // one row (`huddle_people_strip`).
                            ui.add(egui::Label::new(name).truncate());
                        });
                    });
                if row_h == 0.0 {
                    row_h = tile.response.rect.height();
                }
            }
        });
        row_h
    }

    fn open_huddle(&self) {
        let Some(id) = self.current_channel() else {
            return;
        };
        if self.snap.team_id.is_empty() {
            tracing::warn!("team id not available yet");
            return;
        }
        let browser = crate::huddle::Browser::from_env();
        if let Err(e) = crate::huddle::open(&self.snap.team_id, &id, &browser) {
            tracing::warn!("failed to open huddle: {e}");
        }
    }
}

/// Removes the input method from the environment, before the window is born.
///
/// With ibus active on X11 accented letters go through the IME path and get
/// lost: typing "perche è così" leaves "perche", and everything after it
/// disappears. In Italian that makes the client unusable.
///
/// It is an upstream defect, in winit/egui: here we simply do not use that
/// path. The cost is that anyone writing Chinese, Japanese or Korean genuinely
/// needs the IME — for them `SLAQ_KEEP_IME=1` puts it back.
fn disable_broken_ime() {
    if std::env::var_os("SLAQ_KEEP_IME").is_some() {
        return;
    }
    // SAFETY: done before creating the window and before starting any
    // other thread that reads the environment.
    unsafe {
        for v in ["XMODIFIERS", "GTK_IM_MODULE", "QT_IM_MODULE"] {
            std::env::remove_var(v);
        }
    }
}

/// Starts the window. Blocks until the user closes it.
pub fn run(app: App) -> Result<()> {
    disable_broken_ime();
    // The preview has no workspace: start no network task.
    let demo = app.is_demo();
    if !demo {
        app.spawn_bootstrap();
        app.spawn_emoji();
        app.spawn_ws_pump();
    }

    let cookie = app.creds().xoxd.clone();
    let cookie = if cookie.is_empty() {
        None
    } else {
        Some(cookie)
    };
    let rt = tokio::runtime::Handle::current();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 820.0]),
        ..Default::default()
    };

    // `run_native` blocks the thread until the window closes, and that
    // thread is also a runtime worker: without `block_in_place` the tasks already
    // assigned to it stay still forever — the channel loading stopped halfway,
    // with no errors, the instant the window opened. `block_in_place` tells tokio
    // to move its work elsewhere.
    tokio::task::block_in_place(move || {
        eframe::run_native(
            "slaq",
            options,
            Box::new(move |cc| {
                install_fonts(&cc.egui_ctx);
                install_style(&cc.egui_ctx);
                Ok(Box::new(Gui::new(app, cookie, rt)) as Box<dyn eframe::App>)
            }),
        )
        .map_err(|e| anyhow::anyhow!("window startup failed: {e}"))
    })
}

#[cfg(test)]
mod presence_tests {
    use super::{Presence, Snapshot, presence_of};
    use crate::api::client::DndInfo;
    use std::sync::Arc;

    fn snap() -> Snapshot {
        Snapshot::default()
    }

    /// Presence has to be *driven*: the call used to come only from the profile
    /// popover, so a client that never opened it stayed offline for everyone else,
    /// and a keepalive gated on the window having focus let it drop offline again
    /// as soon as the user worked in another window.
    #[test]
    fn the_presence_keepalive_fires_while_the_app_runs() {
        use std::time::Duration;
        let now = std::time::Instant::now();
        let keep = |last, manual_away| super::presence_keepalive_due(last, now, manual_away);
        assert!(keep(None, false), "the first frame sends it");
        let recent = now - Duration::from_secs(5);
        assert!(!keep(Some(recent), false), "not once per frame");
        let old = now - super::PRESENCE_KEEPALIVE;
        assert!(keep(Some(old), false), "but again after the interval");
        assert!(!keep(Some(old), true), "and never against an explicit Away");
    }

    #[test]
    fn an_unknown_user_keeps_the_neutral_sigil() {
        // Better no information than assuming they are away: presence
        // arrives a moment after startup.
        assert_eq!(presence_of(&snap(), "U1"), Presence::Unknown);
    }

    #[test]
    fn connected_and_disconnected_differ() {
        let mut s = snap();
        Arc::make_mut(&mut s.presence).insert("U1".into(), true);
        Arc::make_mut(&mut s.presence).insert("U2".into(), false);
        assert_eq!(presence_of(&s, "U1"), Presence::Active);
        assert_eq!(presence_of(&s, "U2"), Presence::Away);
    }

    #[test]
    fn quiet_hours_win_over_being_connected() {
        // This is the point: someone can be connected at 23:00 and not want
        // to be disturbed, and that is what changes the decision to write to them.
        let now = chrono::Utc::now().timestamp();
        let mut s = snap();
        Arc::make_mut(&mut s.presence).insert("U1".into(), true);
        Arc::make_mut(&mut s.dnd).insert(
            "U1".into(),
            DndInfo {
                dnd_enabled: true,
                next_dnd_start_ts: now - 60,
                next_dnd_end_ts: now + 3600,
                snooze_enabled: false,
            },
        );
        assert_eq!(presence_of(&s, "U1"), Presence::Quiet);
    }

    #[test]
    fn a_schedule_outside_its_window_does_not_hide_presence() {
        let now = chrono::Utc::now().timestamp();
        let mut s = snap();
        Arc::make_mut(&mut s.presence).insert("U1".into(), true);
        Arc::make_mut(&mut s.dnd).insert(
            "U1".into(),
            DndInfo {
                dnd_enabled: true,
                next_dnd_start_ts: now + 3600,
                next_dnd_end_ts: now + 7200,
                snooze_enabled: false,
            },
        );
        assert_eq!(presence_of(&s, "U1"), Presence::Active);
    }
}

#[cfg(test)]
mod unread_tests {
    use super::{UnreadMark, split_unread};

    fn mark(top: f32, mention: bool) -> UnreadMark {
        UnreadMark {
            top,
            bottom: top + 20.0,
            mention,
        }
    }

    /// Visible window from 100 to 300, list already scrolled by 50.
    fn split(marks: &[UnreadMark]) -> (super::UnreadCount, super::UnreadCount) {
        split_unread(marks, 100.0, 300.0, 50.0)
    }

    #[test]
    fn what_is_visible_is_not_counted() {
        let (a, b) = split(&[mark(150.0, false), mark(250.0, false)]);
        assert_eq!((a.channels, b.channels), (0, 0));
    }

    #[test]
    fn above_and_below_are_told_apart() {
        let (a, b) = split(&[mark(10.0, false), mark(400.0, false), mark(500.0, false)]);
        assert_eq!(a.channels, 1);
        assert_eq!(b.channels, 2);
    }

    #[test]
    fn mentions_are_counted_separately() {
        let (a, _) = split(&[mark(10.0, true), mark(30.0, false)]);
        assert_eq!((a.channels, a.mentions), (2, 1));
    }

    #[test]
    fn a_partly_visible_row_counts_as_visible() {
        // Past the edge but not entirely: sending the user to look for a
        // row they are already looking at is worse than saying nothing.
        let (a, b) = split(&[mark(90.0, false), mark(295.0, false)]);
        assert_eq!((a.channels, b.channels), (0, 0));
    }

    #[test]
    fn the_nearest_above_is_the_lowest_one_above() {
        // Rows at 0 and at 60: the one nearest the edge is the one at 60.
        let (a, _) = split(&[mark(0.0, false), mark(60.0, false)]);
        assert_eq!(a.nearest, 50.0 + (60.0 - 100.0));
    }

    #[test]
    fn the_nearest_below_is_the_highest_one_below() {
        let (_, b) = split(&[mark(600.0, false), mark(400.0, false)]);
        assert_eq!(b.nearest, 50.0 + (400.0 - 100.0));
    }

    #[test]
    fn nothing_unread_means_no_strip() {
        let (a, b) = split(&[]);
        assert!(!a.any() && !b.any());
    }
}

#[cfg(test)]
mod label_tests {
    use super::{
        ChannelKind, ChannelRow, channel_kind, conversation_label, is_continuation,
        matching_timestamps, same_day_in, search_hit_label,
    };
    use crate::app::MessageEntry;
    use std::collections::HashMap;

    fn channel(id: &str, name: &str, is_dm: bool, user: Option<&str>) -> ChannelRow {
        ChannelRow {
            id: id.into(),
            name: name.into(),
            is_dm,
            unread: false,
            mentions: 0,
            user: user.map(str::to_string),
        }
    }

    fn message(ts: &str, text: &str) -> MessageEntry {
        MessageEntry {
            user: "U1".into(),
            text: text.into(),
            ts: ts.into(),
            reactions: Vec::new(),
            thread_ts: None,
            reply_count: None,
            author_icon: String::new(),
            attachments: Vec::new(),
            files: Vec::new(),
            actions: Vec::new(),
            bot_id: None,
            app_id: None,
        }
    }

    #[test]
    fn consecutive_messages_from_one_author_group() {
        let a = message("1700000000.000100", "primo");
        let mut b = message("1700000060.000100", "secondo");
        assert!(is_continuation(Some(&a), &b));
        // Past the window: a fresh header.
        b.ts = "1700000400.000100".into();
        assert!(!is_continuation(Some(&a), &b));
    }

    #[test]
    fn a_different_author_breaks_the_group() {
        let a = message("1700000000.000100", "primo");
        let mut b = message("1700000010.000100", "risposta");
        b.user = "U2".into();
        assert!(!is_continuation(Some(&a), &b));
    }

    #[test]
    fn nothing_groups_above_the_first_message() {
        let a = message("1700000000.000100", "primo");
        assert!(!is_continuation(None, &a));
    }

    #[test]
    fn same_day_in_uses_the_zone_not_utc() {
        use chrono::{FixedOffset, TimeZone as _};
        // A non-UTC reader, +02:00.
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let mk = |h: u32, m: u32, day: u32| {
            let d = chrono::NaiveDate::from_ymd_opt(2026, 7, day).unwrap();
            tz.from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap())
                .single()
                .unwrap()
                .timestamp()
        };
        let late = mk(23, 58, 1);
        let early = mk(0, 2, 2);
        // 4 minutes apart, but a different *local* day: not one group.
        assert!(!same_day_in(&tz, late, early));
        // Those same instants are one UTC day — so the old UTC rule grouped them.
        assert!(same_day_in(&chrono::Utc, late, early));
    }

    #[test]
    fn grouping_does_not_panic_on_a_wild_timestamp() {
        let a = message(&format!("{}.000000", i64::MIN), "a");
        let b = message(&format!("{}.000000", i64::MAX), "b");
        assert!(!is_continuation(Some(&a), &b));
    }

    /// A guard: the value of a colour role lives in `crate::theme`, never here.
    /// A raw colour literal in this file is a colour that will drift from the
    /// palette the moment someone edits it. `parse_hex` was the last one and now
    /// lives in the theme too.
    ///
    /// Best-effort, deliberately: it is a denylist of egui's named colours and
    /// constructor prefixes, scanning **only this file**. `Color32::TRANSPARENT`
    /// (a 1×1 placeholder image) and `Color32::PLACEHOLDER` (layout metrics) are
    /// allowed and intentional. A colour in a new `gui/*.rs`, or reached through
    /// an alias, is out of scope — hence MASTER says "best-effort", not "any".
    #[test]
    fn no_raw_colours_outside_the_theme() {
        let src = include_str!("mod.rs");
        // Built from parts so this test's own needles do not appear in the
        // source it scans.
        let needles = [
            concat!("Color32::", "from_"),
            concat!("Color32::", "GRAY"),
            concat!("Color32::", "DARK_GRAY"),
            concat!("Color32::", "LIGHT_GRAY"),
            concat!("Color32::", "WHITE"),
            concat!("Color32::", "BLACK"),
            concat!("Color32::", "RED"),
            concat!("Color32::", "GREEN"),
            concat!("Color32::", "BLUE"),
            concat!("Color32::", "YELLOW"),
            concat!("Color32::", "BROWN"),
            concat!("Color32::", "PINK"),
            concat!("Color32::", "PURPLE"),
        ];
        for n in needles {
            assert!(
                !src.contains(n),
                "raw colour `{n}` in gui/mod.rs — ask the palette for a role instead"
            );
        }
    }

    #[test]
    fn a_channel_gets_the_hash_and_its_name() {
        let c = channel("C1", "general", false, None);
        let names = HashMap::new();
        assert_eq!(conversation_label(Some(&c), &names, "C1"), "#general");
    }

    #[test]
    fn a_dm_is_named_after_the_peer_not_the_id() {
        let c = channel("D1", "D1", true, Some("U2"));
        let mut names = HashMap::new();
        names.insert("U2".to_string(), "Giulia".to_string());
        assert_eq!(conversation_label(Some(&c), &names, "D1"), "Giulia");
    }

    #[test]
    fn an_unresolved_dm_never_shows_a_bare_id() {
        let c = channel("D1", "D1", true, Some("U2"));
        let names = HashMap::new();
        let label = conversation_label(Some(&c), &names, "D1");
        assert_ne!(label, "D1");
        assert!(label.contains("D1"));
    }

    #[test]
    fn an_unknown_conversation_is_prefixed_not_bare() {
        let names = HashMap::new();
        let label = conversation_label(None, &names, "D9");
        assert_ne!(label, "D9");
        assert!(label.contains("D9"));
    }

    #[test]
    fn a_channel_id_is_told_from_a_dm_id() {
        assert_eq!(channel_kind("C1"), ChannelKind::Channel);
        assert_eq!(channel_kind("G1"), ChannelKind::Channel);
        assert_eq!(channel_kind("D1"), ChannelKind::Direct);
    }

    #[test]
    fn a_dm_search_hit_is_named_after_the_peer_not_the_id() {
        let names = HashMap::new();
        // Slack fills a DM hit's channel name with the peer's raw user id.
        let label = search_hit_label("D1", "U2", "daniele.baschieri", None, &names);
        assert_eq!(label, "daniele.baschieri");
        assert!(!label.contains('#'));
        assert!(!label.contains("D1"));
        assert!(!label.contains("U2"));
    }

    #[test]
    fn a_dm_search_hit_resolves_the_peer_when_it_can() {
        let c = channel("D1", "U2", true, Some("U2"));
        let mut names = HashMap::new();
        names.insert("U2".to_string(), "Giulia".to_string());
        assert_eq!(
            search_hit_label("D1", "U2", "daniele", Some(&c), &names),
            "Giulia"
        );
    }

    #[test]
    fn a_channel_search_hit_gets_the_hash_and_name() {
        let names = HashMap::new();
        assert_eq!(
            search_hit_label("C1", "general", "U1", None, &names),
            "#general"
        );
        assert_eq!(
            search_hit_label("G1", "random", "U1", None, &names),
            "#random"
        );
    }

    #[test]
    fn a_search_hit_never_prints_a_hash_id_or_a_bare_id() {
        let names = HashMap::new();
        let dm = search_hit_label("D1", "U2", "?", None, &names);
        assert_ne!(dm, "D1");
        assert_ne!(dm, "U2");
        assert!(!dm.starts_with('#'));
        let channel = search_hit_label("C1", "C1", "?", None, &names);
        assert_ne!(channel, "#C1");
        assert_ne!(channel, "C1");
        assert!(channel.contains("C1"));
    }

    #[test]
    fn matches_are_returned_as_timestamps_in_order() {
        let msgs = vec![
            message("1", "ciao mondo"),
            message("2", "niente"),
            message("3", "CIAO di nuovo"),
        ];
        assert_eq!(matching_timestamps(&msgs, "ciao"), vec!["1", "3"]);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let msgs = vec![message("1", "ciao")];
        assert!(matching_timestamps(&msgs, "   ").is_empty());
    }
}

#[cfg(test)]
mod style_tests {
    use super::*;

    /// Regression guard for the "thread panel grows until it fills the
    /// screen" bug: the composer's icon buttons are laid out at a fixed
    /// `COMPOSER_BTN_W`, and their intrinsic width is `icon + 2×padding`.
    /// If that exceeds the slot, the button overflows; a content-sized panel
    /// (the thread) reads its width from the content rect, so the overflow
    /// compounds every frame.
    #[test]
    fn composer_icon_buttons_fit_their_slot() {
        let needed = COMPOSER_ICON + 2.0 * BUTTON_PADDING_X;
        assert!(
            needed <= COMPOSER_BTN_W + 0.01,
            "composer button content ({needed}) exceeds its slot ({COMPOSER_BTN_W}); \
             it would overflow and grow content-sized panels every frame"
        );
    }

    /// The `/` completion must only fire at the very start and before the
    /// arguments, not inside a path or after some text.
    #[test]
    fn slash_completion_only_at_the_start() {
        assert_eq!(slash_command_fragment("/rem", 4), Some("rem"));
        assert_eq!(slash_command_fragment("/", 1), Some(""));
        assert_eq!(slash_command_fragment("/remind me", 10), None);
        assert_eq!(slash_command_fragment("hello /rem", 10), None);
        assert_eq!(slash_command_fragment("/usr/bin", 8), None);
    }
}

/// Image snapshots of the theme and of representative widgets.
///
/// Visual regression guards: they render headlessly (wgpu) and compare against
/// `tests/snapshots/`. To accept an intended change, delete the baseline `png`
/// and run once — the run writes `<name>.new.png`; then commit it as the new
/// baseline.
#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// A mod theme overrides only known roles; an unknown role or a malformed
    /// value leaves the default in place.
    #[cfg(feature = "mods")]
    #[test]
    fn a_mod_theme_overrides_known_roles_only() {
        let theme = vec![
            ("accent".to_string(), "ff0000".to_string()),
            ("nope".to_string(), "00ff00".to_string()),
            ("bg_base".to_string(), "zzz".to_string()),
        ];
        let p = palette_from(&theme);
        assert_eq!(p.accent, parse_hex("ff0000").unwrap());
        assert_eq!(p.bg_base, Palette::DARK.bg_base, "bad hex: default stays");
        assert_eq!(p.bg_panel, Palette::DARK.bg_panel, "untouched role");
    }

    /// A harness of the given size with the app's fonts and style installed
    /// before any text is drawn.
    ///
    /// `egui_kittest` runs one pass while building the harness, before we get a
    /// chance to install the fonts; drawing the app there would panic on the
    /// first bold run. So the first pass only installs fonts and style (which
    /// egui activates at the start of the *next* pass), and drawing begins on
    /// the following frame — exactly as `run` installs them on the eframe
    /// context before the window is shown.
    fn the_harness<'a>(
        size: egui::Vec2,
        build: impl FnMut(&egui::Context) + 'a,
    ) -> egui_kittest::Harness<'a, bool> {
        harness_in("it", size, 1.0, build)
    }

    /// Compares the harness's rendering with `tests/snapshots/<name>.png`.
    ///
    /// The images are recorded on a hardware renderer, and a software one (the
    /// CI runner's) anti-aliases a few dozen pixels differently, which no
    /// per-pixel tolerance absorbs safely. So with `SLAQ_SNAPSHOT_RENDER_ONLY`
    /// set (the CI workflow sets it) every snapshot is still **rendered**, so a
    /// panic or a failed layout assertion still fails, but its pixels are not
    /// compared. Locally the comparison always runs.
    fn snapshot<S>(h: &egui_kittest::Harness<'_, S>, name: &str) {
        if std::env::var_os("SLAQ_SNAPSHOT_RENDER_ONLY").is_some() {
            let _ = egui_kittest::wgpu::TestRenderer::new().render(h);
            return;
        }
        h.wgpu_snapshot(name);
    }

    /// [`the_harness`] in a given UI language and pixel density.
    fn harness_in<'a>(
        locale: &'static str,
        size: egui::Vec2,
        pixels_per_point: f32,
        build: impl FnMut(&egui::Context) + 'a,
    ) -> egui_kittest::Harness<'a, bool> {
        let mut build = build;
        egui_kittest::Harness::builder()
            .with_size(size)
            .with_pixels_per_point(pixels_per_point)
            .build_state(
                move |ctx, ready: &mut bool| {
                    // Pin the UI language. `rust_i18n` keeps the locale in a
                    // process-global that other tests also set (always to
                    // "it"), so pinning the same value keeps a snapshot from
                    // flipping its labels when the suite runs in parallel.
                    rust_i18n::set_locale(locale);
                    if !*ready {
                        install_fonts(ctx);
                        install_style(ctx);
                        // Snapshots must show the settled UI. egui *fades*
                        // windows and areas in, and a mid-fade capture is
                        // translucent — the conversation ghosted through every
                        // popover and dialog. Zeroing the animation time makes
                        // the first drawn frame final. The app keeps its motion.
                        ctx.style_mut(|s| s.animation_time = 0.0);
                        *ready = true;
                        return;
                    }
                    build(ctx);
                },
                false,
            )
    }

    /// A small harness for a representative widget slice.
    fn harness<'a>(build: impl FnMut(&egui::Context) + 'a) -> egui_kittest::Harness<'a, bool> {
        the_harness(egui::vec2(720.0, 460.0), build)
    }

    /// A harness at the size `run` opens the real window with.
    fn app_harness<'a>(build: impl FnMut(&egui::Context) + 'a) -> egui_kittest::Harness<'a, bool> {
        the_harness(egui::vec2(1200.0, 820.0), build)
    }

    /// A `Gui` over the offline demo workspace, with the runtime kept alive.
    ///
    /// The runtime is single-threaded and never driven: background tasks
    /// (e.g. image downloads) are queued but never polled, so a snapshot is
    /// deterministic and never touches the network.
    fn demo_gui() -> (tokio::runtime::Runtime, Gui) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let app = App::demo(None);
        let gui = Gui::new(app, None, rt.handle().clone());
        (rt, gui)
    }

    /// Mutates the demo's application state before a frame is drawn.
    fn with_state(gui: &Gui, f: impl FnOnce(&mut AppState)) {
        let mut s = gui.app.state().try_write().expect("state not locked");
        f(&mut s);
    }

    /// Fails if any column of the rendered window is left unpainted
    /// (transparent) from top to bottom.
    ///
    /// The headless renderer clears the surface transparent and every panel is
    /// drawn opaque on top, so the only way a whole column stays transparent is
    /// a strip no panel painted — exactly the gap that shows between the side
    /// panel and the central panel when a child is wider than its panel. Text
    /// and antialiased edges never leave a column transparent their whole
    /// height, so this has no false positives.
    #[track_caller]
    fn assert_no_unpainted_column(image: &image::RgbaImage, open_thread: bool) {
        let (w, h) = image.dimensions();
        let mut unpainted = Vec::new();
        for x in 0..w {
            let transparent = (0..h).filter(|&y| image.get_pixel(x, y)[3] < 250).count() as u32;
            if transparent * 10 >= h * 9 {
                unpainted.push(x);
            }
        }
        assert!(
            unpainted.is_empty(),
            "unpainted column(s) between the panels (thread_open={open_thread}): \
             x = {unpainted:?} are transparent top-to-bottom. A sidebar (or \
             thread) row wider than its panel widens the panel's frame, so the \
             central panel starts past it and the strip in between is never \
             painted. Keep every row within the panel width.",
        );
    }

    /// The bold family must be available to the first drawn frame: this guards
    /// the harness ordering (fonts installed before drawing, or the first bold
    /// run panics).
    #[test]
    fn the_bold_family_is_available() {
        let mut h = harness(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label(egui::RichText::new("ciao").family(egui::FontFamily::Name("bold".into())));
            });
        });
        h.run();
        snapshot(&h, "bold_family");
    }

    #[test]
    fn theme_and_widgets() {
        let mut h = harness(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("slaq");
                ui.label("Testo normale, con *grassetto*, `codice` e :rocket:.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let _ = ui.button("Info");
                    let _ = ui.button("Chiama (F2)");
                    let _ = selectable(ui, true, "engineering");
                });
                ui.add_space(6.0);
                egui::Frame::none()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(6.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.monospace("fn main() {\n    println!(\"ciao\");\n}");
                    });
            });
        });
        h.run();
        snapshot(&h, "theme_and_widgets");
    }

    /// The exact bug we fixed: a content-sized `SidePanel` must not grow across
    /// frames when its content fits.
    #[test]
    fn thread_panel_does_not_grow() {
        fn row(ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
                paint_initials(ui.painter(), r, "U_MARCO", "Marco Bianchi", 8.0);
                ui.label("Marco Bianchi");
            });
        }
        let mut h = harness(|ctx| {
            egui::SidePanel::right("thread")
                .default_width(380.0)
                .show(ctx, |ui| {
                    ui.heading("Thread");
                    ui.separator();
                    row(ui);
                    row(ui);
                });
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("conversation");
            });
        });
        h.run();
        let first = h.ctx.available_rect().width();
        h.run();
        let second = h.ctx.available_rect().width();
        assert!(
            (first - second).abs() < 1.0,
            "the central area changed width between frames ({first} -> {second}): \
             a content-sized panel grew"
        );
    }

    // ---- the real screens, drawn from the offline demo workspace ----

    /// An attachment with nothing to show is not drawn at all. Its colour bar
    /// on its own was a stray 3×14 coloured line hanging under the message —
    /// the reported "side line (orange) at the bottom of the message". An app's
    /// card can arrive with only a `color`, its fields coming to us as the
    /// message text, so there is genuinely nothing to paint.
    #[test]
    fn an_empty_attachment_draws_no_bar() {
        use crate::app::Attachment;
        let bar_color = parse_hex("e8912d").expect("the demo's attachment colour");
        let mk = |title: &str, text: &str| Attachment {
            color: "e8912d".into(),
            title: title.into(),
            title_link: String::new(),
            text: text.into(),
            thumb: String::new(),
            image: String::new(),
        };
        // (label, title, text, must the bar be painted?)
        let cases = [
            ("only a colour", "", "", false),
            ("a title", "Titolo", "", true),
            ("a body", "", "corpo", true),
        ];
        for (label, title, text, expect_bar) in cases {
            let (_rt, mut gui) = demo_gui();
            let a = mk(title, text);
            let mut h = harness(move |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    gui.attachment(ui, &a, 400.0);
                });
            });
            h.run();
            // The bar is `ATTACHMENT_STRIPE_W` wide; nothing else paints that
            // colour.
            let bar = h.output().shapes.iter().any(|cs| {
                matches!(&cs.shape,
                    egui::Shape::Rect(r)
                        if r.fill == bar_color && r.rect.width() <= ATTACHMENT_STRIPE_W)
            });
            assert_eq!(
                bar, expect_bar,
                "case `{label}`: the colour bar was drawn={bar}, expected {expect_bar}"
            );
        }
    }

    /// The main window: sidebar, conversation, composer (thread closed).
    #[test]
    fn main_conversation() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "main_conversation");
    }

    /// The same window with the thread panel open (the demo seeds one in
    /// #engineering).
    #[test]
    fn thread_panel() {
        let (_rt, mut gui) = demo_gui();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "thread_panel");
    }

    /// A date divider heads each new local day in the conversation, like the
    /// official client. Two messages more than a day apart (so the boundary
    /// holds in any time zone) show a header before each, with the second one
    /// sitting *between* the two messages.
    #[test]
    fn day_divider_between_days() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.messages_channel = Some("C_ENG".to_string());
            s.messages = vec![
                msg("1700000000.000100", "Primo giorno: apro il discorso."),
                msg("1700100000.000100", "Il giorno dopo: ci torno su."),
            ];
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "day_divider");
    }

    /// No message body may spill past the right edge of its column, whatever
    /// the body: a long line, a mention followed by more text, a long
    /// inline-code run, a long link. Each is rendered into a fixed-width column
    /// (as `message` does with `allocate_ui`) and its content must stay inside
    /// it — the same column constraint whose violation makes text run off the
    /// right of the conversation. Inline code was the worst offender: it laid
    /// out at `f32::INFINITY`, so a long token never wrapped and overflowed.
    #[test]
    fn no_body_kind_spills_past_its_column() {
        use crate::app::mrkdwn::Span;
        const COL: f32 = 300.0;
        let cases: Vec<(&str, Vec<Span>)> = vec![
            ("long line", vec![Span::Text("parola ".repeat(60))]),
            (
                "mention then text",
                vec![
                    Span::User("U_ME".into()),
                    Span::Text(format!(" {}", "testo ".repeat(40))),
                ],
            ),
            (
                "long inline code",
                vec![Span::Code(
                    "un_token_di_codice_molto_lungo_senza_spazi_che_deve_andare_a_capo_nel_pannello"
                        .into(),
                )],
            ),
            (
                "long link",
                vec![Span::Link {
                    url: "https://example.com/x".into(),
                    text:
                        "https://example.com/una/url/molto/lunga/senza/spazi/che/deve/wrappare/qui"
                            .into(),
                }],
            ),
        ];
        for (label, spans) in cases {
            let (_rt, mut gui) = demo_gui();
            let col_left = std::rc::Rc::new(std::cell::Cell::new(0.0_f32));
            let content_right = std::rc::Rc::new(std::cell::Cell::new(0.0_f32));
            let (cl, cr) = (col_left.clone(), content_right.clone());
            let mut h = harness(move |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::Frame::none()
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            let inner = ui.allocate_ui(egui::vec2(COL, 0.0), |ui| {
                                cl.set(ui.max_rect().left());
                                gui.spans(ui, &spans);
                            });
                            cr.set(inner.response.rect.right());
                        });
                });
            });
            h.run();
            assert!(col_left.get() > 0.0, "case `{label}`: the harness never ran");
            assert!(
                content_right.get() <= col_left.get() + COL + 1.0,
                "body kind `{label}` ran past its column: content reaches \
                 x={} but the column ends at x={}",
                content_right.get(),
                col_left.get() + COL,
            );
        }
    }

    /// The day divider must fit *exactly* inside its column — never a pixel
    /// wider. The first version built the row from three separate `allocate`s
    /// with `add_space` between them, and egui adds `item_spacing` between
    /// allocations in a row: the divider came out `2×item_spacing` wider than
    /// the column. In the conversation's `auto_shrink([false, false])` scroll
    /// area that surplus does not shrink back — it ratchets the column wider
    /// every frame until the message text runs off the right edge. This guards
    /// against any divider layout that overflows its column (the B15 regression).
    #[test]
    fn the_day_divider_fits_its_column() {
        const COL: f32 = 300.0;
        let col_left = std::rc::Rc::new(std::cell::Cell::new(0.0_f32));
        let content_right = std::rc::Rc::new(std::cell::Cell::new(0.0_f32));
        let (cl, cr) = (col_left.clone(), content_right.clone());
        let mut h = harness(move |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let inner = ui.allocate_ui(egui::vec2(COL, 0.0), |ui| {
                    cl.set(ui.max_rect().left());
                    day_divider(ui, "1700000000.000100");
                });
                cr.set(inner.response.rect.right());
            });
        });
        h.run();
        assert!(col_left.get() > 0.0, "the harness never ran");
        assert!(
            content_right.get() <= col_left.get() + COL + 1.0,
            "the day divider is {:.1}px wider than its column: a surplus here \
             ratchets the whole list wider under `auto_shrink`",
            content_right.get() - (col_left.get() + COL),
        );
    }

    /// The sidebar keeps the width it is given and does not balloon to its
    /// widest channel name. It was pinned with `exact_width` precisely because a
    /// long name used to drag it wider (and leave an empty strip); now it is
    /// resizable within bounds, so this guards that a long name in the demo does
    /// not push the panel past its width — the reason the pin existed.
    #[test]
    fn the_sidebar_keeps_its_width() {
        let (_rt, mut gui) = demo_gui();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        h.run();
        // The central area begins at the sidebar's right edge, so its left is
        // the sidebar width.
        let width = h.ctx.available_rect().left();
        assert!(
            (crate::app::SIDEBAR_WIDTH_MIN..=crate::app::SIDEBAR_WIDTH_MAX).contains(&width),
            "the sidebar width {width} left its bounds \
             [{}, {}]",
            crate::app::SIDEBAR_WIDTH_MIN,
            crate::app::SIDEBAR_WIDTH_MAX
        );
        assert!(
            width <= crate::app::SIDEBAR_WIDTH_DEFAULT + 4.0,
            "the sidebar ballooned to {width}px, past its {}px default: a channel \
             name is forcing the panel wider instead of truncating",
            crate::app::SIDEBAR_WIDTH_DEFAULT
        );
    }

    /// The huddle participants strip must always show one full row of tiles.
    /// Before the fix the strip panel had no minimum height and settled shorter
    /// than a tile (64px against a 131px row), so the bottom tile was cut —
    /// visible whenever a screen is shared, as the strip is then on screen.
    #[test]
    fn huddle_participants_fit_the_strip() {
        let (_rt, mut gui) = demo_gui();
        let people: Vec<String> = vec!["U_MARCO".into(), "U_ANA".into()];
        let fit = std::rc::Rc::new(std::cell::Cell::new((0.0_f32, 0.0_f32)));
        let f = fit.clone();
        // A short huddle window, the reported case.
        let mut h = the_harness(egui::vec2(640.0, 300.0), move |ctx| {
            f.set(gui.huddle_people_strip(ctx, &people));
        });
        for _ in 0..4 {
            h.run();
        }
        let (needed, shown) = fit.get();
        assert!(needed > 0.0, "the strip never ran");
        assert!(
            needed <= shown + 0.5,
            "the participant tiles are clipped: one row needs {needed} px but the strip shows {shown} px"
        );
    }

    /// A span that wraps while it is **not** at the start of its row used to
    /// land its first row *left* of the text column: egui gives such a `Label` a
    /// negative first-row indentation (`Label::layout_in_ui` + the wrapping
    /// `horizontal_wrapped`), so the row — and the whole list region — grew to
    /// the left and the following rows lost their margin (clipped avatars). A
    /// token only slightly wider than the column triggers it (a much wider one
    /// is moved to a fresh row by egui, which is why a plain long token did not
    /// reproduce it). The fix starts such a span on a fresh row; assert the row
    /// never extends left of the panel.
    #[test]
    fn a_wrapping_span_does_not_extend_left_of_the_column() {
        let (_rt, mut gui) = demo_gui();
        let spans: Vec<crate::app::mrkdwn::Span> = vec![
            crate::app::mrkdwn::Span::Text("Salve, ".into()),
            crate::app::mrkdwn::Span::Text(format!(" {}", "_".repeat(78))),
        ];
        let panel_left = std::rc::Rc::new(std::cell::Cell::new(0.0_f32));
        let row_left = std::rc::Rc::new(std::cell::Cell::new(f32::MAX));
        let (pl, rl) = (panel_left.clone(), row_left.clone());
        let mut h = harness(move |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                pl.set(ui.max_rect().left());
                // The conversation's inset Frame + a message row + its body.
                let frame = egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                    .show(ui, |ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(550.0, 0.0),
                            egui::Layout::left_to_right(egui::Align::TOP),
                            |ui| {
                                ui.allocate_ui(egui::vec2(496.0, 0.0), |ui| {
                                    ui.vertical(|ui| {
                                        ui.allocate_ui(egui::vec2(496.0, 0.0), |ui| {
                                            gui.spans(ui, &spans);
                                        });
                                    });
                                });
                            },
                        );
                    });
                rl.set(frame.response.rect.left());
            });
        });
        h.run();
        assert!(
            panel_left.get() > 0.0,
            "the harness never ran the closure (panel_left was not captured)"
        );
        assert!(
            row_left.get() >= panel_left.get(),
            "a wrapping span extended left of the column: the row starts at \
             x={} while the panel starts at x={}",
            row_left.get(),
            panel_left.get()
        );
    }

    /// Losing focus in the middle of a sentence, for no reason. The composer's
    /// `TextEdit` id has to be stable frame to frame, or egui forgets the focus:
    /// anything that appears **above** the composer (the "X is typing…" line, the
    /// edit banner, a mod's action row) shifts the auto ids of everything after
    /// it — the composer included.
    #[test]
    fn the_composer_keeps_focus_when_a_row_appears_above_it() {
        let (_rt, gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut ready = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 820.0))
            .with_pixels_per_point(1.0)
            .build_state(
                move |ctx, gui: &mut Gui| {
                    rust_i18n::set_locale("it");
                    if !ready {
                        install_fonts(ctx);
                        install_style(ctx);
                        ready = true;
                        return;
                    }
                    gui.draw(ctx);
                },
                gui,
            );
        // One frame so the composer allocates its id and the per-conversation
        // draft is bound; only then is it focused.
        h.run();
        let id = h.state().composer_id.expect("the composer allocated its id");
        h.ctx.memory_mut(|m| m.request_focus(id));
        h.run();
        assert!(
            h.ctx.memory(|m| m.has_focus(id)),
            "the composer starts out focused"
        );

        // Someone in the channel starts typing: the typing line appears above
        // the composer, pushing every widget after it one slot down.
        let channel = h.state().current_channel().expect("a demo channel");
        with_state(h.state(), |s| {
            let mut typing = (*s.typing).clone();
            typing.insert(
                channel,
                std::iter::once((
                    "U_ANA".to_string(),
                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                ))
                .collect(),
            );
            s.typing = std::sync::Arc::new(typing);
        });
        h.run();

        let after = h.state().composer_id.expect("the composer allocated its id");
        assert_eq!(
            after, id,
            "the composer's `TextEdit` id changed when the typing row appeared: \
             egui drops the focus mid-sentence"
        );
        assert!(
            h.ctx.memory(|m| m.has_focus(after)),
            "the composer lost the focus when the typing row appeared"
        );
    }

    /// The same bug as the composer, in the top-bar message search: the clear
    /// "×" is drawn **before** the field and appears as soon as there is text to
    /// clear, which pushed the field's auto id down a slot and dropped the focus
    /// after the very first letter — the field took one character per click.
    #[test]
    fn the_message_search_keeps_focus_after_the_first_letter() {
        let (_rt, gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut ready = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 820.0))
            .with_pixels_per_point(1.0)
            .build_state(
                move |ctx, gui: &mut Gui| {
                    rust_i18n::set_locale("it");
                    if !ready {
                        install_fonts(ctx);
                        install_style(ctx);
                        ready = true;
                        return;
                    }
                    gui.draw(ctx);
                },
                gui,
            );
        h.run();
        // Focus the field the way a click does, through the accessibility tree,
        // so the test does not lean on the widget's internal id. The top-bar
        // field is the `TextInput` in the top 47px (the sidebar one is below).
        use egui_kittest::kittest::Queryable;
        h.get_by(|n| {
            n.role() == egui::accesskit::Role::TextInput
                && n.raw_bounds().map(|b| b.y0 < 47.0).unwrap_or(false)
        })
        .simulate_click();
        h.run();

        let type_char = |h: &mut egui_kittest::Harness<'_, Gui>, c: &str| {
            h.input_mut().events.push(egui::Event::Text(c.to_string()));
            h.run();
        };
        type_char(&mut h, "a");
        assert_eq!(h.state().msg_search, "a", "the first letter went in");
        // The "×" is on screen now, one slot before the field. A second letter
        // must still land: before the fix the field had already lost the focus.
        type_char(&mut h, "b");
        assert_eq!(
            h.state().msg_search,
            "ab",
            "the second letter was dropped: the search field lost the focus when \
             the clear button appeared"
        );
    }

    /// A long post in the main composer must not grow the box: it is bounded and
    /// scrolls, so the buttons that anchor to its bottom stay on the panel. Before
    /// the fix the box grew with the text and pushed Send off the panel.
    #[test]
    fn composer_long_text() {
        let (_rt, mut gui) = demo_gui();
        let long: String = (1..=40)
            .map(|i| format!("riga {i} di un messaggio molto lungo"))
            .collect::<Vec<_>>()
            .join("\n");
        // The composer syncs its text from the open conversation on the first
        // frame (per-conversation drafts), so set ours *after* that frame or it is
        // immediately cleared.
        let mut seeded = false;
        let mut h = app_harness(move |ctx| {
            gui.draw(ctx);
            if !seeded {
                seeded = true;
                gui.input = long.clone();
            }
        });
        h.run();
        snapshot(&h, "composer_long_text");
    }

    /// The same for the (narrow) thread composer, where a long reply wraps into
    /// more lines and used to push Send off the panel first.
    #[test]
    fn thread_composer_long_text() {
        let (_rt, mut gui) = demo_gui();
        // A **single** long line that wraps in the narrow panel: counting `\n`
        // would miss this one, which is exactly why the height is measured on the
        // laid-out galley rather than on `lines()`.
        gui.thread_input = "una risposta molto lunga che non contiene alcun a capo ma che \
            nel pannello stretto del thread va comunque a capo su molte righe: e' proprio \
            il caso in cui il campo cresceva oltre il pannello, il bottone di invio finiva \
            fuori dallo schermo e non si riusciva piu' a mandare il messaggio. Aggiungo \
            ancora un po' di testo per superare il tetto di otto righe e verificare che il \
            campo si fermi, scorra al suo interno e lasci il bottone di invio ben visibile \
            in fondo alla riga."
            .to_string();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "thread_composer_long_text");
    }

    /// A mod decoration must render under the right message in the timeline, and
    /// only for its own channel.
    #[test]
    fn decorations_in_conversation() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            let chan = s
                .channels
                .get(s.selected_channel)
                .map(|c| c.id.clone())
                .unwrap_or_default();
            s.messages_channel = Some(chan.clone());
            let mut decos: std::collections::HashMap<
                String,
                std::collections::HashMap<String, Vec<String>>,
            > = std::collections::HashMap::new();
            if let Some(ts) = s
                .messages
                .iter()
                .rev()
                .find(|m| !m.text.is_empty())
                .map(|m| m.ts.clone())
            {
                decos.insert(
                    chan,
                    std::collections::HashMap::from([(
                        ts.clone(),
                        vec!["traduzione: ciao".to_string()],
                    )]),
                );
                // The same ts under another channel must not render.
                decos.insert(
                    "C_OTHER".to_string(),
                    std::collections::HashMap::from([(ts, vec!["NON deve apparire".to_string()])]),
                );
            }
            assert!(
                decos.values().any(|m| !m.is_empty()),
                "the demo must seed a text message to decorate"
            );
            s.decorations = Arc::new(decos);
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "decorations_in_conversation");
    }

    /// A mod decoration must render under the right reply in the thread panel.
    #[test]
    fn decorations_in_thread() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            let tc = s.thread_channel_id.clone().unwrap_or_default();
            let mut decos: std::collections::HashMap<
                String,
                std::collections::HashMap<String, Vec<String>>,
            > = std::collections::HashMap::new();
            if let Some(ts) = s
                .thread_messages
                .iter()
                .rev()
                .find(|m| !m.text.is_empty())
                .map(|m| m.ts.clone())
            {
                decos.insert(
                    tc,
                    std::collections::HashMap::from([(
                        ts.clone(),
                        vec!["traduzione: ciao".to_string()],
                    )]),
                );
                // The same ts under another channel must not render in the thread.
                decos.insert(
                    "C_OTHER".to_string(),
                    std::collections::HashMap::from([(ts, vec!["NON deve apparire".to_string()])]),
                );
            }
            assert!(
                decos.values().any(|m| !m.is_empty()),
                "the demo must seed a thread reply to decorate"
            );
            s.decorations = Arc::new(decos);
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "decorations_in_thread");
    }

    /// No unpainted column may separate the panels.
    ///
    /// This is the layout bug that has come back more than once: a sidebar row
    /// (a DM/MPIM name plus its badge) wider than the panel widened the side
    /// panel's frame, so the central panel began past it and the strip between
    /// them was left unpainted — a white (or black) column between the chat
    /// list and the chat.
    ///
    /// Regenerating a golden snapshot would just bake the gap in, so this does
    /// not compare against one: it renders the app headless and reads the
    /// pixels. It covers both boundaries, the sidebar/chat one and — with the
    /// thread open — the chat/thread one.
    #[test]
    fn panels_leave_no_unpainted_column() {
        // The panel boundaries, plus the sidebar filter drawn with its clear
        // "x": a row wider than the panel would leave a gap just the same.
        for (open_thread, filter) in [(false, false), (true, false), (false, true)] {
            let (_rt, mut gui) = demo_gui();
            if !open_thread {
                with_state(&gui, |s| {
                    s.thread_parent_ts = None;
                    s.thread_messages.clear();
                });
            }
            if filter {
                gui.search = "e".to_string();
            }
            let mut h = app_harness(move |ctx| gui.draw(ctx));
            h.run();
            let image = egui_kittest::wgpu::TestRenderer::new().render(&h);
            assert_no_unpainted_column(&image, open_thread);
        }
    }

    /// The top bar must be tall enough to hold the profile button whole.
    ///
    /// It once used egui's default 2px vertical margin, so the 28px avatar and
    /// the bell badge sat flush against the panel edges and looked clipped.
    /// This asserts the profile button sits inside the bar with room above and
    /// below, which a bare-bones bar fails.
    #[test]
    fn top_bar_fits_the_profile() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let (_rt, gui) = demo_gui();
        // Shared so the frame can drive it while the test still reads the
        // rects it recorded.
        let gui = Rc::new(RefCell::new(gui));
        let drawn = gui.clone();
        let mut h = app_harness(move |ctx| drawn.borrow_mut().draw(ctx));
        h.run();
        let gui = gui.borrow();
        let (bar, profile) = (gui.top_rect, gui.profile_rect);
        assert!(
            bar.height() > 0.0 && profile.height() > 0.0,
            "the top bar was not drawn"
        );
        let margin = 3.0;
        assert!(
            profile.top() - bar.top() >= margin && bar.bottom() - profile.bottom() >= margin,
            "the profile button ({profile:?}, h={:.0}) does not fit inside the \
             {:.0}px top bar ({bar:?}) with {margin}px to spare: it is clipped. \
             Give the top panel more vertical margin (it defaults to 2px).",
            profile.height(),
            bar.height(),
        );
    }

    /// Ctrl+K puts the caret in the sidebar filter — the quick channel
    /// switcher: type a name and the list narrows to matching conversations.
    #[test]
    fn ctrl_k_focuses_the_sidebar_filter() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        let id = egui::Id::new("sidebar_search");
        assert!(!h.ctx.memory(|m| m.has_focus(id)), "focus starts elsewhere");

        // Ctrl+K. The modifiers live on the input, the key on the events.
        let mods = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        {
            let input = h.input_mut();
            input.modifiers = mods;
            for pressed in [true, false] {
                input.events.push(egui::Event::Key {
                    key: egui::Key::K,
                    pressed,
                    repeat: false,
                    modifiers: mods,
                    physical_key: None,
                });
            }
        }
        h.run();
        assert!(
            h.ctx.memory(|m| m.has_focus(id)),
            "Ctrl+K must put the caret in the sidebar filter (quick switcher)"
        );
    }

    /// Every clickable widget must change the pointer, however it was built:
    /// egui does it only for some widgets, so `enforce_pointer_cursor` (called
    /// at the end of `draw`) guarantees the rest. This is the guard: a button, a
    /// selectable row and a bare clickable label must all show the pointing
    /// hand, and a plain label must not.
    #[test]
    fn clickable_widgets_show_the_pointing_hand() {
        use egui::CursorIcon;

        // Record each widget's rect, so we know where to point the mouse.
        type Rects = std::rc::Rc<std::cell::RefCell<Vec<(&'static str, egui::Rect)>>>;
        let rects: Rects = Default::default();
        let recorder = rects.clone();
        let mut h = harness(move |ctx| {
            recorder.borrow_mut().clear();
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut note = |name: &'static str, r: egui::Response| {
                    recorder.borrow_mut().push((name, r.rect));
                };
                note("button", ui.button("bottone"));
                note("selectable", selectable(ui, false, "riga"));
                note(
                    "label",
                    ui.add(
                        egui::Label::new("manina")
                            .sense(egui::Sense::click())
                            .selectable(false),
                    ),
                );
                note("plain", ui.label("non cliccabile"));
                note(
                    "inert",
                    ui.add(egui::Label::new("inerte").selectable(false)),
                );
                enforce_pointer_cursor(ctx);
            });
        });
        h.run();

        let center = |name: &str| {
            rects
                .borrow()
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, r)| r.center())
                .unwrap_or_else(|| panic!("widget `{name}` was not drawn"))
        };
        let cursor_after_hover = |h: &mut egui_kittest::Harness<'_, bool>, name: &str| {
            h.input_mut()
                .events
                .push(egui::Event::PointerMoved(center(name)));
            h.step();
            h.output().platform_output.cursor_icon
        };

        assert_eq!(
            cursor_after_hover(&mut h, "button"),
            CursorIcon::PointingHand,
            "a button must show the pointing hand"
        );
        assert_eq!(
            cursor_after_hover(&mut h, "selectable"),
            CursorIcon::PointingHand,
            "a selectable row must show the pointing hand"
        );
        assert_eq!(
            cursor_after_hover(&mut h, "label"),
            CursorIcon::PointingHand,
            "a clickable label must show the pointing hand (the safety net)"
        );
        assert_eq!(
            cursor_after_hover(&mut h, "plain"),
            CursorIcon::Text,
            "selectable text keeps the I-beam, not the pointing hand"
        );
        assert_eq!(
            cursor_after_hover(&mut h, "inert"),
            CursorIcon::Default,
            "a label that is neither clickable nor selectable shows nothing"
        );
    }

    /// A channel name too long for the sidebar is truncated to keep the row inside
    /// the panel, so hovering it must reveal the full name — there used to be no
    /// tooltip at all, and the visible text is the cut one.
    #[test]
    fn a_truncated_channel_name_is_revealed_on_hover() {
        use egui_kittest::kittest::Queryable;
        let (_rt, mut gui) = demo_gui();
        let full = "announcements-europe-and-more";
        let row = super::ChannelRow {
            id: "CLONG".into(),
            name: full.into(),
            is_dm: false,
            unread: false,
            mentions: 0,
            user: None,
        };
        let mut h = harness(move |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_max_width(150.0); // a narrow column, like the sidebar
                gui.channel_row(ui, 0, &row);
            });
        });
        h.run();
        // The row shows the truncated name…
        let shown = h
            .get_by_label_contains("\u{2026}")
            .label()
            .unwrap_or_default();
        assert_ne!(shown, full, "the row must truncate a long name: {shown}");
        // The rest of the name (the part cut off) is nowhere in the tree…
        assert_eq!(
            h.query_all_by_label_contains("europe-and-more").count(),
            0,
            "the truncated row must not carry the rest of the name"
        );
        // …until the row is hovered, which reveals the full name (the fix: there
        // used to be no tooltip at all).
        h.get_by_label_contains("\u{2026}").hover();
        h.step();
        h.step();
        assert!(
            h.query_all_by_label_contains("europe-and-more").count() > 0,
            "hovering the truncated row must show the full name"
        );
    }

    /// Opening one centre view closes the others, and clicking the open one
    /// closes it — so Threads then Mentions leaves only Mentions.
    #[test]
    fn centre_views_are_mutually_exclusive() {
        let (rt, mut gui) = demo_gui();
        // The toggles kick off background loads; `enter` lets `tokio::spawn`
        // queue them without a runtime driving them.
        let _guard = rt.enter();
        gui.toggle_center_view(CenterView::Threads);
        assert!(gui.threads_open && !gui.mentions_open);
        gui.toggle_center_view(CenterView::Mentions);
        assert!(
            gui.mentions_open && !gui.threads_open,
            "opening mentions closes threads"
        );
        gui.toggle_center_view(CenterView::Saved);
        assert!(gui.saved_open && !gui.mentions_open);
        gui.toggle_center_view(CenterView::Saved);
        assert!(!gui.saved_open, "clicking the open one closes it");
        assert!(
            !gui.threads_open && !gui.mentions_open && !gui.scheduled_open && !gui.notif_open,
            "nothing else is left open"
        );
    }

    /// Enter in the sidebar filter opens the first match — the quick switcher's
    /// "go" — and clears the filter. With no match, the text stays.
    #[test]
    fn enter_opens_the_first_filter_match() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut ready = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 820.0))
            .with_pixels_per_point(1.0)
            .build_state(
                move |ctx, gui: &mut Gui| {
                    rust_i18n::set_locale("it");
                    if !ready {
                        install_fonts(ctx);
                        install_style(ctx);
                        ready = true;
                        return;
                    }
                    gui.draw(ctx);
                },
                gui,
            );
        h.run();
        let id = egui::Id::new("sidebar_search");

        // A query with a match (#engineering in the demo): Enter opens it and
        // the filter resets.
        h.state_mut().search = "eng".to_string();
        h.ctx.memory_mut(|m| m.request_focus(id));
        h.run();
        h.press_key(egui::Key::Enter);
        h.run();
        assert!(
            h.state().search.is_empty(),
            "Enter must open the first match and clear the filter"
        );

        // A query with no match: there is nothing to open, the text stays.
        h.state_mut().search = "zzzznomatch".to_string();
        h.ctx.memory_mut(|m| m.request_focus(id));
        h.run();
        h.press_key(egui::Key::Enter);
        h.run();
        assert_eq!(
            h.state().search,
            "zzzznomatch",
            "with no match the filter text must stay put"
        );
    }

    /// The full-window login shown while there is no usable session.
    #[test]
    fn login_screen() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| s.authenticated = false);
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "login_screen");
    }

    /// Renders `docs/screenshot.png`, the README's screenshot: the English
    /// showcase workspace (`app::demo::seed_showcase`) with the real colour
    /// emoji, at 2x density. The emoji come from Slack's public CDN, so this
    /// needs a network and is **ignored** by default. Regenerate with:
    ///
    /// `cargo test --features mods readme_screenshot -- --ignored`
    #[test]
    #[ignore]
    fn readme_screenshot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        // The GUI spawns the emoji downloads onto the ambient runtime.
        let _rt = rt.enter();
        let app = App::demo(None);
        let state = app.state().clone();
        crate::app::demo::seed_showcase(&mut state.try_write().expect("state not locked"));
        let mut gui = Gui::new(app, None, rt.handle().clone());
        let mut h = harness_in("en", egui::vec2(1440.0, 1040.0), 2.0, move |ctx| {
            gui.draw(ctx)
        });
        // Each frame asks for the emoji it draws; the downloads run while the
        // runtime is driven, then the next frame turns them into textures.
        for _ in 0..6 {
            h.run();
            rt.block_on(tokio::time::sleep(std::time::Duration::from_millis(1500)));
        }
        // Driving the runtime also ran the demo's own tasks (presence), which
        // fail without a session and overwrite the status line: restore it.
        state.try_write().expect("state not locked").status = "Northwind · launch week".to_string();
        h.run();
        let image = egui_kittest::wgpu::TestRenderer::new().render(&h);
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshot.png");
        image.save(&out).expect("save the screenshot");
        eprintln!("wrote {} ({}x{})", out.display(), image.width(), image.height());
    }

    /// The mods consent dialog: what a mod asks for, as checkboxes. Covers a
    /// `filesystem` grant (its declared slots) and the `filesystem` + `net.egress`
    /// exfiltration warning.
    #[cfg(feature = "mods")]
    #[test]
    fn mods_consent_dialog() {
        use crate::app::{ModConsentCap, ModConsentDialog, ModConsentEntry, ModConsentSlot};
        use crate::mods::capability::{Capability, ReadScope};
        use crate::mods::fs::FsAccess;
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.mods_consent = Some(ModConsentDialog {
                entries: vec![ModConsentEntry {
                    id: "dev.slaq.example".into(),
                    name: "Example mod".into(),
                    caps: vec![
                        ModConsentCap {
                            cap: Capability::MessagesRead,
                            selected: true,
                            initial: true,
                        },
                        ModConsentCap {
                            cap: Capability::StoreKv,
                            selected: false,
                            initial: false,
                        },
                        ModConsentCap {
                            cap: Capability::UiMessageDecoration,
                            selected: true,
                            initial: true,
                        },
                        ModConsentCap {
                            cap: Capability::Filesystem,
                            selected: true,
                            initial: false,
                        },
                        ModConsentCap {
                            cap: Capability::NetEgress,
                            selected: true,
                            initial: false,
                        },
                    ],
                    read_scope: Some(ReadScope::Channels {
                        channels: ["C_ENG".to_string()].into(),
                    }),
                    effective_scope: None,
                    egress: vec!["example.com".into(), "cdn.example.com".into()],
                    // The user's own choices: two channels, one manifest host
                    // unticked, and a local host added.
                    access: crate::app::ModConsentAccess {
                        user_scope: Some(ReadScope::Channels {
                            channels: ["C_ENG".to_string(), "C_GEN".to_string()].into(),
                        }),
                        egress_allowed: vec!["example.com".into()],
                        egress_added: vec!["localhost:11434".into()],
                        ..Default::default()
                    },
                    fs_slots: vec![
                        ModConsentSlot {
                            name: "inbox".into(),
                            access: FsAccess::Read,
                            mapped: Some("/home/user/docs".into()),
                        },
                        ModConsentSlot {
                            name: "outbox".into(),
                            access: FsAccess::Write,
                            mapped: None,
                        },
                    ],
                }],
                pending: true,
                cursor: 0,
            });
        });
        // The host editor open; the channel editor shows its summary.
        gui.access_ui.open.insert("dev.slaq.example/hosts".into());
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "mods_consent");
    }

    /// The consent dialog's channel/host editors: the user picks channels beyond
    /// the manifest's, unticks a manifest host and adds a local one (with the
    /// bad-host error showing). `open` is the editor to show (`channels` or
    /// `hosts`); each gets its own snapshot, as one alone fills the dialog.
    #[cfg(feature = "mods")]
    fn access_editor_snapshot(open: &str, name: &str) {
        use crate::app::{ModConsentAccess, ModConsentCap, ModConsentDialog, ModConsentEntry};
        use crate::mods::capability::{Capability, ReadScope};
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            let cap = |c| ModConsentCap {
                cap: c,
                selected: true,
                initial: true,
            };
            s.mods_consent = Some(ModConsentDialog {
                entries: vec![ModConsentEntry {
                    id: "dev.slaq.brain".into(),
                    name: "Second Brain".into(),
                    caps: vec![cap(Capability::MessagesRead), cap(Capability::NetEgress)],
                    read_scope: Some(ReadScope::Channels {
                        channels: ["C_ENG".to_string()].into(),
                    }),
                    effective_scope: None,
                    egress: vec!["api.example.com".into(), "cdn.example.com".into()],
                    access: ModConsentAccess {
                        user_scope: Some(ReadScope::Channels {
                            channels: ["C_ENG".to_string(), "D_ANA".to_string()].into(),
                        }),
                        egress_allowed: vec!["api.example.com".into()],
                        egress_added: vec!["localhost:11434".into()],
                        error: Some(
                            t!("gui.mods_access_bad_host", locale = "it", host = "http://x").to_string(),
                        ),
                        ..Default::default()
                    },
                    fs_slots: Vec::new(),
                }],
                pending: false,
                cursor: 0,
            });
        });
        gui.access_ui.open.insert(format!("dev.slaq.brain/{open}"));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, name);
    }

    #[cfg(feature = "mods")]
    #[test]
    fn mods_access_channels_editor() {
        access_editor_snapshot("channels", "mods_access_channels");
    }

    #[cfg(feature = "mods")]
    #[test]
    fn mods_access_hosts_editor() {
        access_editor_snapshot("hosts", "mods_access_hosts");
    }

    /// The "Mod updated" prompt: an updated mod whose saved secret waits for
    /// Keep/Forget, with the hosts it can reach.
    #[cfg(feature = "mods")]
    #[test]
    fn mods_secrets_prompt() {
        let (_rt, gui) = demo_gui();
        with_state(&gui, |s| {
            s.mods_secrets = vec![crate::mods::StaleSecrets {
                id: "dev.slaq.brain".into(),
                name: "Second Brain".into(),
                keys: vec!["api_key".into()],
                package: "fp".into(),
                egress: vec!["llm.example.com".into(), "localhost:11434".into()],
            }];
        });
        let mut gui = gui;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "mods_secrets");
    }

    /// The consent dialog with many mods (and many capabilities each): the list
    /// must scroll inside a bounded dialog rather than overflow the window.
    #[cfg(feature = "mods")]
    #[test]
    fn mods_consent_dialog_scrolls() {
        use crate::app::{ModConsentCap, ModConsentDialog, ModConsentEntry, ModConsentSlot};
        use crate::mods::capability::Capability;
        use crate::mods::fs::FsAccess;
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            let cap = |c, sel| ModConsentCap {
                cap: c,
                selected: sel,
                initial: sel,
            };
            let entries = (0..6)
                .map(|i| ModConsentEntry {
                    id: format!("dev.slaq.m{i}"),
                    name: format!("Mod {i}"),
                    caps: vec![
                        cap(Capability::MessagesRead, true),
                        cap(Capability::StoreKv, false),
                        cap(Capability::StoreVector, false),
                        cap(Capability::StoreSql, false),
                        cap(Capability::UiMessageDecoration, true),
                        cap(Capability::Filesystem, true),
                        cap(Capability::NetEgress, true),
                    ],
                    read_scope: None,
                    effective_scope: None,
                    egress: vec!["example.com".into()],
                    access: Default::default(),
                    fs_slots: vec![
                        ModConsentSlot {
                            name: "inbox".into(),
                            access: FsAccess::Read,
                            mapped: Some("/home/user/docs".into()),
                        },
                        ModConsentSlot {
                            name: "outbox".into(),
                            access: FsAccess::Write,
                            mapped: None,
                        },
                    ],
                })
                .collect();
            s.mods_consent = Some(ModConsentDialog {
                entries,
                pending: true,
                cursor: 0,
            });
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        // The image **snapshot** is the guard here: the Apply/Later row is the
        // dialog's only control (no keyboard dismissal, non-resizable window), and
        // a list that over-budgets the window pushes it off — the diff catches it.
        // A widget query cannot: egui reports *unclipped* AccessKit bounds, so a
        // clipped button still "exists on screen" as far as the tree is concerned.
        snapshot(&h, "mods_consent_many");
    }

    /// The "Running mods" window: a live mod with its background lane, and a
    /// stopped one (real hosts, loaded from the hello fixture).
    #[test]
    #[cfg(feature = "mods")]
    fn mods_running_window() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = std::env::temp_dir().join(format!("slaq-snap-running-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut consent = crate::mods::consent::Consent::default();
        for (dir, id, name, extra, events) in [
            (
                "a",
                "dev.slaq.brain",
                "Second Brain",
                r#","background":{"intervalSecs":30}"#,
                r#"["background.tick"]"#,
            ),
            ("b", "dev.slaq.hello", "Hello", "", "[]"),
        ] {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
            std::fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"{id}","name":"{name}","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{{"messages.read":{{"scope":"all"}}{extra}}},"events":{events}}}"#
                ),
            )
            .unwrap();
            consent.grant(
                id,
                &crate::mods::capability::Capability::ALL,
                Some(crate::mods::capability::ReadScope::All),
            );
        }
        let host = crate::mods::Host::load_root(
            &root,
            crate::mods::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(crate::mods::Directory::empty()),
            &root,
        );
        assert_eq!(host.len(), 2, "both mods load");
        assert!(host.stop("dev.slaq.hello"));
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| s.mods = Some(std::sync::Arc::new(host)));
        gui.running_open = true;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "mods_running");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mod settings form: one row per field type, a secret masked, an enum.
    #[cfg(feature = "mods")]
    #[test]
    fn mods_settings_dialog() {
        use crate::app::{ModSettingsDialog, ModSettingsPane};
        use crate::mods::settings::{Schema, Settings};
        let (_rt, mut gui) = demo_gui();
        let schema = Schema::parse(&serde_json::json!([
            { "key": "targetLang", "type": "string", "default": "it", "label": "Target language" },
            { "key": "mode", "type": "enum", "values": ["auto", "on_demand"], "default": "auto", "label": "Mode" },
            { "key": "notify", "type": "bool", "default": true, "label": "Notify" },
            { "key": "endpoint", "type": "url", "default": "http://localhost:11434/v1", "label": "Endpoint" },
            { "key": "apiKey", "type": "secret", "label": "API key" }
        ]))
        .unwrap();
        let settings = Settings::load_at(
            "dev.slaq.example",
            schema,
            std::env::temp_dir().join("slaq-snapshot-settings.json"),
        );
        let handle = crate::mods::SettingsHandle {
            id: "dev.slaq.example".into(),
            name: "Example mod".into(),
            settings: std::sync::Arc::new(std::sync::RwLock::new(settings)),
        };
        let mut pane = ModSettingsPane::from_handle(handle);
        // A set secret must render masked, never in the clear.
        if let Some(f) = pane.fields.iter_mut().find(|f| f.key == "apiKey") {
            f.text = "sk-super-secret".into();
            f.has_value = true;
        }
        with_state(&gui, |s| {
            s.mods_settings = Some(ModSettingsDialog {
                panes: vec![pane],
                tab: 0,
                cursor: 0,
                editing: false,
                error: None,
                busy: false,
                save_gen: 0,
            });
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "mods_settings");
    }

    /// The composer with a mod-declared composer action: a button row above the
    /// input (the "translate and review" flow).
    #[cfg(feature = "mods")]
    #[test]
    fn composer_actions_row() {
        use crate::app::ModComposerAction;
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.mod_composer_actions = std::sync::Arc::new(vec![ModComposerAction {
                mod_id: "dev.slaq.example".into(),
                mod_name: "Example mod".into(),
                action_id: "translate-send".into(),
                label: "Traduci e rivedi".into(),
                icon: Some("\u{1f524}".into()),
            }]);
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "composer_actions");
    }

    /// The modal dialog a mod showed (`ui.dialog`): title, blocks, actions and the
    /// always-available Dismiss.
    #[cfg(feature = "mods")]
    #[test]
    fn mod_dialog() {
        use crate::app::ModDialog;
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.mod_dialog = Some(ModDialog {
                mod_id: "dev.slaq.example".into(),
                title: "Example dialog".into(),
                blocks: std::sync::Arc::new(vec![
                    ("section".into(), "A modal a mod showed.".into()),
                    ("divider".into(), String::new()),
                    ("context".into(), "a secondary line".into()),
                ]),
                actions: std::sync::Arc::new(vec![
                    ("ok".into(), "OK".into()),
                    ("later".into(), "Later".into()),
                ]),
            });
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "mod_dialog");
    }

    /// The "find in conversation" bar, with a query that matches the demo.
    #[test]
    fn find_bar() {
        let (_rt, mut gui) = demo_gui();
        gui.find_open = true;
        gui.find = "deploy".to_string();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "find_bar");
    }

    /// The notification centre popover, anchored under the bell.
    #[test]
    fn notification_center() {
        let (_rt, mut gui) = demo_gui();
        gui.notif_open = true;
        gui.notif_rect =
            egui::Rect::from_min_size(egui::pos2(1140.0, 18.0), egui::vec2(24.0, 24.0));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "notification_center");
    }

    /// The notification centre with many entries: the panel grows to the space
    /// left below the bell (not a fixed 440 px cap) and scrolls with a solid bar,
    /// so the tail is reachable instead of clipped. The image is the guard — a
    /// widget query cannot catch clipping (egui reports unclipped bounds).
    #[test]
    fn notification_center_many() {
        let (_rt, mut gui) = demo_gui();
        gui.notif_open = true;
        gui.notif_rect =
            egui::Rect::from_min_size(egui::pos2(1140.0, 18.0), egui::vec2(24.0, 24.0));
        with_state(&gui, |s| {
            s.notifications = (0..14)
                .map(|i| crate::app::NotifEntry {
                    channel_id: format!("C{i}"),
                    channel_name: format!("canale-{i}"),
                    author: format!("Autore {i}"),
                    text: format!("notifica {i}: un testo abbastanza lungo da andare a capo"),
                    ts: format!("{i}.000000"),
                    is_dm: i % 3 == 0,
                    thread_ts: None,
                    read: i % 4 == 0,
                })
                .collect();
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "notification_center_many");
    }

    /// The panel must also grow on a **reopen**: egui keeps the popover's area
    /// state after it closes, so seeding only the first show would leave it capped
    /// at the height it had then — small panel, then the list grows while closed,
    /// then reopened cramped.
    #[test]
    fn notification_center_grows_on_reopen() {
        use std::cell::Cell;
        use std::rc::Rc;
        let (_rt, mut gui) = demo_gui();
        gui.notif_rect =
            egui::Rect::from_min_size(egui::pos2(1140.0, 18.0), egui::vec2(24.0, 24.0));
        // Open with two short entries: the panel settles small.
        gui.notif_open = true;
        with_state(&gui, |s| s.notifications = notifs(2, false));
        let frame = Rc::new(Cell::new(0usize));
        let f = frame.clone();
        let mut h = app_harness(move |ctx| {
            let n = f.get();
            f.set(n + 1);
            match n {
                // Close it…
                2 => gui.notif_open = false,
                // …grow the list while it is closed, then reopen.
                3 => {
                    let mut s = gui.app.state().try_write().expect("state not locked");
                    s.notifications = notifs(3, true);
                    drop(s);
                    gui.notif_open = true;
                }
                _ => {}
            }
            gui.draw(ctx);
        });
        for _ in 0..8 {
            h.step();
        }
        snapshot(&h, "notification_center_reopen");
    }

    /// `n` notifications, `long` ones with the multi-line text a real bot sends.
    fn notifs(n: usize, long: bool) -> Vec<crate::app::NotifEntry> {
        (0..n)
            .map(|i| crate::app::NotifEntry {
                channel_id: format!("C{i}"),
                channel_name: format!("canale-{i}"),
                author: format!("Autore {i}"),
                text: if long {
                    format!(
                        "notifica {i}: <https://github.com/acme/platform/blob/develop/services/storage-hub/migration/.manual/apply_organization_operator_policies_for_bucket_access.sh|acme-policy>[bot] requested your review on (operator-inventory): include all the necessary policies and also verify that the operator has the right permissions to run the commands"
                    )
                } else {
                    format!("notifica {i}: breve")
                },
                ts: format!("{i}.000000"),
                is_dm: false,
                thread_ts: None,
                read: false,
            })
            .collect()
    }

    /// Few entries, each **taller than the panel measured when it opened**: this
    /// is the case the fixed `max_height` never covered. An `Area` hands its
    /// content the rect it sized on the previous frame, so the list was trapped at
    /// its opening height and the tail was clipped; the panel now grows to the
    /// content (up to the space below the bell) and scrolls only past that.
    #[test]
    fn notification_center_long_entries() {
        let (_rt, mut gui) = demo_gui();
        gui.notif_open = true;
        gui.notif_rect =
            egui::Rect::from_min_size(egui::pos2(1140.0, 18.0), egui::vec2(24.0, 24.0));
        with_state(&gui, |s| {
            s.notifications = (0..3)
                .map(|i| crate::app::NotifEntry {
                    channel_id: format!("C{i}"),
                    channel_name: format!("canale-{i}"),
                    author: format!("Autore {i}"),
                    text: format!(
                        "notifica {i}: <https://github.com/acme/platform/blob/develop/services/storage-hub/migration/.manual/apply_organization_operator_policies_for_bucket_access.sh|acme-policy>[bot] requested your review on (operator-inventory): include all the necessary policies and also verify that the operator has the right permissions to run the commands"
                    ),
                    ts: format!("{i}.000000"),
                    is_dm: i % 3 == 0,
                    thread_ts: None,
                    read: i % 4 == 0,
                })
                .collect();
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "notification_center_long_entries");
    }

    /// The profile popover, anchored under the avatar button.
    #[test]
    fn profile_popover() {
        let (_rt, mut gui) = demo_gui();
        gui.profile_open = true;
        gui.profile_rect =
            egui::Rect::from_min_size(egui::pos2(1080.0, 18.0), egui::vec2(24.0, 24.0));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "profile_popover");
    }

    /// The settings window: language, interface scale, desktop notifications
    /// and the version, opened by the gear in the top bar.
    #[test]
    fn settings_window() {
        let (_rt, mut gui) = demo_gui();
        // The checkbox shows the live setting; pin it so a parallel test
        // flipping notifications cannot change the snapshot.
        with_state(&gui, |s| s.desktop_notifications = true);
        gui.settings_open = true;
        gui.settings_rect =
            egui::Rect::from_min_size(egui::pos2(1100.0, 18.0), egui::vec2(24.0, 24.0));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        // The "Mods…" entry is `mods`-gated, so the window differs per build;
        // keep a baseline for each so both CI builds have one.
        #[cfg(feature = "mods")]
        snapshot(&h, "settings_window_mods");
        #[cfg(not(feature = "mods"))]
        snapshot(&h, "settings_window");
    }

    /// The notification switch flips the state the panel shows (the notifier
    /// reads the same value through the runtime flag).
    #[test]
    fn settings_toggle_notifications() {
        let (rt, gui) = demo_gui();
        let _guard = rt.enter();
        with_state(&gui, |s| s.desktop_notifications = true);
        assert!(gui.app.state().try_read().unwrap().desktop_notifications);
        gui.app.set_notifications(false);
        assert!(!gui.app.state().try_read().unwrap().desktop_notifications);
        gui.app.set_notifications(true);
        assert!(gui.app.state().try_read().unwrap().desktop_notifications);
    }

    /// A failure goes both in the status line and in a toast, so an error is
    /// not just a faint line in the top bar.
    #[test]
    fn fail_sets_status_and_toast() {
        let (_rt, gui) = demo_gui();
        with_state(&gui, |s| s.fail("boom".to_string()));
        let s = gui.app.state().try_read().unwrap();
        assert_eq!(s.status, "boom");
        assert!(s.toasts.iter().any(|t| t.text == "boom" && t.error));
    }

    /// The error toast, drawn in the bottom-right corner.
    #[test]
    fn error_toast() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| s.fail("Non sei in #canale".to_string()));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "error_toast");
    }

    /// The re-login dialog: the chat stays visible (the app is still
    /// "authenticated") and the user chooses whether to sign in again, instead
    /// of being yanked to the login screen on one failed request.
    #[test]
    fn session_expired_dialog() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| s.session_expired = true);
        assert!(
            gui.app.state().try_read().unwrap().authenticated,
            "the chat is not replaced by the login screen"
        );
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "session_expired_dialog");
    }

    /// "Later" closes the dialog; it comes back on the next failure.
    #[test]
    fn session_expired_can_be_postponed() {
        let (_rt, gui) = demo_gui();
        with_state(&gui, |s| s.session_expired = true);
        gui.app.dismiss_session_expired();
        assert!(!gui.app.state().try_read().unwrap().session_expired);
    }

    /// A `#channel` reference to a conversation we are not in says so, instead
    /// of guessing or auto-joining.
    #[test]
    fn open_unknown_channel_reports_it() {
        let (rt, gui) = demo_gui();
        let _guard = rt.enter();
        gui.app
            .open_channel("C_NOPE".to_string(), "nope".to_string());
        rt.block_on(async {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        });
        let status = gui.app.state().try_read().unwrap().status.clone();
        assert!(status.contains("nope"), "status was: {status}");
    }

    /// A message authored by someone else, for seeding state.
    fn msg(ts: &str, text: &str) -> MessageEntry {
        MessageEntry {
            user: "U_ANA".into(),
            text: text.into(),
            ts: ts.into(),
            reactions: Vec::new(),
            thread_ts: None,
            reply_count: None,
            author_icon: String::new(),
            attachments: Vec::new(),
            files: Vec::new(),
            actions: Vec::new(),
            bot_id: None,
            app_id: None,
        }
    }

    /// A mention of yourself is highlighted; a mention of someone else is not.
    #[test]
    fn self_mention_is_highlighted() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.messages_channel = Some("C_ENG".to_string());
            s.messages = vec![msg(
                "1700000001.000100",
                "ciao <@U_ME> e <@U_ANA>, guardate il diff?",
            )];
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "self_mention");
    }

    /// Arrows and other symbols Noto Sans lacks must fall back to a font that
    /// has them (Hack) instead of a tofu box.
    #[test]
    fn symbols_have_a_fallback() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.messages_channel = Some("C_ENG".to_string());
            s.messages = vec![msg(
                "1700000002.000100",
                "stato: Error=7 → ReadOnly; regola error>0 || offline>0 → Error.\n\
                 frecce ← ↑ ↓, math ⇒ ≤ ≥ ≠ ∞ ≈, box ─ │ ═, geometrici ● ■.",
            )];
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "symbols");
    }

    /// The symbols the interface itself hardcodes must all have a glyph: a
    /// missing one draws an empty box. ASCII is always present, so only the
    /// rest is listed — this is what would have caught the fullwidth plus
    /// (U+FF0B), once used for the "new channel" button.
    #[test]
    fn interface_symbols_have_glyphs() {
        let (_rt, mut gui) = demo_gui();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        let missing: Vec<String> = h.ctx.fonts(|f| {
            let id = egui::FontId::proportional(14.0);
            ["\u{1f50d}", "●", "○", "✕", "×", "·", "…"]
                .iter()
                .filter(|s| !f.has_glyphs(&id, s))
                .map(|s| format!("{s:?}"))
                .collect()
        });
        assert!(
            missing.is_empty(),
            "interface symbols with no glyph (they render as boxes): {missing:?}"
        );
    }

    /// The fonts must cover the scripts and symbols the client commits to
    /// rendering; anything outside this shows as a tofu box. The list is the
    /// coverage contract: when it fails, add a font, do not trim the sample.
    #[test]
    fn text_glyphs_are_covered() {
        let (_rt, mut gui) = demo_gui();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        const SAMPLE: &str = "àèìòùéçñüöäß αβγΩ Привет \
             →⇒←↑↓↔ ≤≥≠∞≈∑√∂±×÷ ○●■□▲▼◆◇★☆ ─│═┌┐└┘ ✓✗ \
             ☀☁☂☎☺ ♥♦♣♠ ✂✈✎✦ €£¥₽₹¢ 😀👍🎉✅🔥⚠🚀";
        let missing: Vec<String> = h.ctx.fonts(|f| {
            let id = egui::FontId::proportional(14.0);
            SAMPLE
                .chars()
                .filter(|c| !c.is_whitespace() && !f.has_glyph(&id, *c))
                .map(|c| format!("U+{:04X} {c:?}", c as u32))
                .collect()
        });
        assert!(
            missing.is_empty(),
            "the fonts do not cover these characters (they render as tofu): {missing:?}"
        );
    }

    /// The search-results view (a query run from the top bar).
    #[test]
    fn search_results() {
        use crate::api::client::{SearchChannel, SearchHit};
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.searching = false;
            let hit = |ts: &str, text: &str, user: &str, thread: Option<&str>| SearchHit {
                ts: ts.into(),
                text: text.into(),
                username: None,
                user: Some(user.into()),
                permalink: String::new(),
                thread_ts: thread.map(str::to_string),
                channel: SearchChannel {
                    id: "C_ENG".into(),
                    name: "engineering".into(),
                },
            };
            s.search_results = Arc::new(vec![
                hit(
                    "1700000001.000100",
                    "il *deploy* è andato su `prod`: <https://example.com/run|run #128>",
                    "U_MARCO",
                    None,
                ),
                hit(
                    "1700000003.000300",
                    "risposta dentro il thread",
                    "U_ANA",
                    Some("1700000001.000100"),
                ),
            ]);
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "search_results");
    }

    /// The sidebar filter — the quick channel switcher — with a query: the
    /// matching conversations, the "write to" list and the clear "x".
    #[test]
    fn sidebar_filter() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.search = "e".to_string();
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "sidebar_filter");
    }

    /// The "new channel" dialog.
    #[test]
    fn new_channel_dialog() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.new_channel = Some(NewChannel {
            name: "marketing".to_string(),
            private: false,
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "new_channel_dialog");
    }

    /// The people picker for a new group DM, with a couple of people chosen.
    #[test]
    fn people_pick_dialog() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut pick = PeoplePick::default();
        pick.selected.insert("U_ANA".to_string());
        pick.selected.insert("U_MARCO".to_string());
        gui.people_pick = Some(pick);
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "people_pick_dialog");
    }

    /// The "is typing…" line above the composer.
    #[test]
    fn typing_indicator() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.typing = std::sync::Arc::new(std::collections::HashMap::from([(
                "C_ENG".to_string(),
                std::collections::HashMap::from([(
                    "U_ANA".to_string(),
                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                )]),
            )]));
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "typing_indicator");
    }

    /// The confirmation before removing a member from a channel.
    #[test]
    fn remove_member_dialog() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.kick_pending = Some(("C_ENG".into(), "U_ANA".into(), "Ana Rossi".into()));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "remove_member_dialog");
    }

    /// The "saved for later" view.
    #[test]
    fn saved_view() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.saved_open = true;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "saved_view");
    }

    /// The "pinned" tab of the saved view.
    #[test]
    fn pinned_view() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.saved_open = true;
        gui.saved_tab = SavedTab::Pinned;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "pinned_view");
    }

    /// The "Threads" inbox.
    #[test]
    fn threads_view() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.threads_open = true;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "threads_view");
    }

    /// The "Mentions" view.
    #[test]
    fn mentions_view() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.mentions_open = true;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "mentions_view");
    }

    /// The "Scheduled" view.
    #[test]
    fn scheduled_view() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        gui.scheduled_open = true;
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "scheduled_view");
    }

    /// A quote: a faint background and a left bar spanning the whole block.
    #[test]
    fn quote_style() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.messages_channel = Some("C_ENG".to_string());
            s.messages = vec![msg(
                "1700000002.000100",
                "Ecco cosa aveva detto:\n> prima riga della citazione\n> seconda riga, per vedere che la barra sia alta quanto il blocco\n> e una terza riga per sicurezza.\n\nFuori dalla citazione.",
            )];
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "quote_style");
    }

    /// The channel-info side panel: topic, purpose and the member list.
    #[test]
    fn channel_info() {
        use crate::api::client::{ChannelInfo, ChannelTopicPurpose};
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
            s.channel_info = Some(ChannelInfo {
                id: "C_ENG".into(),
                name: "engineering".into(),
                name_normalized: "engineering".into(),
                topic: ChannelTopicPurpose {
                    value: Some("Build & deploy".into()),
                    creator: None,
                    last_set: None,
                },
                purpose: ChannelTopicPurpose {
                    value: Some("Il canale del team engineering".into()),
                    creator: None,
                    last_set: None,
                },
                num_members: 42,
                is_channel: true,
                is_group: false,
                is_im: false,
                is_mpim: false,
                is_archived: false,
                is_ext_shared: false,
                is_general: false,
                is_member: true,
                user: None,
            });
            s.channel_members = vec![
                "U_ANA".into(),
                "U_MARCO".into(),
                "U_GIULIA".into(),
                "U_LUCA".into(),
            ];
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "channel_info");
    }

    /// The emoji/reaction picker popup.
    #[test]
    fn emoji_picker() {
        let (_rt, mut gui) = demo_gui();
        gui.emoji_popup = Some(EmojiPopup {
            toolbar_id: egui::Id::new("snapshot-emoji"),
            channel: "C_ENG".into(),
            ts: "1700000001.000100".into(),
            anchor: egui::Rect::from_min_size(egui::pos2(320.0, 520.0), egui::vec2(24.0, 24.0)),
        });
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "emoji_picker");
    }

    /// Editing a message replaces the composer with the edit box and a save
    /// button.
    #[test]
    fn editing_a_message() {
        let (_rt, mut gui) = demo_gui();
        gui.editing = Some((
            "1700000001.000100".to_string(),
            "Ciao! Ho aggiornato il *deploy* — bozza di modifica.".to_string(),
        ));
        let mut h = app_harness(move |ctx| gui.draw(ctx));
        h.run();
        snapshot(&h, "editing");
    }

    /// The `/` autocompletion popup. It needs the composer focused, so the
    /// first frame allocates the field's id and the test focuses it directly.
    #[test]
    fn slash_command_completion() {
        let (_rt, mut gui) = demo_gui();
        // Close the thread: its composer also stores into the shared
        // `composer_id`, and the focus would land there instead.
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut ready = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 820.0))
            .with_pixels_per_point(1.0)
            .build_state(
                move |ctx, gui: &mut Gui| {
                    // Pin the locale, as `the_harness` does: other tests set
                    // `rust_i18n`'s process-global, and the labels would flip.
                    rust_i18n::set_locale("it");
                    if !ready {
                        install_fonts(ctx);
                        install_style(ctx);
                        ready = true;
                        return;
                    }
                    gui.draw(ctx);
                },
                gui,
            );
        // One frame so the composer allocates its id and the per-conversation
        // draft is bound; only then is the text put in and the field focused.
        h.run();
        h.state_mut().input = "/rem".to_string();
        let id = h.state().composer_id.expect("composer id");
        h.ctx.memory_mut(|m| m.request_focus(id));
        h.run();
        snapshot(&h, "slash_completion");
    }

    /// Switching conversation must stash the draft and bring it back, not carry
    /// the text across.
    #[test]
    fn drafts_are_kept_per_conversation() {
        let (_rt, mut gui) = demo_gui();
        with_state(&gui, |s| {
            s.thread_parent_ts = None;
            s.thread_messages.clear();
        });
        let mut ready = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 820.0))
            .with_pixels_per_point(1.0)
            .build_state(
                move |ctx, gui: &mut Gui| {
                    rust_i18n::set_locale("it");
                    if !ready {
                        install_fonts(ctx);
                        install_style(ctx);
                        ready = true;
                        return;
                    }
                    gui.draw(ctx);
                },
                gui,
            );
        // First drawn frame: the input belongs to the selected conversation.
        h.run();
        h.state_mut().input = "bozza per engineering".to_string();

        // Switch conversation: the draft is stashed, the new one starts empty.
        set_selected(&h, 1);
        h.run();
        assert_eq!(h.state().input, "", "the draft followed the switch");
        assert_eq!(
            h.state().drafts.get("C_ENG").map(String::as_str),
            Some("bozza per engineering")
        );

        // Back: the draft returns.
        set_selected(&h, 0);
        h.run();
        assert_eq!(h.state().input, "bozza per engineering");
    }

    fn set_selected(h: &egui_kittest::Harness<'_, Gui>, index: usize) {
        let app_state = h.state().app.state().clone();
        app_state
            .try_write()
            .expect("state not locked")
            .selected_channel = index;
    }
}
