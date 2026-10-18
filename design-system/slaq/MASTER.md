# slaq — design system

The single source of truth for how the **GUI** looks. Colour lives in
`src/theme/mod.rs` (roles → values) and is applied in `gui::install_style`. Call
sites ask for a **role** (`pal().danger`); a raw `Color32::from_rgb(…)` outside
`src/theme/` is a bug.

Status: **dark only as the built-in**, but the palette is now a *runtime value*
(`theme::palette()` / `set_palette`), so it can be replaced: a **mod** overrides
roles declaratively through the manifest `ui.theme` key (capability `ui.theme`),
merged onto `Palette::DARK`. There is no built-in light theme and no in-app theme
picker — theming is a mod (`docs/mods/manifest-v0.md`). The host does not police
contrast in a mod theme; an unreadable one is the user's to remove.

## Identity: "Slate + Signal Indigo"

A cool slate neutral ramp with an indigo accent: calm and corporate, and
deliberately unlike Slack's aubergine-green. The identity comes mostly from the
**neutral ramp + surface hierarchy + spacing**, not from the accent alone.

Direction per the ui-ux-pro-max data: *Minimalism & Swiss* for a business
messaging tool — restrained, grid-based, high contrast, motion as feedback only.

## Colour roles

Surfaces are ordered from the page back to the front. The separation between
them is what gives the interface its hierarchy.

| Role | Dark | Used for |
|---|---|---|
| `bg_panel` | `#090C12` | sidebar, top bar, thread / channel-info panels |
| `bg_base` | `#0B0E14` | conversation surface (the page) |
| `bg_input` | `#11161F` | text fields (composer, search) |
| `bg_elevated` | `#161B24` | popovers, dialogs, menus, cards |
| `bg_hover` | `#1C2230` | hovered rows and widgets |
| `bg_active` | `#213156` | the selected conversation |
| `border_subtle` / `border_strong` | `#202836` / `#2E3A4D` | separators / widget outlines |
| `text_primary` | `#E6EAF2` | message text, names |
| `text_secondary` | `#9AA6B8` | secondary labels |
| `text_muted` | `#7E8A9B` | timestamps, hints, weak text |
| `text_on_accent` / `ink` | `#08122B` / `#14161A` | text on accent / on light chips |
| `accent` | `#6E8BFF` | brand, focus, selected, primary actions |
| `link` | `#79B8FF` | URLs, hyperlinks |
| `success` / `warning` / `danger` / `info` | `#3FB950` / `#D9A441` / `#E5534B` / `#58A6FF` | states |
| `mention_self_bg` / `_fg` | `#E5B84B` / `#1A1508` | a mention of yourself |
| `mention_other_fg` | `#E5B84B` | a mention of someone else |
| `unread_badge_bg` / `_fg` | `#E6EAF2` / `#14161A` | unread count pill |
| `danger_fill` / `on_danger` | `#C43C34` / `#FFFFFF` | mention pill, filled danger buttons |
| `reaction_bg`, `code_bg`, `quote_border` | `#1C2230`, `#161B24`, `#6B7686` | message decorations |
| `favorite` | `#E8B84B` | the favourite star (decoration, not a warning) |
| `presence_active` / `_away` / `_quiet` | `#3FB950` / `#6B7686` / `#8C8CE0` | presence marks |
| `scrim`, `danger_bg` | `#000` (+alpha), `#3A1E1E` | modal dim, error toast fill |

Avatars: the 8 pastel fills in `theme::AVATAR`, chosen by hashing the user id.

## Type & spacing

From `gui::install_style`: body 15px, small 12.5px, heading 22px, monospace 14px;
item spacing `(6,3)`, interact size `≥26px`, one corner radius (8px, windows 10px),
no size change between widget states — colour marks state. Type is Noto Sans
(regular/bold/italic), with Noto Emoji and a Symbols subset as fallback.

## Component decisions

- **Modals.** The app's own dialogs — forward, new channel, people pick, remove
  member, session-expired and the screen-share picker — are `egui::Modal`
  (`gui::modal`): the palette scrim behind, an opaque `bg_elevated` sheet, and
  input blocked to the UI underneath. Backdrop/Esc close them; the pickers and
  forms also carry a visible Cancel (the session-expired prompt offers
  Retry/Later instead). A floating `Window` did not dim what was behind it. The
  `mods`-feature dialogs are not converted yet: consent and settings use a plain
  `Window`, and `mod_dialog` is a hand-rolled `Area` with its own scrim.
- **Primary action** is `accent` + `text_on_accent`. `success`/`warning`/`danger`
  are *states*, never a button fill (the exception: `danger_fill` carries white
  text for a destructive action).
- **Count badges** are the neutral `unread_badge_*` pill; only a mention uses
  `danger_fill`. The selected conversation is `bg_active` (the sidebar scopes
  egui's selection colour to it, so text selected inside the sidebar filter
  follows that too); elsewhere the accent tint is the text-selection colour.
- **Sidebar order**: read = `text_secondary`, unread = `text_primary` bold, open
  = `text_primary`.
- **Message grouping**: same author, same *local* day, within 5 minutes share one
  header; a grouped message keeps its time on hover.
- **Icons** are painted PNGs (Lucide, ISC). Emoji are used only where the emoji
  *is* the content (body, status, reaction, picker categories).
- **Snapshots**: the test harness zeroes `animation_time` so a capture is the
  settled frame (a mid-fade window is translucent). Colour emoji are network
  images, so snapshots show the font fallback — a known, accepted limitation.

## Rules (checked)

- Two tests in `src/theme/mod.rs` enforce WCAG 2.1 AA and fail the build on a
  regression: `dark_palette_meets_wcag_aa` checks every role pair (body ≥ 4.5:1
  on the surfaces it is drawn on, UI/link/badge ≥ 3:1), and
  `weak_text_is_readable` checks egui's *derived* `weak_text_color()` (the ~60
  `ui.weak(...)` sites) on base/panel/elevated/input. They have already caught
  muted text, the mention pill, and the derived weak colour.
- No raw colour outside `src/theme/`: `no_raw_colours_outside_the_theme` in
  `gui/mod.rs` fails on egui's named colours and `Color32::from_*` constructors.
  Best-effort, not absolute: it scans only `gui/mod.rs`, and allows
  `Color32::TRANSPARENT` (a placeholder image) and `Color32::PLACEHOLDER`
  (layout); a colour in a new `gui/*.rs`, or via an alias, is out of scope.
  Slack's hex strings go through `theme::parse_hex`.
- Icons are painted or from the font, never emoji-as-icon. Exception, by
  intent: where an emoji **is** the content — a message body, a status, a
  reaction, and the emoji-picker categories — it stays a colour emoji (that is
  the data, not an affordance).
- Motion and focus: see the ui-ux-pro-max priority list (accessibility →
  interaction → layout → type/colour → motion).

## Regenerating the identity tiles

`design-system/slaq/preview/palette_preview.py` emits the HTML style tile (three
candidate identities with measured contrast); render it with
`chromium --headless --screenshot`. Pure stdlib.
