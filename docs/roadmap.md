# Roadmap

Status as of 2026-10-18 (the latest update entry is first). The order is by *how often it forces you to reopen
Slack*, not by difficulty.

Effort legend: **S** a few hours · **M** a day · **L** several days.

The phases below are kept as they were written, because they document the
reasoning. A few claims inside them are older than this header: where they have
been overtaken, an inline note says so.

## Update — 2026-10-18 — 0.7 to 0.9

- **0.7.0:** GUI. Resizable sidebar, day dividers in the timeline, animated GIFs,
  rounded badges. 0.7.1 and 0.7.2 fixed sidebar-width persistence, GIF memory
  and a flickering message toolbar.
- **0.8.0:** the mod runtime rework. Each mod runs on its own thread with no CPU
  budget, background work, cancellable requests, and a **Running mods** window
  with Stop/Reload. Details in `docs/mods/execution-model.md`.
- **0.8.1–0.8.2:** the user picks a mod's channels and hosts in the app, and a
  mod's saved secrets survive its updates.
- **0.9.0:**
  - mods can read channel history and threads (paced per mod);
  - **secrets are stored in the real OS keyring.** Before 0.9.0 the keyring
    library ran without a backend, held secrets in memory, and lost them at
    exit.
- **Mods** have their own roadmap: [`docs/mods/roadmap.md`](mods/roadmap.md).
  Published mods (Second Brain, Light theme) are on the download page.

## Update — 2026-10-13 — GUI design system · ✅ landed in 0.5.0

The interface had grown ad hoc: ~40 hardcoded colours in `gui/mod.rs` (the TUI
has its own), no token layer, and everything on egui's default dark grey.

Landed in **0.5.0** (`feat/ui-design-system`):

- **A design system.** `src/theme/mod.rs` defines every colour by **role**
  (`danger`, `bg_panel`, `mention_self_bg`, …) and one dark identity, "Slate +
  Signal Indigo". `gui::install_style` pushes it into egui's `Visuals`; call
  sites ask for `PAL.<role>` and never write a value. The source of truth is
  `design-system/slaq/MASTER.md`.
- **Enforced, not by convention.** Two tests check WCAG 2.1 AA — every role pair
  *and* egui's derived `weak_text_color()` — and
  `no_raw_colours_outside_the_theme` (best-effort: egui's named colours in
  `gui/mod.rs`) blocks a raw colour. They have caught muted text (4.20:1), the
  mention pill (3.7:1) and the derived weak colour.
- **Surface hierarchy.** Sidebar/top bar/panels sit on `bg_panel`, the
  conversation on `bg_base`; before, every surface was the same grey.
- **Hierarchy and rhythm.** Read conversations are muted, unread primary+bold,
  the open one primary; 36px avatars; a conversation inset; a hover wash.
- **Dialogs are modals** (scrim + input blocked); the app's own dialogs also
  carry a visible Cancel.
- **Message grouping** (same author, same *local* day, within 5 minutes) and a
  composer whose send is the accent primary action, with a Lucide paperclip
  (not a 📎 emoji) in both the conversation and the thread.

Still open: a built-in **light palette** (themes are a mod for now, not a
setting); an individual pass on the remaining screens (search, settings,
notification centre); focus/keyboard/reduced-motion. The `mods`-feature dialogs
still use a plain `Window`.

## Update — 2026-10-13 — Themes via mods · 0.6.0

The palette is a runtime value (`theme::palette()`/`set_palette`); a mod declares
`ui.theme` (role → `#rrggbb`, capability `ui.theme`) and the GUI merges it onto
the default, applying it at load and reverting when the mod is gone. Lenient by
design: an unknown role or bad hex is ignored, no contrast policing — an
unreadable theme is the user's to remove. So a light theme ships as a mod, not a
built-in.

## Update — 2026-10-10

Landed since the last revision:

- **Channel and group-DM management**: create (public/private), rename, topic,
  invite, remove a member, leave, archive — in the GUI and the CLI, on the
  `xoxc` token (validated against the live API).
- **Typing indicator, on receive**: `user_typing` shows "… sta scrivendo" above
  the composer and clears on its own. *Sending* it is still missing.
- **Ctrl+K** quick channel switcher, putting the caret in the sidebar filter.
- **Full names** instead of nicknames, everywhere.
- **Settings window** (gear in the top bar): language (it/en), interface scale,
  desktop notifications on/off, version.
- **Error toasts**: a failure pops in the corner, not only in the status line.
- **Clickable `#channel`** in messages.
- **Pointing hand over everything clickable**, with a guard test.
- **Screen sharing is native**: send and receive, VP8 by default.
- **Re-login on an expired session**: the chat stays on screen and a dialog
  asks *"Rientra / Più tardi"*, instead of jumping straight to the login screen
  on one failed request.
- `--version` says `slaq`, not `slack`.

Still open (roughly most to least annoying):

- **Link/media previews** — attachments already show image thumbnails, but a
  link's `image_url` (the unfurl) is not drawn.
- **Sending the typing indicator.**
- **Archive button in the GUI** (the CLI already has `channel-archive`).
- **Syntax highlighting** in code blocks (`syntect` costs a few MiB).
- **Multi-workspace** — one workspace at a time.
- **Packaging + SBOM.**
- **TUI parity** — see the note at the bottom.

---

## Phase 1 — Stop reopening Slack to work

These are the four gaps that, on their own, send you back to the official
client several times a day.

### 1.1 Backward history · ✅

Only the last 50 messages are shown (`WINDOW` in `app/mod.rs`). Scrolling to
the top does nothing.

The API already exists: `conversations_history_range(channel, limit, None, latest)` with
`latest = oldest_ts` in cache — `slack history --older` uses it. What's missing:

- `App::load_older(channel_id)`, which asks the cache for the `oldest_ts`, downloads
  and calls `upsert_messages`;
- in the GUI, intercept scrolling to the top: `ScrollArea` exposes the offset, and
  when it's zero `load_older` is called only once (a "loading in progress" flag is
  needed, otherwise it fires every frame);
- preserve the position after insertion, otherwise the view jumps.

The cache makes the second pass instant, so it should be done before the rest:
it also makes search more useful.

### 1.2 Multiline composer · ✅

Today it's `TextEdit::singleline`: no Shift+Enter, so no structured messages and
no code blocks.

- `TextEdit::multiline` with height growing up to ~6 lines;
- Enter sends, Shift+Enter adds a newline (in egui you tell them apart by reading
  `modifiers.shift` before consuming the key);
- also applies to the thread box.

### 1.3 Sending files and images · ✅

No upload method. Pasting a screenshot is half of technical conversations.

Done with the three-step flow — `files.getUploadURLExternal`, POST of the
content to the returned URL, `files.completeUploadExternal` with the channel —
verified that it accepts an `xoxc` token.

Paste from the clipboard exists (2026-10-01): egui exposes only text, so
the image is read from the system with `arboard` and a temporary PNG is saved
that enters the same upload flow. The key is intercepted on **release**:
`egui_winit` recognizes `Ctrl+V` as a clipboard command and returns without ever
emitting the key event, so nothing arrives on press.

### 1.4 Editing and deleting your own messages · ✅

`chat.update` and `chat.delete`. A typo today just stays there.

- context menu on your own message (`response.context_menu`);
- editing reuses the composer, with a status indicator;
- incoming `message_changed` / `message_deleted` events are **already
  handled** by `ws::cache_op`, so the update is free.

---

## Phase 2 — Daily friction

### 2.1 `@` autocomplete · ✅

The 438 users are already in `AppState::user_names`. Typing `@` opens a
filtered popup; choose with arrows and Enter/Tab.

In the composer you write `@Nome Cognome`; the `<@U123>` form that Slack
interprets is applied only on send (`app::resolve_mentions`). Injecting it
right away made the text unreadable and moved the cursor into the middle of the
mention. For Tab to select instead of changing focus, `lock_focus` is needed on
the `TextEdit`: egui consumes Tab at the start of the frame, before it can be
intercepted.

Same mechanics for `#` on channels, which are already in memory.

### 2.2 Message search · ✅

`search.messages`, verified with `xoxc`: 3918 results in the workspace. A results view is
needed with channel, author, date and excerpt, and jumping to the message in context — which
requires 1.1, otherwise you can only jump within the last 50.

Meanwhile the sidebar search already covers channels and people.

### 2.3 Copying the permalink · ✅

`chat.getPermalink`, or build it from team + channel + ts. Practical
prerequisite of forwarding: often pasting the link is enough.

### 2.4 Forwarding a message · ✅

Slack has no dedicated method: you send the permalink, which the recipient
sees expanded with author and text — that's what the official client does too.

---

## Phase 3 — Polish

- **Link preview image** · S — we show title and text but not
  `image_url`. The authenticated image path already exists.
- **"typing" indicator** · receive ✅ / send open — `user_typing` is handled on
  receive; sending it is not done yet.
- ~~**Presence**~~ · ✅ — see below.
- **Syntax highlighting in code blocks** · M — `syntect`
  costs a few MiB of binary: weigh whether it's worth it.
- ~~**Quick channel shortcut (Ctrl+K)**~~ · ✅

### Unread out of view · ✅

The sidebar is more than a hundred entries long: an unread channel outside the
visible part gave no signal. Now a strip above and below the list says how many
there are on that side, in red if there's a mention, and clicking takes you there
without opening — opening it would mark it as read, and someone just checking
didn't ask for that.

The rows are all drawn, even off-screen, so the position of each one is known
and the comparison with the window is exact (`split_unread`). Two non-obvious
details: the count is known only **after** drawing the list, but the strips go
above and below it — the previous frame's values are used, requesting a redraw
when they change; and the space for the bottom strip must be **reserved
beforehand**, because the list takes all the remaining height and whatever is
drawn after has no room left.

A channel inside a closed folder isn't drawn at all: the count ends up on the
folder header, which is marked in turn.

---

## Presence, status and quiet hours · ✅

Three pieces of information from three different sources, all verified against
the real API:

- **Presence**: `{"type":"presence_sub","ids":[…]}` over the WebSocket. The
  detail that cost time: it must be sent **after** the `hello` event. Before
  the handshake the gateway drops it silently — no response, no error, no
  event. The response is batched
  (`{"type":"presence_change","presence":"away","users":[…]}`) because the URL
  already carries `batch_presence_aware=1`, and a snapshot of all subscribers
  arrives right away, then the changes. `presence_query` exists but only gives
  the initial snapshot, so it's not needed. A numeric `id` on the message is
  optional.

  The subscription covers only the people in the sidebar — 43 here, not the
  438 of the workspace — and is re-sent when the list changes, because
  `presence_sub` replaces the previous one instead of adding to it.

- **Personal status**: `profile.status_emoji` and `status_text` already arrive
  with `users.list` on first load, so at zero cost; `user_change` events keep
  it updated during the day.

- **Quiet hours**: `dnd.teamInfo`, which accepts a list of users —
  `users.getPresence` ignores the plural and answers only for yourself, so it's
  not usable for the sidebar. Watch out for `dnd_enabled`: it says a schedule is
  *configured*, not that it's active right now. It counts only if the moment
  falls between `next_dnd_start_ts` and `next_dnd_end_ts` (`app::in_quiet_hours`).

In the sidebar presence takes the place of the symbol: a solid green dot if online, an empty ring if away, a moon if in quiet hours — which
wins over the rest, because that's what decides whether to message now. The status
emoji follows the name, with the full text on hover.

### Color emoji · ✅

The standard emoji were gray because `ab_glyph`, the rasterizer under
egui, reads **only monochrome outlines**: neither colored bitmap fonts
(CBDT, like `NotoColorEmoji.ttf`) nor colored vector ones (COLRv1).
No color emoji font can be loaded, whatever it is — color is only possible
through images.

Slack itself serves the images, and they're exactly the Google ones from the
official client:

    https://a.slack-edge.com/production-standard-emoji-assets/14.0/google-medium/{sequenza}@2x.png

PNG 44×44, about 3 KiB. It's the same host from which avatars and workspace
emoji are already downloaded, so no extra third party, and it reuses the on-disk
cache that never expires: each emoji is requested only once in its lifetime.

The file name is the **entire** codepoint sequence, variant selectors
included: `:female-detective:` is `1f575-fe0f-200d-2640-fe0f`, and the same
without `fe0f` doesn't exist. This is why `emoji_sequence` doesn't go through
`unicode_emoji`, which returns the reduced form for the font.

One image covers the entire sequence, so three things a font doesn't compose
solve themselves: ZWJ sequences (`:female-detective:` showed ♀), skin tones and
flags.

Out of 40 random emoji from our list, 39 are there; the missing one was
`:maracas:`, newer than the 14.0 set. The monochrome font remains as
a fallback, also for when you're offline.

All emoji go through it, including those of folders and statuses: they were
the last two places with the font glyph.

Watch out for a detail that hid the problem for a long time: downloading
happens in a separate task, and until the image is there the fallback is drawn.
Without explicitly requesting a redraw when it arrives, the fallback stays
until something else moves the interface — and it seemed that color worked for
some emoji and not others, depending on what was already in cache from a
previous session.

### Note on fonts

Egui doesn't include Noto Emoji but a **subset**: 752 emoji out of about 1900.
Measured against our map of 3670, **59%** of them were drawable — the
others turned into empty squares, and it's the same reason
`male-mechanic` appeared empty in the folder headers.

Now the whole font is bundled (`src/gui/data/`, OFL): **100%** coverage,
865 KiB in the binary and no measurable difference in memory (115 MiB
versus 116 before), because the glyph cache grows only for the ones drawn.
Color emoji fonts are not an option: they're CBDT bitmaps and `ab_glyph`
doesn't read them.

Two precautions remain: the variant selector `U+FE0F` is always removed
(on its own it produced a second small square next to the emoji), and a glyph the
font doesn't have falls back to the short name — still needed for the
workspace's custom emoji, which no font can contain.

Dots and arrows, instead, are **painted, not written**. None of the geometric
shapes is of use: neither `U+25CF` (dot), nor `U+25B2`/`U+25BC` (large
triangles), nor `U+25BE`/`U+25B8` (small ones, the folder ones), nor
`U+2191`/`U+2193` (arrows) exist in the fonts that egui loads, not even with the
whole Noto Emoji. The folder arrows had always been small squares, mistaken
for decorations. For three vertices it's not worth depending on a font.

### Bot messages · ✅

`text` is the fallback Slack sends for notifications and screen readers:
it's **flattened**, and for app messages it loses the links. The message GitHub
uses to report a review is the example — in `text` the PR title is
plain text, in `blocks` it's the link, which is the very reason for the message.
So blocks are preferred when an app composes them (`display_text`).

`rich_text` blocks are the exception: they're the rendering of what a person
wrote, and the corresponding `text` already has all the markup.


Modern integrations leave `text` empty and put everything in `blocks`, both
at the top of the message and inside attachments. We used to read only `text`
and then the attachment `fallback`, which is at the discretion of whoever
writes the bot: the one from `ci_e2e` contained `[no preview available]` for
every message, so the channel was a column of unreadable lines. Now the blocks
are read (`app::blocks_text`).

The author was "?" because those messages have neither `user` nor
`bot_profile`, only `bot_id`. The `bot_id` is kept as identity and resolved
with `bots.info`, once per bot: name and icon end up among the user
names, so the rest of the client doesn't have to know it's a bot.

Resolution must be hooked **where messages enter the state**
(`spawn_resolve_bots`), not where a conversation is opened: loading is
asynchronous, so asking right after starting it still finds the old list.
That was why the id appeared sometimes and not others, and why it stayed on
messages arriving from the WebSocket or from scrolling back.

### Dates, nested links, groups · ✅

Three defects of the same parser, found on Google Calendar messages.

`<!date^1790762400^{time}>` ended up in the `<!here>` branch and came out
raw: Slack leaves it to the client to write the date in the reader's
timezone. Now the placeholders are expanded (`{time}`, `{date}`, the `_pretty`
variants that say "today" only if it really falls there).

In the same branch the label after the bar was discarded, so
`<!subteam^S123|@devs>` showed the id instead of the group name.

There's a corollary that only shows up on a real URL: entities must be skipped
while looking for the closing delimiter. An `_` inside `utm_source`
closed the italic halfway through the address, breaking the link — it
happens on every message n8n sends, because it signs off with a link full of
`utm_` parameters. It also applies to emoji names (`:white_check_mark:`). For
code between backticks, instead, nothing is skipped: there the content is
verbatim.

The third was the deepest: emphasis contained **raw text**, not other
spans. A `*<url|testo>*` — like the title of every calendar event —
left the link markup on screen instead of the link. Now `Bold`,
`Italic` and `Strike` contain `Vec<Span>` and the rendering propagates
emphasis to what's inside. `Code` stays verbatim: there the markup is content.

---

## Notifications · ✅

The worst defect found so far, and invisible to any test that wasn't
end-to-end: **the first real notification made the client abort.**

`notify_rust` opens a synchronous D-Bus connection that internally starts its
own runtime; calling it from a tokio thread panics the process with
«Cannot start a runtime from within a runtime». It had never been
seen because `slack notify-test` runs from the command line, outside the
runtime: the test command worked and the real thing didn't.

Now it goes through a system thread (`notify::show_detached`), which has no
tokio context. `spawn_blocking` is **not** enough: those threads stay inside the
runtime context and the problem reappears identically.

In the body, mentions are written by name: `to_plain` leaves the id, because
it's the wrong level at which to know the names, but in a notification «@U01ABCDEF»
defeats half the message.

The rules remain Slack's: only a direct message, an explicit
mention or an `@here`/`@channel`/`@everyone` is notified. An ordinary message
in a channel just makes a badge.

A second defect, sneakier than the crash because it left no traces:
**without the `urgency` hint GNOME doesn't open the banner.** In the
absence of declared urgency it treats it as low priority, and low-priority
notifications only end up in the clock's drop-down list. The symptom was «you
hear the sound but see nothing». It was found by comparing line by line
what `notify-send` sends — which was visible — with our call, which
sent empty hints.

Diagnosing a notification requires looking at who actually sent it:

    dbus-monitor "interface='org.freedesktop.Notifications',member='Notify'"

The `appname` field distinguishes ours (`slack-rs`) from those of the official
client (`desktop-entry = snap.slack`) — without that check it's easy to
mistake a Slack notification for one of your own.

---

## Debt: the TUI

Everything added after the `app`/`ui`/`gui` split is **GUI
only**: folders, reactions, images, attachments, colored mrkdwn, search,
badges.

The TUI has channels, messages, threads and sending. The core is shared, so
porting a feature costs only the drawing — but there are still quite a few
features.

Three paths, to be chosen deliberately:

1. **Parity** — every feature in both. Doubles interface work
   forever.
2. **Read-only TUI** — read and reply over SSH, the rest in the GUI.
   Honest and near-zero cost; it should be written in the README.
3. **Port only the essentials** — folders, badges and colored mrkdwn, which are
   the ones you really notice missing.

Option 3 is probably the right compromise, but it's a product decision,
not a technical one.

---

## Out of scope for now

**Native calls.** ✅ They **work** as of 2026-09-30: two-party call
verified, and as of 2026-09-30 **in use**: first real work call
sustained by the client, with echo and noise cancellation, without a headset.
`F2` in the GUI joins a call. The blocker was the SRTP
encryption profile (we offered GCM first, the SFU chose it and couldn't
decrypt us). Details in `huddle-protocol.md`.

**Screen sharing is native too** (2026-10): sending in VP8
(`calls/screen_send.rs`), receiving Slack's AV1, or VP8 from other clients
(`calls/screen.rs`). The browser is no longer needed for calls or for sharing.

As of 2026-10-01 **incoming calls** are also visible: `huddle_invite`,
`huddle_invite_cancel` and the three `sh_room_*` (protocol in §11) become a
ring next to the channel in the list, a bar with Answer/Ignore, and a
system notification. Only a direct invite rings: in a channel the call
is just visible.

What follows is the description of the blocker, now **fully overtaken** —
remote audio, echo cancellation and screen sharing all work natively today — and
kept only for the record:

**(historical)** The protocol is mapped and works up to `SUBSCRIBE_ACK`
(see `huddle-protocol.md`), but still missing are: the blocker documented there,
remote audio playback, echo cancellation — and for screen
sharing a second Chime protocol never analyzed. Meanwhile `F2` opens
the huddle in the browser, where everything works, including seeing someone
else's screen. *(The last part was the prognosis at the time; it turned out
wrong: the client does have a video window now, and native sharing landed in
2026-10.)*
