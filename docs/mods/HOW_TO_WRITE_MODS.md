# How to write a slaq mod — a guide for an LLM agent

**Audience: you, the coding agent.** A user has pointed you at this file because
they want a slaq mod. Read this whole file first. Then **interview the user**
about what the mod should do, **tell them the relevant limitations**, choose the
**minimal** set of capabilities, and produce a **working** mod: a `manifest.json`,
a Rust WASM component, and the exact build/grant/run commands.

Do not skip the interview. A mod is a security boundary: every capability you
request is something the user must consent to, and some combinations
(read + network, filesystem + network) can exfiltrate their data. Your job is to
build the *least-privilege* mod that does what they actually need.

This file is self-contained, but these are the normative sources if you need to
verify a detail (read them, don't guess):

- `wit/slaq-mod.wit` — the exact host/guest contract (what you can call, what you
  must implement). **Vendor this file into your mod** (see Scaffold).
- `docs/mods/capabilities.md` — the capability vocabulary (single source of truth).
- `docs/mods/manifest-v0.md` — the manifest shape and behavioural rules.
- `docs/mods/api-v0.md` — the full WIT signatures and call contracts.
- `docs/mods/manifest.schema.json` — the JSON Schema (bounds: maxItems, maxLength).
- Working examples: the sibling repos `slaq-mod-hello/` (decoration + KV counter)
  and `slaq-mod-docs/` (filesystem search). Read their `src/lib.rs`.

---

## 0. What a mod is (the mental model)

- A mod is a **WebAssembly Component** (`wasm32-unknown-unknown`, the component
  model — *not* WASI) plus a **declarative `manifest.json`**.
- It runs in a **sandbox** (Wasmtime). It has **no filesystem, no sockets, no
  network, no Slack credentials** of its own. The **host does every side effect**
  on the mod's behalf, and only for capabilities the user granted.
- The mod **imports** host functions (things it may ask the host to do) and
  **exports** callbacks (things the host calls on the mod when events happen).
- Everything is **default-deny**: a host call without its granted capability (or
  outside its scope) returns a `permission-denied` error string; an event outside
  scope is never delivered.
- Delivery of `on-message` is **at-least-once**: the same message can arrive more
  than once (after an edit, or after the host's delivery history evicts an old
  entry). **Handlers must be idempotent** — e.g. a KV counter can advance twice.

This is slice-based: the current slaq (0.7.x) implements **slices 1–5**, i.e. the
whole capability set below except the `reserved` names. A manifest that requests a
capability the host does not implement, a `reserved` name, or an unknown name, is
**refused at load**.

---

## 1. Interview the user first

Ask these before writing anything. Map each answer to the minimal capability.
Confirm the exfiltration trade-offs out loud.

1. **Purpose, in one sentence.** What should the mod *do* when a message arrives,
   a button is clicked, or a command is typed?
2. **Trigger.** Reacts to incoming messages? A message-action button? A slash
   command? A composer action? A context-menu item? Runs once at `start`?
   → picks `events` + which `ui.*`/`commands.slash`.
3. **Which conversations?** All channels, or a specific list? → `messages.read`
   scope (`all` vs `channels` + ids). Prefer a narrow scope.
4. **Does it read message content?** → `messages.read` (High risk).
5. **Does it talk to the network / an LLM / an API?** Which host(s)? → `net.egress`
   allow-list. **Warn:** `messages.read` + `net.egress` is an *exfiltration
   combination* — the mod can send message content anywhere the egress list allows.
   Confirm the user accepts this, and keep the egress list as tight as possible.
6. **Does it write back?** React (`reactions.write`), post a message
   (`chat.send`), forward (`chat.forward`), upload a file (`files.upload`)? These
   act **as the user**.
7. **Does it need to show something?** An inline decoration under a message
   (`ui.message_decoration`), a sidebar view (`ui.view`), a modal
   (`ui.dialog`), a theme (`ui.theme`), a desktop notification (`notify`)?
8. **Does it need to remember state?** Small key/value (`store.kv`), vector/
   embedding search (`store.vector`), a private SQL DB (`store.sql`)?
9. **Does it need local files?** → `filesystem` with named **slots** the user maps
   to directories. **Warn:** `filesystem` + `net.egress` is also exfiltration.
10. **Settings / secrets?** API keys, endpoints, options the user configures →
    `settings` schema + `ui.settings`. Secrets live in the system keyring; the mod
    can read but never write them. After an update or rebuild the user confirms
    once that the new package keeps its saved secrets; until then the mod reads
    none, so handle a missing key gracefully.

Then state back: *"I'll request exactly these capabilities: … . Here's what each
lets the mod do, and here's the one combination to be aware of … . OK to proceed?"*

---

## 2. Capabilities (what you may request)

All of the below are implemented by current slaq. Request the **fewest**. Each
manifest entry's shape is shown. Full rules: `docs/mods/capabilities.md`.

### Read
| Capability | Unlocks | Manifest shape | Risk |
|---|---|---|---|
| `messages.read` | `get-message`, `history`, `thread-replies`; delivery of `on-message`/`on-reaction` | `{ "scope": "all" }` or `{ "scope": "channels", "channels": ["C…","D…"] }` | **High** |
| `channels.list` | `list-channels`; `on-event` channel events | `{}` | Low |
| `users.list` | `list-users` | `{}` | Low |
| `files.download` | `file-get(id)` (id from a delivered message; Slack files only) | `{}` | Medium |

### Write (acts as the user)
| Capability | Unlocks | Manifest shape | Risk |
|---|---|---|---|
| `chat.send` | `chat-send(channel, text, thread?)` (needs `messages.read` scope too) | `{}` | **High** |
| `chat.forward` | `chat-forward(from, ts, to)` | `{}` | Medium |
| `reactions.write` | `reactions-add` / `reactions-remove` | `{}` | Low |
| `files.upload` | `file-upload(channel, name, bytes)` | `{}` | Medium |

### Network
| Capability | Unlocks | Manifest shape | Risk |
|---|---|---|---|
| `net.egress` | `http-fetch(method, url, headers, body?)` | `["host:port", "api.example.com"]` (allow-list) | **High with any content Read** |

`net.egress` is enforced **per connection**: the host resolves the name, refuses
non-public addresses / DNS-rebinding, does **not** follow redirects, and caps
request (2 MB) / response (4 MB) / timeout (10 s). `localhost` is never implicit —
list it explicitly if the mod talks to a local model.

The manifest's list is a **default, not a ceiling**: the user can untick any of
its hosts and add their own, in the app (Mods → net.egress → *Change…*) or with
`slaq mods egress <id> add|rm <host>`. A mod whose endpoint is the user's choice
(a self-hosted model, a local agent) can declare `"net.egress": []` and let the
user add it. The same goes for the channels: the user may widen or narrow the
`messages.read` scope (*Channels → Change…*, or `slaq mods scope <id> --all |
--reset | <channel ids>`). Write the mod to cope with whatever the user grants —
`list-grants` and `permission-denied` tell it at runtime.

### Storage (per-mod, private, persistent)
| Capability | Unlocks | Manifest shape |
|---|---|---|
| `store.kv` | `kv-get` / `kv-set` | `{ "maxBytes": 1048576 }` |
| `store.vector` | `vec-upsert` / `vec-search` (cosine) / `vec-delete` | `{ "dim": 384, "maxVectors": 10000, "maxBytes": 8388608 }` |
| `store.sql` | `sql-query` / `sql-tx` against a private `scratch.db` | `{ "maxBytes": 8388608, "maxResultRows": 10000 }` |

`maxBytes` is clamped to a 64 MiB host maximum. KV is persistent and (slice 1)
only `get`/`set` — a mod can't free space yet, so don't let it grow unbounded.

### Interface
| Capability | Unlocks | Manifest field |
|---|---|---|
| `ui.message_decoration` | `decorate(channel, ts, text)` under a message | — (call at runtime) |
| `ui.message_action` | a button on a message → `on-action` | `ui.messageActions: [{id,label}]` |
| `ui.context_menu` | a context-menu item → `on-action` | `ui.contextMenu: [{id,label,icon?}]` |
| `ui.composer_action` | `ui-set-composer-text` (needs `messages.read`) → `on-composer-action` | `ui.composerActions: [{id,label}]` |
| `ui.view` | `set-view-content(view, blocks)` in a sidebar view | `ui.views: [{id,label}]` |
| `ui.dialog` | `ui-show-dialog` / `ui-close-dialog` → `on-dialog-action` | — (call at runtime) |
| `ui.theme` | role → `#rrggbb` palette overrides | `ui.theme: {"accent":"#ff0066", …}` |
| `ui.settings` | renders the `settings` form tab | — (required if `settings` non-empty) |
| `notify` | `notify-post(title, body)` desktop notification | — |

### Commands
| Capability | Unlocks | Manifest field |
|---|---|---|
| `commands.slash` | registers `/name` → `on-command` (shadows Slack's own) | `commands: [{name,description?}]` |

### Filesystem
| Capability | Unlocks | Manifest shape |
|---|---|---|
| `filesystem` | `fs-list` / `fs-read` / `fs-search` (read slots), `fs-write` (write slots) | `{ "slots": [{"name":"docs","access":"read"}] }` |

Slots are **labels** (`^[a-z0-9]([a-z0-9_-]*[a-z0-9])?$`, ≤ 8). The **user** maps
each to a real directory; the mod passes `(slot, relative-path)` and the host does
the I/O (no `..`, no absolute paths, no symlink escape, slaq's own dirs denied,
files ≤ 4 MiB, `fs-write` is create-only/no-overwrite).

### Background (unattended work)
| Capability | Unlocks | Manifest shape |
|---|---|---|
| `background` | a **background lane**: the host calls your existing `on-event` with `kind = "background.tick"` (empty `channel`) every `intervalSecs` | `{ "intervalSecs": 30 }` (default 60, fractions ok, min 0.1) **and** `"background.tick"` in `events` |

Use it for work that should happen without a user gesture or a new message:
embedding a backlog, re-indexing, summarizing, compacting your store. Each tick
is an ordinary callback on your mod's own thread — do a **bounded chunk** per
tick (e.g. "embed the next 50 rows") and keep progress in `store.sql`/`store.kv`,
so message/action callbacks interleave promptly. The user must grant
`background`; check `list-grants` and degrade if it's absent.

### Talking to a local agent (Claude Code, opencode, …)
A mod can hand work to an agent running on the user's machine: list its local
endpoint explicitly in `net.egress` (e.g. `["localhost:8787"]`; loopback is
refused unless named — or declare `[]` and let the user add their own port) and
`http-fetch` it. A callback may wait minutes for the
answer — each mod runs on its own thread, so nothing else stalls — then
`decorate` the result. For long jobs, prefer *submit + poll*: the user's button
queues the job (`store.sql`), and the background lane polls the agent and
decorates when it is done.

**Security — read before building one.** A mod that reads Slack messages and can
drive a local agent with tools (shell, files) lets **anyone who posts in the
workspace** try to prompt-inject that agent. Only forward content on an explicit
**user gesture** (a message action), never automatically from `on-message`; tell
the user plainly in the manifest description; and configure the agent itself with
the narrowest tool permissions that do the job. A first-class `agent.bridge`
capability (consent flagging, progressive output) is planned.

### Reserved — do NOT request (load-time rejection)
`chat.edit`, `chat.delete`, `presence.read`, `pins.*`, `saved.*`,
`profile.status.write`, `scheduled.*`, `huddle.*`, `webhook.receive`,
`messages.sql`, and `search.read` (deferred). If the user needs one of these, tell
them it isn't available yet — don't try to work around it.

**Rules that bite:**
- To receive an event you must **both** list it in `events` **and** hold its
  capability, within scope.
- A non-empty `ui.*` array / `settings` / `commands` **requires** the matching
  capability, or the manifest is rejected.
- `ui.settings` is required whenever `settings` is non-empty.

---

## 3. The contract (WIT) — what you call, what you implement

The authoritative file is `wit/slaq-mod.wit` (package `slaq:api@0.1.0`, world
`plugin`). Slice 1 is **primitive types only** (strings, ints, lists, options,
results) to keep the host small.

**You may call (host imports)** — each capability-gated, returns
`result<_, string>` (the error string is `"permission-denied"` when a grant/scope
is missing, so you can branch):

```
get-message(channel, ts) -> message            // messages.read (what slaq has loaded)
history(channel, before?, limit) -> list<message>        // messages.read: past messages from Slack, newest first
thread-replies(channel, thread-ts, limit) -> list<message> // messages.read: a whole thread, parent first
kv-get(key) / kv-set(key, value)               // store.kv
decorate(channel, ts, text)                    // ui.message_decoration
http-fetch(method, url, headers, body?) -> (status, bytes)   // net.egress
list-channels() / list-users()                 // channels.list / users.list
list-grants() -> list<string>                  // always available — branch on partial consent
setting-get(key) / setting-set(key, value)     // always available (settings.*); secrets read-only
set-view-content(view, blocks)                 // ui.view
vec-upsert(items) / vec-search(vector, k, prefix?) / vec-delete(keys)   // store.vector
sql-query(query, params) / sql-tx(statements)  // store.sql
notify-post(title, body)                        // notify
reactions-add / reactions-remove(channel, ts, name)  // reactions.write
chat-send(channel, text, thread?) -> (channel, ts)   // chat.send + messages.read
chat-forward(from-channel, ts, to-channel)     // chat.forward + messages.read
ui-set-composer-text(channel, text)            // ui.composer_action (only during on-composer-action)
ui-show-dialog(title, blocks, actions) / ui-close-dialog()   // ui.dialog
file-get(id) -> bytes / file-upload(channel, name, bytes) -> id   // files.download / files.upload
fs-list / fs-read / fs-write / fs-search(slot, …)   // filesystem
```

`blocks` are `(kind, text)` pairs where `kind` is `section`, `context` or
`divider`. A `message` record is `{ author, ts, text, thread-ts?, reply-count,
files: [{id,name,mime}] }`.

**You must implement ALL of these (guest exports)** — even as no-ops. Missing one
means the component won't load:

```
start()                                         // once after instantiation
on-message(channel, ts, author, text)           // a message (created or updated) — MUST be idempotent
on-action(id, channel, ts)                       // a message-action / context-menu item
on-event(kind, channel)                          // channel.opened / channel.updated
on-reaction(channel, ts, name, user, added)      // a reaction (not the user's own)
on-command(name, args, channel)                  // a slash command
on-composer-action(id, channel, draft)           // a composer action
on-dialog-action(id)                             // a dialog button
```

> If you only build against an older WIT, the component **won't load** — the
> required exports changed. Always vendor the current `wit/slaq-mod.wit`.

---

## 4. Manifest (`manifest.json`)

Minimal viable manifest, then the field list. See `docs/mods/manifest-v0.md` for
every rule and `manifest.schema.json` for bounds.

```json
{
  "manifestVersion": 0,
  "id": "com.example.mymod",
  "name": "My Mod",
  "version": "0.1.0",
  "apiVersion": "0.1",
  "minSlaq": "0.9.0",
  "description": "One line shown in the consent prompt.",
  "entry": { "type": "wasm", "path": "mod.wasm" },
  "capabilities": {
    "messages.read": { "scope": "channels", "channels": ["C0ENGINEER"] },
    "ui.message_decoration": {}
  },
  "events": ["message.created", "message.updated"]
}
```

| Field | Req | Notes |
|---|---|---|
| `manifestVersion` | yes | `0`. |
| `id` | yes | reverse-DNS, unique; also namespaces the mod's storage + keyring secrets. |
| `name`, `version` | yes | display name; the mod's own semver. |
| `apiVersion` | yes | `"0.1"` (the host contract it targets). |
| `minSlaq` | no | minimum slaq version: set it to the version that implements what you use. Capabilities in slices 1–5 need 0.7; `background` needs 0.8; `history`, `thread-replies` and `vec-delete` need 0.9. slaq 0.9+ refuses a mod that needs a newer version and says so. Older builds ignore the field, but still refuse a capability or a host function they lack. |
| `description`, `author`, `homepage`, `license`, `locales` | no | provenance; `description` shows in the prompt. |
| `entry` | yes | `{ "type": "wasm", "path": "mod.wasm" }`. |
| `capabilities` | yes | object keyed by capability name (shapes above). |
| `events` | no | subscriptions: `message.created`, `message.updated`, `channel.opened`, `channel.updated`, `reaction.added`, `reaction.removed`. |
| `ui` | no | `{ messageActions?, contextMenu?, composerActions?, views?, theme? }`. |
| `settings` | no | `[{ key, type, default?, label?, values? }]`; types `string|number|bool|enum|url|secret`. |
| `commands` | no | `[{ name, description? }]`. |

Labels can be literals or `"i18n:key"` for localization.

---

## 5. Limitations (tell the user)

Discuss these so the mod's design is realistic:

- **Primitive data only (slice 1).** No rich records across the boundary yet;
  blocks are simple `(kind, text)` section/context/divider.
- **At-least-once delivery.** `on-message` can fire twice for one message — make
  handlers idempotent (don't double-count, double-post, double-react).
- **No ambient I/O.** No sockets, no raw filesystem, no Slack token. Network only
  via `net.egress` + `http-fetch` (no redirects, size/time caps). Files only via
  user-mapped `filesystem` slots.
- **Chat writes are ambient (slice 1).** `chat-send`/`chat-forward`/`file-upload`
  act with just the grant (no per-action confirmation token yet) — so a mod that
  can read + send could auto-reply; design conservatively ("stop and propose").
- **Scope bounds everything.** `messages.read` scope limits which channels deliver
  events and which `get-message`/`chat-send` targets are allowed.
- **KV can't shrink (slice 1).** Only `get`/`set`; a mod can wedge its own store if
  it writes unbounded keys. Cap what you store.
- **Grants apply at load.** `slaq mods grant/revoke` take effect on the **next**
  load; live mid-session revocation isn't implemented.
- **Partial consent is normal.** The user may grant a subset. Call `list-grants`
  at `start` and branch — don't assume a capability is present (see how hello
  guards `ui.view`/`ui.dialog`).
- **Execution model.** Each mod runs on its own OS thread, so a slow mod never
  blocks the UI or other mods. There is no CPU budget; a callback has a generous
  wall-clock backstop (300 s by default, `SLAQ_MOD_DEADLINE_SECS`) and guest
  memory defaults to 512 MiB (`SLAQ_MOD_MEMORY_MB`). A trap (incl. hitting the
  backstop) quarantines the mod for the session. Network calls count against the
  backstop, so a multi-second LLM call is fine; an unbounded loop is not.
- **One workspace, Linux-first**, like slaq itself.
- **Reserved capabilities don't exist** (editing/deleting messages, presence,
  pins, saved, scheduled, huddle control, inbound webhooks, free SQL over
  messages). Don't promise them.

---

## 6. Project scaffold

Layout:

```
mymod/
  Cargo.toml
  manifest.json
  wit/
    slaq-mod.wit      # a byte-identical copy of slaq/wit/slaq-mod.wit
  src/
    lib.rs
```

**Prerequisites** (tell the user to install if missing):

```sh
rustup target add wasm32-unknown-unknown
cargo install cargo-component          # the component-model build tool
```

**Vendor the contract** (keep it in sync with the host; a stale copy that drops a
required export won't load):

```sh
cp /path/to/slaq/wit/slaq-mod.wit mymod/wit/slaq-mod.wit
```

**`Cargo.toml`**:

```toml
[package]
name = "mymod"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen-rt = { version = "0.44.0", features = ["bitflags"] }

[package.metadata.component]
package = "slaq:mymod"

[package.metadata.component.dependencies]
```

**`src/lib.rs`** — implement **every** guest export; call host functions through
`bindings::slaq::api::host`. Skeleton (fill in the handlers you need, leave the
rest as `Ok(())`):

```rust
#[allow(warnings)]
mod bindings;

use bindings::exports::slaq::api::guest::Guest;
use bindings::slaq::api::host;

struct Component;

impl Guest for Component {
    fn start() -> Result<(), String> {
        // Branch on partial consent: only use a capability you were granted.
        // let grants = host::list_grants();
        Ok(())
    }

    fn on_message(channel: String, ts: String, _author: String, _text: String)
        -> Result<(), String>
    {
        // Idempotent! This can be called more than once for the same message.
        let m = host::get_message(&channel, &ts)?;         // messages.read
        host::decorate(&channel, &ts, &format!("seen: {}", m.text)) // ui.message_decoration
    }

    fn on_action(_id: String, _channel: String, _ts: String) -> Result<(), String> { Ok(()) }
    fn on_event(_kind: String, _channel: String) -> Result<(), String> { Ok(()) }
    fn on_reaction(_c: String, _t: String, _n: String, _u: String, _a: bool) -> Result<(), String> { Ok(()) }
    fn on_command(_name: String, _args: String, _channel: String) -> Result<(), String> { Ok(()) }
    fn on_composer_action(_id: String, _channel: String, _draft: String) -> Result<(), String> { Ok(()) }
    fn on_dialog_action(_id: String) -> Result<(), String> { Ok(()) }
}

bindings::export!(Component with_types_in bindings);
```

`cargo component` generates `src/bindings.rs` from `wit/` at build time.

---

## 7. Build

```sh
cargo component build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/mymod.wasm mod.wasm   # next to manifest.json
```

(`mymod` → your crate name with `-` turned to `_`.) A rebuild is required when a
**required guest export** changes or an existing import's signature changes; a new
host import you don't call does **not** need a rebuild.

---

## 8. Run, grant, and test

The slaq binary must be built **with the mods feature**:
`cargo build --release --features mods` (or use a release binary — mods ship in
every slaq release).

**One-shot headless run** (fastest dev loop — fires one synthetic event):

```sh
# Grant exactly what you want to test (repeatable); default-deny otherwise:
slaq mod --dir ./mymod --grant messages.read --grant ui.message_decoration \
  --channel C0TEST --ts 1.000000 --author U0TEST --text "hello"

# Dev shortcut: grant everything the host implements
slaq mod --dir ./mymod --grant-all

# Filesystem mod: map a slot for this run only
slaq mod --dir ./mymod --grant filesystem --fs-slot docs=/tmp/docs-demo
```

**Installed-mod consent** (persisted; used by the GUI/TUI at startup):

```sh
slaq mods list                                  # requested vs granted caps
slaq mods grant  com.example.mymod messages.read ui.message_decoration
slaq mods revoke com.example.mymod net.egress
slaq mods map    com.example.mymod docs /home/me/notes      # filesystem slot → dir
slaq mods unmap  com.example.mymod docs
```

Grants take effect on the next load. The GUI/TUI also show an in-app consent
prompt at startup for any undecided mod.

---

## 9. Definition of done (check before you hand it back)

- [ ] Requests the **minimal** capabilities; every one is justified by the user's
      stated purpose.
- [ ] Exfiltration combos (`messages.read`/`files.download`/`filesystem` +
      `net.egress`) are **intentional** and the user confirmed them.
- [ ] `manifest.json` is valid: every `ui.*`/`settings`/`commands` entry has its
      capability; `messages.read` has a `scope`; `events` entries match held caps.
- [ ] **All eight** guest exports are implemented; `on-message` is **idempotent**.
- [ ] It builds (`cargo component build --release --target wasm32-unknown-unknown`)
      and `mod.wasm` sits next to `manifest.json`.
- [ ] It runs under **explicit** grants (not only `--grant-all`) and behaves
      sanely when a capability is **missing** (branch on `list-grants`, tolerate
      `permission-denied`).
- [ ] `wit/slaq-mod.wit` is a current, byte-identical copy of the host's.
- [ ] No reserved capabilities; no promises the host can't keep.

When these hold, show the user the final `manifest.json`, the `src/lib.rs`, and
the exact grant/run command for their case.
