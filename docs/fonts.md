# Text and missing glyphs ("tofu")

A character the installed fonts have no glyph for is drawn as an empty box
(tofu). It can appear **anywhere** text is painted: a message, a name, a topic,
a hardcoded interface symbol — the last one is how the fullwidth plus `＋`
(U+FF0B) ended up as a box on the "new channel" button.

egui renders text through a **font family**: an ordered list of faces. A
character is looked up in the first face, then the next, until one has it; if
none does, tofu. So the fix and the prevention both live in that list.

## The chain we install

`install_fonts` (in `src/gui/mod.rs`) sets the proportional family to:

1. `noto-sans-regular` — Latin, Greek, Cyrillic, punctuation;
2. `noto-sans-symbols2` (a subset) — geometric shapes, stars, checks, dingbats;
3. egui's defaults, incl. `Hack` — arrows, math operators, box drawing;
4. `noto-emoji` — monochrome emoji.

Bold/italic are the same chain with their face first. Monospace gets the emoji
fallback too. This covers the scripts and symbols `text_glyphs_are_covered`
lists. **Not** covered, deliberately for now: CJK, Arabic, Hebrew, Devanagari
and other large scripts.

## Rules

- **Interface chrome** (buttons, badges): prefer a **painted icon** (see
  `close_button`, `notification_bell`) or **ASCII**. They can never be tofu.
  If a non-ASCII symbol is unavoidable, add it to `interface_symbols_have_glyphs`.
- **Content** (messages, names, topics): you cannot enumerate it, so rely on the
  fallback chain. When a real gap appears, **add a face** (see below); do not
  silently drop the text.

## The guards

Two tests turn "it looks fine on my machine" into something checked:

- `interface_symbols_have_glyphs` — every symbol the interface hardcodes has a
  glyph. This is what catches the `＋` class of bug.
- `text_glyphs_are_covered` — a committed sample of the scripts and symbols we
  promise to render. It is a **contract**: when it fails, add a font; do not
  trim the sample. (Verified to catch CJK/Arabic, which are outside the contract
  today.)

Both use `egui::Context::fonts` and `Fonts::has_glyph(s)`, so they read the
**real** installed fonts — no snapshot, no GPU.

## Adding a face

Drop the `.ttf` in `src/gui/data/`, `include_bytes!` it, register it in
`font_data`, and insert it in the right place of the family chain (order is the
priority). Subset large fonts to the blocks you need: the Symbols 2 face is a
91 KiB subset of a 656 KiB font. `text_glyphs_are_covered` then proves the new
coverage.

## If you must cover arbitrary scripts

Embedding every script is heavy (a CJK face alone is tens of MB). The
alternative is a **runtime system-font fallback**: scan a few standard font
directories at startup and append the faces found, *after* the embedded chain.
It does not disturb the embedded Latin rendering (and so the snapshots), but it
makes coverage depend on the machine — which is why `text_glyphs_are_covered`
must then assert only the **embedded** set, and the rest becomes best-effort.
