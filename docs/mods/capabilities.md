# Capabilities — normative registry

Status: v0, in force (written 2026-10-10, kept current through 0.9). The host
**enforces** every capability marked implemented in the registry below (with
`messages.read` scoped per channel and `net.egress` an allow-list enforced per
connection), with **default deny**: a host call without its
capability (or outside the read scope) returns `permission-denied`, and a message
outside the read scope is never delivered. The granted set is exposed to the mod
through `list-grants`. `settings.get`/`settings.set` are **ungated** (a mod always
reads its own settings); a **secret** lives in the system keyring, is written by
the user in the host-rendered form and is read-only to the mod. The form itself
renders only for a mod whose `ui.settings` is granted (revoking it hides the
form). Grants are
recorded per mod and managed with the `slaq mods`
CLI **and the in-app consent prompt** (GUI and TUI): it opens at startup for any
mod not yet decided, groups the capabilities it asks for, and is reopenable to
review or revoke. The same dialog (and `slaq mods scope|egress`) lets the user
choose **which channels** a mod reads and **which hosts** it reaches, beyond the
manifest: what a manifest declares is a default, and the user's own choice is
enforced as made (`min(consent, manifest)` still bounds what a *package update*
can ask for). The capability itself must be declared and granted. The store is
`~/.config/slaq/mods-grants.json`, next to
`$SLAQ_MODS_DIR`, or `$SLAQ_MODS_GRANTS`. The registry
below remains the target for the rest: a manifest that requests a capability this
host does not implement is refused at load. See [`api-v0.md`](api-v0.md).

**This file is the single source of truth for the capability vocabulary**: the
capability names, their host operations, contract stability, implementation
slice, consent group and risk class, plus the event rules and the reserved
names. `manifest-v0.md`, `api-v0.md` and `00-analysis.md` are narrative and must
**not** restate these lists. (Three artifacts are authoritative, one per axis:
**this file** for capability metadata, **`manifest.schema.json`** for the
manifest shape, **`api-v0.md`** for the WIT signatures.)

## Two independent axes

Do not conflate these:

- **Availability** — *has a host of this version implemented it?* A host ships
  the slices up to its version. A capability the host has not shipped is a
  **load-time rejection** (fail closed); a manifest may request only what the
  host implements. `reserved` = no slice at all — never requestable.
- **Contract stability** — *may the shape still change?* `stable` (frozen;
  breaking it needs a major `apiVersion` bump) or `draft` (the shape may still
  change). Stability says nothing about whether a host has shipped it.

So a capability can be `stable` **and** ship in a later slice: the contract is
frozen now, the host implements it later. The `Ships in` column is the
availability timeline.

## Consent groups

The install prompt **groups** capabilities under Read / Write / …; the groups
are visual headings, not the unit of consent. **Each capability is an
individual control**, so grants are per capability (a mod may be granted a
subset of what it requested). Groups only organize the prompt and let the UI
flag a dangerous combination.

| Group | Capabilities |
|---|---|
| **Read** | `messages.read`, `channels.list`, `users.list`, `search.read`, `files.download` |
| **Write** | `chat.send`, `chat.forward`, `reactions.write`, `files.upload` |
| **Network** | `net.egress` |
| **Filesystem** | `filesystem` |
| **Storage** | `store.kv`, `store.vector`, `store.sql` |
| **Interface** | `ui.*`, `notify` |
| **Commands** | `commands.slash` |
| **Background** | `background` |

Any **content-bearing Read** capability (`messages.read`, `search.read`,
`files.download`) combined with `net.egress` is an exfiltration combination and
is flagged separately. `filesystem` combined with `net.egress` is **also**
exfiltration: a mod may read its mapped slots and send what it finds anywhere the
egress list allows (the host contains *where* the mod reads, not *where* it
sends).

## The registry

| Capability | Host operations | Stability | Ships in | Group | Risk |
|---|---|---|---|---|---|
| `messages.read` | `get-message` (what slaq has loaded), `history` (a channel's past messages from Slack, paged with `before`) and `thread-replies` (a whole thread), both paced per mod (30 reads/min, ≤ 200 messages each) | stable | 1 (history: 0.9) | Read | **High** |
| `channels.list` | `directory.channels` | stable | 1 | Read | Low |
| `users.list` | `directory.users` | stable | 1 | Read | Low |
| `search.read` | `messages.search` | draft | — (deferred) | Read | **High** |
| `chat.send` | `chat.send` (slice 1: `chat-send`; ambient, no activation token) | stable | 2 | Write | **High** |
| `chat.forward` | `chat.forward` (slice 1: `chat-forward`, ambient) | draft | 3 | Write | Medium |
| `reactions.write` | `reactions.add`, `reactions.remove` (slice 1: `reactions-add`/`-remove`) | stable | 2 | Write | Low |
| `files.upload` | `files.upload` (slice 1: `file-upload`, ambient) | draft | 3 | Write | Medium |
| `files.download` | `files.download` (slice 1: `file-get`, an id from a delivered message) | draft | 3 | Read | Medium |
| `net.egress` | `http.fetch` | draft | 1 | Network | **High with a content-bearing Read** |
| `filesystem` | `fs-list`, `fs-read`, `fs-write`, `fs-search` (named slots the user maps to real directories; the host does all I/O) | draft | 5 | Filesystem | **High** (High with `net.egress`) |
| `background` | none: the host wakes the mod with `background.tick` on its own worker thread every `intervalSecs` (default 60, min 0.1) | draft | 1b/2 | Background | Medium (runs unattended; combine with care with `net.egress`) |
| `store.kv` | `store.kv-get`, `store.kv-set-many`, `store.kv-delete`, `store.kv-scan` (slice 1: `kv-get`/`kv-set`, persistent) | stable | 1 | Storage | Low |
| `store.vector` | `vec-upsert`, `vec-search`, `vec-delete` | stable | 1 (delete: 0.9) | Storage | Low |
| `store.sql` | `store.sql-query`, `store.sql-tx` (`sql-query`/`sql-tx`) | draft | 4 | Storage | Medium (DoS) |
| `ui.message_action` | message action buttons; the `action` callback | stable | 1 | Interface | Low |
| `ui.message_decoration` | `ui.set-message-decoration`, `ui.clear-message-decoration` | stable | 1 | Interface | Low |
| `ui.view` | `ui.set-view-content` | stable | 1 | Interface | Low |
| `ui.settings` | the settings tab rendered from `settings` | stable | 1 | Interface | Low |
| `ui.composer_action` | `ui.set-composer-preview`, `ui.set-composer-text` (slice 1: `ui-set-composer-text`) | stable | 2 | Interface | Low |
| `ui.context_menu` | context-menu items (slice 1: `ui.contextMenu`, delivered via the `on-action` callback) | draft | 2 | Interface | Low |
| `ui.dialog` | `ui.show-dialog`, `ui.close-dialog` (slice 1: `ui-show-dialog`/`ui-close-dialog` + the `on-dialog-action` callback) | draft | 2 | Interface | Low |
| `ui.theme` | declarative `ui.theme` (role → `#rrggbb` overrides; no host call) | draft | 2 | Interface | Low |
| `notify` | `notify.post` (slice 1: `notify-post`) | draft | 2 | Interface | Low |
| `commands.slash` | registers `/name`; the `command` callback (slice 1: `on-command`) | stable | 2 | Commands | Medium |

Behavioural requirements (scope, activation token, egress enforcement,
private-DB-only, upload confirmation) live in `manifest-v0.md` / `api-v0.md`.

## Events

Delivered only if the mod **both** subscribed in `events` **and** holds the
capability the event belongs to, within scope.

| Event | Requires | Ships in | Suppressed when |
|---|---|---|---|
| `message.created` / `.updated` | `messages.read` | 1 | channel is outside the read scope |
| `channel.opened` / `.updated` | `channels.list` | 1 | — |
| `reaction.added` / `.removed` | `messages.read` | 2 | channel is outside the read scope |
| `background.tick` | `background` | 2 | the mod is quarantined |
| `channel.member_joined` | `channels.list` + `users.list` | reserved | — |

Events carry **metadata** (channel, ts, author), not content; content is fetched
with `messages.get`, where scope is enforced. `action` and `command` are
**callbacks**, not subscriptions: declaring the matching UI/command delivers the
callback without an `events` entry. A **message action** is delivered only inside
the mod's `messages.read` scope (like message events), so a mod that declares one
should also request `messages.read`. **Message** events are delivered through
`on-message` (one kind per dispatch: a new message is `message.created`, an edit
`message.updated`); **channel** events carry no message and are delivered through
`on-event(kind, channel)`. **Reaction** events carry no message either:
`on-reaction(channel, ts, name, user, added)` names the emoji and whether it was
added, and the user's own reactions are **not** delivered (a mod reacts as the
user, so echoing them back would loop it into itself).

**Background lane:** `background.tick` is not a Slack event. A mod that requests
`background` (optionally `{"intervalSecs": N}`; default 60, fractions allowed,
minimum 0.1) **and** lists `background.tick` in `events` is woken through the
existing `on-event(kind, channel)` export with `kind = "background.tick"` and an
empty `channel`, on its own worker thread, every `intervalSecs` — whether or not
anything else happens, and never starved by a busy event stream (fixed delay:
the next tick is scheduled after the previous one returns). This is where a mod
does unattended work: re-index, embed a backlog, summarize, reorganize its
store. A tick is an ordinary callback (same wall-clock backstop and kill-switch),
so long work should be done in chunks across ticks; the mod's handler callbacks
(`on-message`, actions, …) run on the same thread between ticks. The user must
grant `background`; without it the lane never runs. No WIT change: existing
components need no rebuild.

**Slice-1 change (upgrade note):** the `events` subscription is now **enforced**,
so a mod that does **not** list `message.created`/`.updated` no longer receives
`on-message`; and a manifest listing an event this host has not implemented
(`message.deleted`, `channel.member_joined`) is now **refused at load** (it was
previously ignored). The WIT also gained **required** exports
(`on-event`, `on-reaction`, `on-command`, `on-composer-action`, `on-dialog-action`),
so a component built against an earlier slice must be **rebuilt** against the
current `wit/slaq-mod.wit` to load. Update older mods accordingly.

**Commands (slice 2):** a declared `commands` entry registers `/name`; typing it
in a composer runs the mod (`on-command`), **taking precedence over Slack's own
slash commands** (a mod may therefore shadow one such as `/status` — the user
grants `commands.slash`). Matching is by name: if two mods declare the same name,
**both** are invoked with the same arguments, and a command that reaches no live
mod falls through to Slack.

## Reserved (name held, not implemented)

A manifest requesting one is refused at load.

| Reserved | Intended shape |
|---|---|
| `chat.edit` / `chat.delete` | rewrite/remove a message — needs a host confirmation flow that does not exist yet |
| `presence.read` | live presence/status of users |
| `pins.read` / `pins.write` | pinned messages |
| `saved.read` / `saved.write` | the "Later" list |
| `profile.status.write` | set the user's status/emoji |
| `scheduled.read` / `scheduled.write` | the client-side send queue |
| `huddle.read` / `huddle.control` | presence in / control of a huddle |
| `webhook.receive` | an inbound endpoint delivering events to a mod |
| `messages.sql` | free SQL over messages — **deliberately dangerous, likely never** |

## Implementation slices

Each slice moves capabilities from "not shipped" to shipped; the contract does
not change (stability is separate). A host implements slices up to its version.

| Slice | Ships |
|---|---|
| **1 — substrate** | host spine + manifest parse/validate (fail closed); **implemented and enforced**: `messages.read` (scoped), `channels.list`, `users.list`, `store.kv`, `store.vector`, `ui.message_decoration`, `ui.settings`, `ui.message_action`, `ui.view`, `net.egress`; the `list-grants` import; the ungated `setting-get`/`setting-set` (per-mod settings, secrets in the keyring, read-only to the mod); message-action buttons + the `action` callback (`on-action`); mod views (`ui.views`) + `set-view-content`; the per-mod SQLite vector store (`vec-upsert`/`vec-search`); the `events` **subscription + delivery** (`message.created`/`.updated` via `on-message`, `channel.opened`/`.updated` via `on-event`); per-mod consent store + `slaq mods` CLI + in-app consent prompt (GUI/TUI). Slice 1 complete |
| **2 — chat-ops** | **complete** — implemented and enforced: `notify` (`notify-post`), `commands.slash` (declared `/name` + the `on-command` callback), `reactions.write` (`reactions-add`/`-remove`), `chat.send` (`chat-send`, ambient), `ui.composer_action` (declared `ui.composerActions` + `on-composer-action` + `ui-set-composer-text`), `ui.context_menu` (declared `ui.contextMenu`, via `on-action`), `ui.dialog` (`ui-show-dialog`/`ui-close-dialog` + the `on-dialog-action` callback), events `reaction.added`/`.removed` (via the `on-reaction` callback). Deferred: `search.read` — no concrete consumer yet, and a mod can search what it already reads (`messages.read` + `store.vector`) |
| **3 — files** | `files.upload` ✅ (slice 1: `file-upload`), `files.download` ✅ (slice 1: `file-get`), `chat.forward` ✅ (slice 1: `chat-forward`) |
| **4 — store.sql** | `store.sql` ✅ (`sql-query`/`sql-tx`) |
| **5 — filesystem** | `filesystem` ✅ (`fs-list`/`fs-read`/`fs-write`/`fs-search`): named slots the **user** maps to real directories; the **host** does every disk access, contains it (no `..`, no absolute path, no symlink escape, slaq's own dirs denied) and bounds it. The guest never sees a path. |
| **6 — reserved → shipped** | promote reserved names as real mods demand them |
