# slaq mod host API — v0

Status: draft v0, 2026-10-10. Companion to [`manifest-v0.md`](manifest-v0.md).

> **Slice-1 implementation (what the code actually ships).** The first slice
> (`src/mods/`, behind the `mods` feature) implements a deliberately minimal,
> **naming-divergent** subset, `wit/slaq-mod.wit`: `package slaq:api@0.1.0`,
> `world plugin`, imports `host` (`get-message`, `history`, `thread-replies`,
> `kv-get`, `kv-set`, `decorate`,
> `http-fetch`, `list-channels`, `list-users`, `setting-get`, `setting-set`,
> `set-view-content`, `vec-upsert`, `vec-search`, `vec-delete`, `notify-post`, `reactions-add`,
> `reactions-remove`, `chat-send`, `ui-set-composer-text`, `ui-show-dialog`,
> `ui-close-dialog`, `file-get`, `file-upload`, `chat-forward`, `sql-query`,
> `sql-tx`, `fs-list`, `fs-read`, `fs-write`, `fs-search`, `list-grants`) and exports `guest` (`start`,
> `on-message`, `on-action`, `on-event`, `on-reaction`, `on-command`,
> `on-composer-action`, `on-dialog-action`). Errors are plain `string`s rather than
> the `error` variant.
> **Capabilities are now enforced** for the implemented subset — `messages.read`
> (scoped per channel), `channels.list`, `users.list`, `store.kv`, `store.vector`,
> `ui.message_decoration`, `ui.settings`, `ui.message_action`, `ui.view`,
> `net.egress` (an
> allow-list, enforced per connection) — with default deny
> and `permission-denied`; `list-grants` is the slice-1 form of `grants.list`.
> `setting-get`/`setting-set` are **ungated** (always available); a `secret`
> setting is read-only to the mod and lives in the keyring.
> A declared `ui.messageActions` entry is rendered by the front ends and its
> `on-action` is the slice-1 form of the `action` callback.
> A declared `ui.views` entry is opened as a panel; the mod fills it with
> `set-view-content` `(kind, text)` blocks (the slice-1 form of `content-block`).
> `history(channel, before?, limit)` and `thread-replies(channel, thread-ts, limit)`
> (0.9.0, under `messages.read` and its scope) read **past** messages from Slack
> through the user's session, newest first and parent first respectively. This is
> unlike `get-message`, which serves only what slaq has loaded. Each call returns
> at most 200 messages and 4 MiB. Calls are **paced per mod**: 30 reads a minute,
> across dispatches. An error starting with `rate-limited` means try again later,
> and Slack's own rate limiting is reported the same way. A returned message's
> `files` is empty (`file-get` serves only files slaq has shown).
> `store.vector` is a per-mod SQLite store (`~/.local/share/slaq/mods/<id>/store.db`,
> opened lazily); `vec-upsert`/`vec-search` are cosine over the mod's rows, capped
> by the manifest's `dim`/`maxVectors`/`maxBytes`, and `vec-delete(keys)` (0.9.0)
> removes rows (returning how many), so a mod can keep its index within quota. `store.kv` shares that
> persistent file (`_slaq_kv`, bounded by `store.kv.maxBytes`) — unlike slice 1's
> in-memory map, a mod keeps its KV across reloads (and can use it to stay
> idempotent).
> The manifest `events` **subscription** is enforced:
> `message.created`/`message.updated` are delivered through `on-message` (one
> kind per dispatch: a new message vs an edit), `channel.opened`/`channel.updated`
> through `on-event` (as is `background.tick`, the periodic wake-up of a mod
> holding the `background` capability), and `reaction.added`/`reaction.removed` through
> `on-reaction` (the emoji, the reacting user and `added`; the reacted message is
> served by `get-message` when it is on screen). `message.deleted` is not
> implemented yet (refused at load, as is the reserved `channel.member_joined`).
> The client does not deliver the **user's own** messages *or reactions* to mods (a
> mod posting/reacting as the user would otherwise re-trigger itself in a loop).
> `notify-post` raises a desktop notification attributed to the mod (a no-op when
> the user has notifications off); it is the slice-1 form of `notify.post` and
> needs the `notify` capability.
> A declared `commands` entry registers `/name`: typing it in a composer runs the
> mod (the slice-1 `on-command(name, args, channel)`), taking precedence over
> Slack's own slash commands, and needs the `commands.slash` capability.
> `reactions-add`/`reactions-remove` add/remove one of the **user's** reactions on
> a message inside the mod's `messages.read` scope (the slice-1 form of
> `reactions.add`/`remove`, which take no activation token); the change appears
> when Slack echoes it back. A write is available from a message callback, not
> from `start` (the API client is installed after the first run).
> `chat-send` posts a message **as the user** in a channel inside the mod's
> `messages.read` scope (needs `chat.send` **and** `messages.read`). **Slice-1
> divergence:** the target `chat.send` takes an `activation-token` bound to a
> user-confirmed payload; slice 1 ships it **ambient** — the capability grant (plus
> the channel scope) is the authorization, with no per-message token or preview.
> The token remains for none of the slice-1 writes: `chat.forward` and
> `files.upload` shipped **ambient** too (see below); the target
> `activation-token` model (below) is the direction. **A posting mod must dedup
> itself** (e.g. a `sent` marker in the persistent `store.kv`, keyed by the source
> `ts`): delivery is at-least-once and the whole visible window is re-dispatched
> on a mod reload or an app restart, so a naive auto-reply would post once per
> message again. The message appears when Slack echoes it back. A **metered** call (Slack `429` + retry) blocks the
> dispatch thread, but the per-callback wall-clock deadline is now generous and
> configurable (`SLAQ_MOD_DEADLINE_SECS`, default 300 s), so only a genuinely
> wedged call quarantines the mod — the blocking bridge is shared with
> `reactions`/`http.fetch`.
> A declared `ui.composerActions` entry is rendered by the composer; invoking it
> runs `on-composer-action(id, channel, draft)`. The mod may reply with
> `ui-set-composer-text(channel, text)` — honored **only** during that callback —
> and the composer **becomes** the text (the **user** still sends it). Needs
> `ui.composer_action`; the slice-1 form of `ui.set-composer-text`
> (`ui.set-composer-preview` is a follow-up). Invoking an action requires the
> conversation to be inside the mod's `messages.read` scope (the draft is bounded
> by it, like every other content read); the new text is sanitized, so
> control/format characters — including newlines — are stripped. (The GUI shows a
> button per action; the TUI has no picker yet: **F5** runs the first declared
> action.)
> A declared `ui.contextMenu` entry is offered in a message's context menu and
> delivered through the **same** `on-action` callback as a message action (`id`,
> `channel`, `ts`; the message is made current), so it needs no import or export
> of its own — only `ui.context_menu` (and the channel inside the mod's
> `messages.read` scope). The TUI has no context menu, so the items ride its action
> menu.
> `ui.show-dialog`/`ui.close-dialog` are the slice-1 `ui-show-dialog` /
> `ui-close-dialog`: a mod shows a **modal** (title + `(kind, text)` blocks + a
> button per `(id, label)` action) that the front ends render as a modal over the
> UI. The user dismisses it (Esc / a Close button, always available — a mod cannot
> trap the user), or clicks an action, which is delivered through the
> `on-dialog-action(id)` callback. Needs `ui.dialog`; the sizes are bounded and one
> dialog is shown at a time (the last `show` wins).
> `files.download` is the slice-1 `file-get`: a message's **file ids** come from the
> `message` record `get-message` returns (`files: list<file-ref>`, each with `id`,
> `name`, `mime`), and the mod asks for the bytes with `file-get(id)`. The id must
> be one the host delivered, the file's channel must be in the mod's read scope, it
> must resolve to a Slack-hosted URL (the mod never supplies a URL), and the body
> is **size-capped** (a larger file is refused, not truncated). Needs
> `files.download` (and the file's channel must be in the mod's `messages.read`
> scope). `get-message` now returns a `message` record (`author`, `ts`,
> `text`, `thread-ts`, `reply-count`, `files`) instead of a bare `(author, text)`
> pair.
> `files.upload` is the slice-1 `file-upload`: the mod passes `(channel, name,
> bytes)` and the host uploads the file **as the user**, returning the new file's
> id (the type is inferred from `name`'s extension; slice 1 carries no explicit
> mime). It is **ambient** — the capability grant is the authorization, like
> `chat-send` — so it may be called from any callback; the target's payload-bound
> activation token (with a preview) is a deliberate slice-1 divergence. The
> channel must be inside the mod's `messages.read` scope, the bytes are
> size-capped, and uploads share the per-dispatch write budget.
> `chat.forward` is the slice-1 `chat-forward`: the mod forwards `(from-channel,
> ts)` into `to-channel` by posting a **link** to the source (the call the app's
> own forward uses: `chat.getPermalink` then `chat.postMessage`), returning the new
> `(channel, ts)`. It is **ambient** too; the source must be a message the mod
> **received** (the delivered window), and **both** channels must be inside its
> `messages.read` scope (it may only forward what it received, and only where it
> may post); it charges two slots of the per-dispatch write budget.
> `store.sql` runs against a **private** `scratch.db` (its own file, its own
> `maxBytes` quota): `sql-query` for one statement, `sql-tx` for a transaction.
> The hardening of `manifest-v0.md` is implemented — defensive mode, an authorizer
> denying `ATTACH`/`DETACH`, non-allow-listed pragmas and the reserved `_slaq_`
> namespace, `SQLITE_LIMIT_*` complexity/value limits, a statement timeout, and
> per-result row/byte caps (`maxResultRows`). It is **not** a door onto the host's
> data: messages stay behind `messages.read`. Needs `store.sql`.
> `filesystem` is the slice-5 set of four imports — `fs-list`, `fs-read`,
> `fs-write`, `fs-search` — over the **named slots** the user mapped to real
> directories. The guest passes `(slot, relative-path)`; the **host** performs the
> I/O and contains every access (no `..`, no absolute path, no symlink escape, and
> slaq's own config/data dirs are denied), so the guest never sees a path.
> `fs-list`/`fs-read`/`fs-search` need a `read` slot, `fs-write` a `write` slot;
> `fs-read` and `fs-write` are size-capped (refuse, never truncate) and
> `fs-write` is `create_new` (no overwrite); the list/search walk is bounded.
> Needs `filesystem`; `filesystem` + `net.egress` is an exfiltration combination.
> The granted set is recorded per mod (managed with the `slaq mods` CLI or the
> in-app consent prompt; see [`capabilities.md`](capabilities.md)).
> The full contract below is the target; the names will be reconciled before the
> API is declared stable, so build mods against `wit/slaq-mod.wit` for now.
> Adding a **required `guest` export** (as `ui.dialog` did with
> `on-dialog-action`) is a breaking ABI change that is **not** reflected in
> `apiVersion` (`0.1`) yet: an older component no longer instantiates and is
> skipped at load (per mod, so the others still run), so a host and its guests must
> be rebuilt together until an `apiVersion` bump gates this. Replacing or
> re-signing an existing host **import** (as `get-message` did when it moved to the
> `message` record) breaks old guests the same way.
> **Wall-clock kill-switch:** a guest callback has a generous, configurable
> wall-clock deadline (`SLAQ_MOD_DEADLINE_SECS`, default 300 s; `0` disables it),
> enforced by epoch interruption. A host call that blocks on the network
> (`file-get`, `file-upload`, `http-fetch`) counts against it, so only a genuinely
> wedged transfer quarantines the mod. The deadline is a backstop, not a compute
> budget — fuel was removed in the execution-model rework (see
> [`execution-model.md`](execution-model.md)). Each mod runs on its own OS
> thread, and stopping it (the Running mods window, a reload, exit) also cancels
> an in-flight host request within about 0.1 s.

The **manifest** is the contract with the *user* (what a mod declares). This
document is the contract with the *developer* (how a mod calls the host at
runtime). It is written in **WIT** (WebAssembly Interface Types), the interface
description language of the component model — the equivalent of `.proto` for a
WASM component. The host generates its bindings with
`wasmtime::component::bindgen!`; a mod written in Rust uses `wit-bindgen` (or
`cargo-component`). Neither side writes marshalling by hand.

A mod is a component that **imports** the host interfaces below and **exports**
one `handler`. The host implements the imports (that is the only place the Slack
session, network, storage and UI live) and calls the exports.

---

## The world

```wit
package slaq:mod@0.1.0;

world mod {
  import messages;
  import directory;
  import chat;
  import reactions;
  import files;
  import http;
  import settings;
  import store;
  import ui;
  import notify;
  import grants;

  export handler;
}
```

Each imported interface is gated by the manifest capabilities (mapping at the
end of this document). If a capability was not granted, the corresponding calls
return `error::permission-denied`. Two interfaces are **exempt**: `settings.*`
and `grants.list` are always available and never return `permission-denied`.

---

## WIT

```wit
package slaq:mod@0.1.0;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------
interface types {
  record message-ref {
    channel: string,
    ts: string,
  }

  record message {
    ref: message-ref,
    author: string,                 // user id; the host resolves display names
    ts: string,
    text: string,                   // mrkdwn, verbatim
    thread-ts: option<string>,
    reply-count: u32,
    files: list<file-ref>,          // attachments; feed `id` to files.download
  }

  record file-ref {
    id: string,                     // opaque Slack file id
    name: string,
    mime: string,
  }

  record channel-info {
    id: string,
    name: string,
    kind: channel-kind,
    member-count: u64,
  }

  enum channel-kind {
    channel,
    private-channel,
    dm,
    group-dm,
  }

  record user-info {
    id: string,
    display-name: string,
    is-bot: bool,
  }

  record sent {
    channel: string,
    ts: string,
  }

  // Opaque, host-minted, single-use, short-lived, and bound to a specific
  // operation and target. The host mints it only for an intent it rendered and
  // the user confirmed; a token for one operation is rejected for another.
  type activation-token = string;

  // A configured setting value. Secrets are read back as `text`.
  variant setting-value {
    text(string),
    integer(s64),
    real(f64),
    flag(bool),
  }

  variant error {
    permission-denied(string),      // capability not granted, or out of scope
    not-found(string),
    rate-limited(u32),              // retry-after, seconds
    unavailable(string),            // granted but disabled after load (e.g. revoked)
    invalid-argument(string),
    internal(string),
  }

  // ---- events (host -> mod) ----
  enum message-kind { created, updated, deleted }

  record message-event {
    ref: message-ref,
    author: string,
    kind: message-kind,
  }

  record reaction-event {
    ref: message-ref,
    name: string,
    user: string,
    added: bool,
  }

  record action-invoked {
    action-id: string,
    ref: option<message-ref>,       // set for a message action (carries channel)
    view: option<string>,           // set for a view action
    channel: option<string>,        // set for a composer action (its target)
    composer-draft: option<string>, // set for a composer action
    activation: option<activation-token>, // authorizes the matching follow-up
    user: string,
  }

  record command-invoked {
    name: string,
    args: string,
    channel: string,
    user: string,
    activation: option<activation-token>, // authorizes a follow-up chat.send
  }

  record channel-event {
    channel: string,
    user: option<string>,           // the member, for member-joined
  }

  variant event {
    message(message-event),
    reaction(reaction-event),
    action(action-invoked),
    command(command-invoked),
    channel-opened(channel-event),
    channel-updated(channel-event),
    member-joined(channel-event),
  }
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------
interface messages {
  use types.{message-ref, message, error};

  get: func(refs: list<message-ref>) -> result<list<message>, error>;
  history: func(channel: string, oldest: option<string>,
                latest: option<string>, limit: u32) -> result<list<message>, error>;
  thread: func(channel: string, thread-ts: string) -> result<list<message>, error>;
  search: func(query: string, limit: u32) -> result<list<message>, error>;
}

interface directory {
  use types.{channel-info, user-info, error};

  channels: func() -> result<list<channel-info>, error>;
  users: func() -> result<list<user-info>, error>;
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------
interface chat {
  use types.{sent, error, activation-token};

  // Every mutating call takes a `token` the host minted for THIS operation,
  // target AND payload: it binds the exact channel/thread/text the user approved
  // (by hash). A call whose arguments differ from the approved payload is
  // rejected, so a mod cannot show one thing and send another. Single-use,
  // short-lived, not transferable.
  send: func(token: activation-token, channel: string, text: string,
             thread: option<string>) -> result<sent, error>;
  forward: func(token: activation-token, from-channel: string, ts: string,
                to-channel: string) -> result<sent, error>;
}

interface reactions {
  use types.{error};

  add: func(channel: string, ts: string, name: string) -> result<_, error>;
  remove: func(channel: string, ts: string, name: string) -> result<_, error>;
}

interface files {
  use types.{error, activation-token};

  // `token` authorizes publishing THIS payload to THIS channel: the host shows a
  // preview/confirmation and binds the token to (channel, content hash), so an
  // upload cannot happen from `start` or a passive event. Draft: large bodies
  // should move via streams or host-provided handles, not `list<u8>`.
  upload: func(token: activation-token, channel: string, name: string,
               mime: string, bytes: list<u8>) -> result<string, error>;
  // `file` is the opaque id from `message.files[].id`; the host resolves it and
  // rejects non-Slack targets.
  download: func(file: string) -> result<list<u8>, error>;
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------
interface http {
  use types.{error};

  record request {
    method: string,
    url: string,
    headers: list<tuple<string, string>>,
    body: option<list<u8>>,
  }

  record response {
    status: u16,
    headers: list<tuple<string, string>>,
    body: list<u8>,
  }

  // The host enforces a fixed maximum request/response size; a body over the
  // limit is rejected deterministically (never buffered unbounded). There is no
  // field to name a host-managed secret: that would let a mod point a
  // host-owned credential at a destination of its choosing (confused deputy).
  fetch: func(req: request) -> result<response, error>;
}

// ---------------------------------------------------------------------------
// Settings and storage
// ---------------------------------------------------------------------------
interface settings {
  use types.{error, setting-value};

  // The value the user configured. Secrets are returned as `text`.
  get: func(key: string) -> result<option<setting-value>, error>;

  // Non-secret keys only; secret fields are user-written and read-only to the mod.
  set: func(key: string, value: setting-value) -> result<_, error>;
}

interface store {
  use types.{error};

  kv-get: func(key: string) -> result<option<list<u8>>, error>;
  kv-set-many: func(items: list<tuple<string, list<u8>>>) -> result<_, error>;
  kv-delete: func(keys: list<string>) -> result<_, error>;
  kv-scan: func(prefix: string) -> result<list<string>, error>;

  record vector-item { key: string, vector: list<f32>, meta: option<string> }
  record vector-hit  { key: string, score: f32, meta: option<string> }
  vec-upsert-many: func(items: list<vector-item>) -> result<_, error>;
  vec-search: func(vector: list<f32>, k: u32, filter-prefix: option<string>)
      -> result<list<vector-hit>, error>;
  // Implemented in 0.9.0 as `vec-delete(keys) -> result<u32, string>`.
  vec-delete: func(keys: list<string>) -> result<u32, error>;

  // Implemented in slice 4, with the hardening of `manifest-v0.md`. Private DB only.
  variant sql-value { null, int(s64), real(f64), text(string), blob(list<u8>) }
  record sql-result { columns: list<string>, rows: list<list<sql-value>>, changes: u64 }
  sql-query: func(query: string, params: list<sql-value>) -> result<sql-result, error>;
  // One result per statement, in order.
  sql-tx: func(statements: list<tuple<string, list<sql-value>>>) -> result<list<sql-result>, error>;
}

// ---------------------------------------------------------------------------
// Declarative UI — the mod contributes data; the host renders in egui and ratatui
// ---------------------------------------------------------------------------
interface ui {
  use types.{message-ref, error};

  variant content-block {
    section(string),                // paragraph, message inline markup
    context(string),                // small, dimmed secondary line
    reference(message-ref),         // clickable; the host jumps to the message
    divider,
  }

  record action { id: string, label: string }

  set-message-decoration: func(ref: message-ref, content: list<content-block>)
      -> result<_, error>;
  clear-message-decoration: func(ref: message-ref) -> result<_, error>;

  // acts on the composer that invoked the action
  set-composer-preview: func(content: list<content-block>, actions: list<action>)
      -> result<_, error>;
  set-composer-text: func(text: string) -> result<_, error>;

  set-view-content: func(view: string, content: list<content-block>) -> result<_, error>;

  record dialog { title: string, content: list<content-block>, actions: list<action> }
  show-dialog: func(dialog: dialog) -> result<_, error>;
  close-dialog: func() -> result<_, error>;
}

interface notify {
  use types.{error};
  post: func(title: string, body: string) -> result<_, error>;
}

// ---------------------------------------------------------------------------
// The mod's exports
// ---------------------------------------------------------------------------
interface grants {
  // The capabilities the host actually granted (may be a subset of the manifest's
  // request, since consent is per capability). This is a HOST import: the guest
  // cannot fabricate it.
  list: func() -> list<string>;
}

interface handler {
  use types.{event, error};

  // Runs once after instantiation; query what was granted via the `grants`
  // import (not a guest return value).
  start: func() -> result<_, error>;
  // Receives passive events (only those subscribed in the manifest) AND the
  // implicit `action` / `command` callbacks. (Slice 1: `on-event(kind, channel)`
  // for the message-less events; message events go to `on-message`.)
  on-event: func(ev: event) -> result<_, error>;
}
```

---

## Design notes

### Metadata, not content
Events carry references and metadata. Message text is fetched with
`messages.get`, which is where the read **scope** is enforced per `message-ref`.
The host delivers a *passive* event only if the mod listed it in the manifest
`events`. The **callback** events — `action` and `command` — are implicit: a mod
that declares a message action, a composer action or a slash command receives
its own callback without listing it in `events`.

### Sends follow a visible action
Every outbound/mutating call — `chat.send`, `chat.forward`, `files.upload` —
takes an `activation-token` the host mints only for a specific intent it
rendered and the user confirmed. The token is single-use, short-lived and
**bound to the operation, the target and the exact payload** (channel/thread and
a hash of the text or of the file bytes), so neither the operation nor the
destination nor the content can change between preview and send. A call from
`start`, from a passive event, or with arguments that differ from the approved
payload is rejected — install-time consent alone does not let untrusted code
publish content it did not show.

### Batching
Every read/write that can touch many rows is plural (`get`, `history`,
`kv-set-many`, `vec-upsert-many`). Crossing the host boundary per item is the one
measurable cost of the sandbox (`00-analysis.md` §5.1).

### Bytes and the boundary
`files.*` and `http.body` use `list<u8>`, which copies across the boundary; this
is fine for API-sized payloads but is **draft** for large files, where streams or
host-provided handles are the right shape. `http.fetch` bodies are **bounded by
the host**: an over-limit request or response is rejected with a deterministic
error rather than buffered, so an untrusted peer cannot OOM the privileged host.

### Secrets
- **Mod-owned** secret → the user fills it in the settings form, the host stores
  it in the keyring, and the mod reads it with `settings.get`. The mod may put it
  in `http.fetch` headers.
- **Host-owned** secret → **never** exposed, and not reachable through any v0
  call. In particular `http.fetch` cannot name one: a mod-controlled secret name
  plus a mod-controlled URL would send a host credential to an arbitrary host.
- `xoxc`/`xoxd` are host-only and are not reachable through any interface.

### Errors
`permission-denied` covers both "capability not granted" and "outside scope". A
capability the host does not implement is a **load-time rejection**, not a
runtime error: a manifest requesting it does not load (see `manifest-v0.md`).
`unavailable` at runtime is reserved for a capability disabled *after* load
(e.g. revoked mid-session); a mod must degrade gracefully on it.

### Declarative UI
The mod never draws. `content-block` is a small, purpose-built schema — *not*
Slack's Block Kit. `reference(message-ref)` lets a view cite a message and make
it clickable (the host jumps to it), which is what an answer must do to be
grounded.

### Capability → interface mapping

The mapping and the implementation slices are normative in
[`capabilities.md`](capabilities.md). In one line: every imported interface is
unlocked by the capability named after it — `messages.*` →
`messages.read`/`search.read`, `directory.*` → `channels.list`/`users.list`,
`chat.*` → the matching `chat.*`, `http.fetch` → `net.egress`, `store.*` → the
matching `store.*`, `ui.*` → the matching `ui.*`, `notify.post` → `notify`.
`settings.*` and `grants.list` are **ungated** — always available and never
`permission-denied`; every *other* operation without its capability returns
`permission-denied`.

---

## Open points

- Streaming / handles for large file and HTTP bodies (replace `list<u8>`).
- View action callbacks: `set-view-content` exists, but a view needs its own
  action event — folded into `action.invoked.view` for now.
- `sql` value types: BLOB ↔ `list<u8>` mapping and size limits.
- A host helper for mrkdwn segmentation (`text.segments`) — see
  `examples/translate-mod.md`.
