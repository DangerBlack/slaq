# slaq mod manifest — v0

Status: draft v0, 2026-10-10. **Slice-1 note:** the host reads
`manifestVersion`, `id`, `name`, `version`, `apiVersion`, `entry`, `capabilities`,
`settings`, `ui`, `events` and `commands`. `capabilities` is parsed, **validated**
(an unknown, reserved or not-yet-implemented name refuses the package at load) and
**enforced** (default deny) for every capability the host implements (the
list is normative in [`capabilities.md`](capabilities.md)). To subscribe to an event you
must also request its capability; a non-empty `settings`/`ui.*` array or a
non-empty `commands` requires its `ui.*`/`commands.slash` capability. Each mod's
KV and vector stores are **persistent** (`~/.local/share/slaq/mods/<id>/store.db`,
opened lazily, surviving a reload), each bounded by its declared `maxBytes`.
Grants are applied when the host loads the mods: `slaq mods
grant`/`revoke` take effect on the next load (live revocation mid-session is not
implemented yet), and the recorded grants drive runtime enforcement (managed with
the `slaq mods` CLI and the in-app consent prompt). See
[`api-v0.md`](api-v0.md).

The **capability vocabulary** is normative in
[`capabilities.md`](capabilities.md); the **manifest shape** is normative in
[`manifest.schema.json`](manifest.schema.json); the **runtime signatures** are
normative in [`api-v0.md`](api-v0.md). This document is the manifest's shape and
its behavioural rules — it does not restate the capability list.

A mod is a WASM component plus a **manifest**. The manifest is *declarative*: it
says what the mod wants and what it contributes, before any code runs. It is the
basis of the install-time consent prompt and of runtime enforcement. It never
executes anything.

Two axes are kept separate:

- **Declared** — a capability exists in the vocabulary; a manifest may request
  it.
- **Enforced** — the host actually performs the operation.

A capability the host does not implement **cannot be requested**: the manifest is
rejected at load, and the user is told which capability is missing. This is what
lets the vocabulary stay complete while the runtime ships in slices.

Two terms are kept separate: a **capability** is what the manifest declares (and
the user grants); a **host operation** is the runtime function it unlocks.

---

## Design rules

1. **Default deny.** Anything not declared is not callable. A call without a
   granted capability is denied and logged.
2. **Scope, never booleans.** `messages.read` and `net.egress` are always
   restricted.
3. **Per-capability consent**, grouped in the prompt, shown at install from the
   manifest, re-checked at runtime, revocable.
4. **Dangerous combinations are flagged.** Any content-bearing Read capability
   (`messages.read`, `search.read`, `files.download`) + `net.egress` = potential
   exfiltration; approved as a *combination*, with an explicit warning.
5. **Unknown or unavailable capability ⇒ the mod does not load.** Fail closed.
6. **Capabilities are additive and versioned.** Adding one is safe; removing or
   changing one is a major `apiVersion` bump.
7. **`apiVersion` is `major.minor`.** Host and mod announce their versions;
   incompatible ⇒ no load.
8. **Host secrets are never exposed.** `xoxc`/`xoxd`, sockets and the host DB are
   unreachable. The filesystem is not exposed *as such*: a mod reaches disk only
   through the `filesystem` capability's **user-mapped slots**, and the host
   performs the I/O, contains it (no `..`, no absolute path, no symlink escape,
   slaq's own dirs denied) and bounds it. The guest never sees a path.
9. **Outbound: stop and propose, never rewrite.**

---

## Capabilities, stability and consent

The vocabulary, the stability levels (`stable` / `draft` / `reserved`) and the
consent groups are normative in **[`capabilities.md`](capabilities.md)**. In
short: `stable` is implemented and frozen, `draft` may still change, `reserved`
is a name a manifest may **not** request (the host rejects it at load); the
install prompt groups capabilities, and a content-bearing Read + `net.egress`
together is flagged as an exfiltration combination.

---

## Full example

```json
{
  "manifestVersion": 0,
  "id": "com.example.translate",
  "name": "Auto Translate",
  "version": "0.1.0",
  "apiVersion": "0.1",
  "minSlaq": "0.4.0",
  "description": "Translates incoming messages.",
  "author": "Example",
  "homepage": "https://example.com/translate",
  "license": "MIT",

  "entry": { "type": "wasm", "path": "mod.wasm" },
  "locales": ["en", "it"],

  "capabilities": {
    "messages.read":     { "scope": "channels", "channels": ["C0ENGINEER", "D0ME"] },
    "store.kv":          { "maxBytes": 2097152 },
    "net.egress":        ["localhost:11434"],
    "ui.message_action": {},
    "ui.message_decoration": {},
    "ui.settings":       {}
  },

  "events": ["message.created", "message.updated"],

  "ui": {
    "messageActions": [
      { "id": "show-original", "label": "i18n:show_original" },
      { "id": "retranslate",   "label": "i18n:retranslate" }
    ]
  },

  "settings": [
    { "key": "targetLang", "type": "string", "default": "it",  "label": "i18n:target_lang" },
    { "key": "mode",       "type": "enum",   "default": "auto",
      "values": ["auto", "on_demand"], "label": "i18n:mode" },
    { "key": "endpoint",   "type": "url",    "default": "http://localhost:11434/v1" },
    { "key": "apiKey",     "type": "secret", "label": "API key" }
  ]
}
```

---

## Top-level fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `manifestVersion` | int | yes | `0` for this spec. |
| `id` | string | yes | Reverse-DNS, unique. |
| `name` | string | yes | Display name (host may localize via `locales`). |
| `version` | semver | yes | The mod's own version. |
| `apiVersion` | major.minor | yes | Host API contract the mod targets. |
| `minSlaq` | semver | no | Minimum client version. Enforced at load from 0.9.0: an older slaq refuses the package and says which version it needs. |
| `description` | string | no | Shown in the install prompt. |
| `author` / `homepage` / `license` | string | no | Provenance. |
| `entry` | object | yes | `{ "type": "wasm", "path": "mod.wasm" }`. |
| `locales` | string[] | no | Language tags the mod ships. |
| `capabilities` | object | yes | See `capabilities.md` and the schema. |
| `events` | string[] | no | Passive event subscriptions (see `capabilities.md`). |
| `ui` | object | no | Static UI contributions. |
| `settings` | object[] | no | Settings schema (incl. secrets). |
| `commands` | object[] | no | Slash commands. |

---

## Capability behaviour

The *list* is in `capabilities.md`; the behaviour that belongs to the manifest
contract:

- **`messages.read` requires a scope object**: `{ "scope": "all" }` or
  `{ "scope": "channels", "channels": ["C…", "D…"] }`.
- **`net.egress` is an allow-list of hosts.** A host not on it triggers a runtime
  consent prompt (or is denied). Egress is enforced **per connection**: the host
  resolves the name and checks the **address class** on every connect and
  redirect (DNS rebinding is refused). `localhost` is never implicit.
- **Every mutating chat call takes a host-minted, single-use activation token**
  bound to the operation and target; there is no "remember for this mod". The
  token is what enforces "stop and propose, never rewrite".
- **`files.download` takes an opaque Slack file id, never a URL** (a
  mod-controlled URL would be a second fetch primitive that bypasses
  `net.egress`); the host resolves it and rejects non-Slack targets.
- **`files.upload` requires a payload-bound activation token**: the host shows a
  preview/confirmation and binds the token to the channel and the content, so an
  upload cannot happen from `start` or a passive event. *(Slice 1 ships
  `file-upload` **ambient** — the grant is the authorization, like `chat.send`,
  with a size cap, the read scope and the per-dispatch write budget; the token and
  its preview are a follow-up.)*
- **`store.sql` runs only against the mod's private DB** (see Storage).
- **`filesystem` reaches disk only through user-mapped slots** (see Filesystem):
  the mod names a slot and a **relative** path; the host resolves it against the
  directory the user mapped, contains it, and performs the I/O. The guest never
  sees a path, and `filesystem` + `net.egress` is an exfiltration combination.
- **Events**: a mod receives a passive event only if it subscribed *and* holds
  the event's capability within scope (table in `capabilities.md`);
  `action`/`command` callbacks are delivered implicitly for declared UI/commands.

---

## Settings and secrets

The host renders a settings form from `settings`; the mod never draws.

Field types: `string` · `number` · `bool` · `enum` · `url` · `secret`.
Runtime values are typed (`text` / `integer` / `real` / `flag`); secrets read
back as `text` (`api-v0.md`).

- **Non-secret** values are stored in the mod's config area and readable/writable
  by the mod (`settings.get` / `settings.set`).
- **Secret** values are stored by the **host** in the system keyring, namespaced
  `slaq:mod:<id>:<key>`, and delivered to the mod at runtime (`settings.get`).
  Only the **user** writes them (the form can set, replace or clear a secret);
  the mod can read but not set them. A secret
  is **bound to the package fingerprint** (a hash of both the raw manifest bytes
  and the component bytes) recorded when
  the user entered it: because packages are unsigned, a replacement that reuses
  the `id` cannot read the previous package's credential on its own. When the
  package changes (an update or a rebuild), slaq asks once — *Mod updated: keep
  its saved secrets for this version?*, showing the hosts it can reach — and
  the new package runs without them until the user keeps them (one click; the
  keyring value is untouched) or forgets them. CLI: `slaq mods keep-secrets
  <id>` / `slaq mods forget-secrets <id>`; `slaq mods list` shows what waits.
- The form masks secrets and never logs them.

**Two classes of secret** (see `00-analysis.md` §6.1):

- **Mod secret** — the user typed it for *this* mod; the mod may read it. The
  install prompt says explicitly: *"this mod will be able to read this key"*.
- **Host secret** (`xoxc`/`xoxd`, shared org keys) — **never** exposed, by any
  manifest.

### Providers (LLM and other APIs)

There is no mandatory host-side provider registry. A mod declares its endpoint
(a `url` setting, or a `net.egress` host) and, if needed, a `secret`, then calls
`http.fetch` itself. This keeps slaq provider-agnostic: a mod (or an LLM writing
one) needs no adapter for a specific vendor, and each mod may use a different
provider. A host-level named-provider registry is a possible **optional**
convenience for the future; it is not required.

---

## Filesystem

`filesystem` gives a mod access to **named slots**, each mapped by the **user** to
a real directory. The mod passes `(slot, relative-path)` and the **host** performs
every disk access: the guest is wasm with no filesystem, so it never sees a path.
The manifest declares the slots and their access:

```json
"filesystem": { "slots": [ { "name": "inbox", "access": "read" },
                           { "name": "outbox", "access": "write" } ] }
```

A slot `name` is a label (1–32 bytes, `^[a-z0-9]([a-z0-9_-]*[a-z0-9])?$`, unique),
not a path. There are at most **8** slots, each `read` or `write`; `fs-list`,
`fs-read` and `fs-search` need a `read` slot, `fs-write` a `write` slot. The user
maps each declared slot to a directory from the in-app consent dialog — the GUI
has a native folder **picker** (and a **Clear** to unmap), the TUI shows the
current mapping read-only — or with `slaq mods map <id> <slot> <dir>` from the
CLI (the effective mapping is the intersection of the manifest's declared slots
and the user's mapping, so an update cannot add a slot the user never mapped, and a
slot the manifest drops is dropped). The **access** is intersected too: the
enforced access is the narrower of the manifest's and the one in force when the
slot was mapped, so an update cannot widen a mapped slot from `read` to `write` on
its own (re-mapping is how the user approves a wider access). A mapping is
**independent of the capability checkbox**: it is persisted as soon as the slot is
mapped, and granting `filesystem` in the prompt stays **inert until a slot is
mapped** — the runtime intersects the grant with the mapping. The prompt lists the
declared slots and flags the `filesystem` + `net.egress` exfiltration combination.

For a **one-shot** headless run there is no persisted mapping: `slaq mod` accepts
`--fs-slot NAME=DIR` (repeatable, `NAME` a slot the package declares) to map a
slot for that run only, e.g.
`slaq mod --dir ./mod --fs-slot docs=/tmp/docs-demo --grant filesystem`. It is the
one-shot counterpart of `slaq mods map`.

**Enforcement is containment (all of it is implemented; the stability label stays
`draft`):**

| Defence | Mechanism |
|---|---|
| No path traversal | the guest path is relative; only `Normal` components are accepted, `..` is refused ("path escapes the slot") and an absolute path is refused |
| No symlink escape | every resolved path is `canonicalize`d (symlinks resolved) and must still be under the slot root; the list/search walk never lists nor traverses a symlink, so it cannot escape or loop |
| No slaq data | slaq's own config dir and data dir (consent store, session) — and the credential file's hard-coded `$HOME/.config/slaq` — are **denied prefixes**: a slot whose root is inside one is inert, and a resolved path inside one is refused, even when the directory does not exist yet and even under a non-default `XDG_CONFIG_HOME` — so mapping `~` cannot hand a mod slaq's files |
| Bounds | a guest path ≤ 4096 bytes; `fs-list` ≤ 4096 entries (more is an error, never a truncation; the walk does not materialize a wider directory); `fs-read` refuses a file over 4 MiB (never truncates, with a bounded read); `fs-write` creates ≤ 4 MiB; `fs-search` pattern ≤ 256 bytes, files ≤ 1 MiB, ≤ 64 MiB / 4096 files scanned and ≤ 512 hits (exceeding a cap is an error) |
| Per-slot access | each operation checks the slot's access, **intersected** with the access in force when the slot was mapped (a later manifest cannot widen `read` → `write`); a missing slot or the wrong access is an error (never a panic) |
| No special files | a FIFO/socket/device is never listed and never opened (`File::open` on a FIFO with no writer would block the host thread forever) |
| No overwrite | a write is `create_new` (`0600` on Unix): an existing file, or a symlink at the leaf, is refused; the parent directory must already exist |

`filesystem` is **High** risk; combined with `net.egress` it is an **exfiltration**
combination (the host contains where the mod reads, not where it sends), flagged
in the consent prompt.

---

## Storage

Each mod gets its **own** storage, opened and owned by the **host**; the mod
never sees a path. There are two files, so their quotas are independent:

- `~/.local/share/slaq/mods/<id>/store.db` — host-managed KV and vector tables,
  bounded by `store.kv.maxBytes` and `store.vector.maxBytes`;
- `~/.local/share/slaq/mods/<id>/scratch.db` — arbitrary SQL (`store.sql`),
  bounded by `store.sql.maxBytes`.

Separate files matter: `max_page_count` caps a whole file, so if SQL and
KV/vector shared one DB, SQL growth could consume space declared for those
stores. Every level is batched (`*_many`) because crossing the host boundary per
row is the one real performance cost (see `00-analysis.md` §5.1).

The declared `maxBytes` is a **logical** quota on the stored payload (keys +
values, vectors + meta); SQLite's own page/index overhead makes the physical file
a little smaller in practice — for many very short rows the file cap can bind
before the declared budget — so treat `maxBytes` as an upper bound, not a
byte-exact one. Each quota is clamped to a **64 MiB** host maximum. A mod's
data dir is owned by one running slaq instance; a second process sharing it waits
on a busy timeout but is not a supported configuration.

### `store.kv` — key/value
`get` / `set_many` / `delete` / `scan`, namespaced, capped by `maxBytes`.
Slice 1 ships only `get`/`set`, and the store is **persistent**: a mod that fills
its declared `maxBytes` cannot free space until `delete`/`scan` land (a
`slaq mods reset <id>` to drop a mod's data is the follow-up), so a buggy mod can
wedge its own store.

### `store.vector` — embeddings
`upsert_many` / `search(vec, k, filter)`, declared `dim` and `maxVectors`.
Backed by SQLite (BLOB + cosine, or HNSW) — host-side.

### `store.sql` — arbitrary SQL, private DB only (implemented; slice 4)

```wit
sql-query: func(query: string, params: list<sql-value>) -> result<sql-result, error>
sql-tx:    func(statements: list<tuple<string, list<sql-value>>>) -> result<list<sql-result>, error>

variant sql-value { null, int(s64), real(f64), text(string), blob(list<u8>) }

record sql-result { columns: list<string>, rows: list<list<sql-value>>, changes: u64 }
```

Rules:

1. **Private DB only.** The host never attaches the host cache or any other DB
   to that connection.
2. **One statement per call.** Transactions go through `sql-tx`.
3. DDL allowed (it is the mod's DB). The host's `_slaq_*` tables (its KV/vector
   stores, which live in `store.db`) are a reserved namespace the **authorizer
   denies to SQL entirely** — belt and braces, since `scratch.db` is a separate
   file.
4. Quotas: `maxBytes` (`max_page_count`) on `scratch.db`, a per-result row cap
   (`maxResultRows`) and an aggregate **result-byte** bound, so a computed value
   (`SELECT zeroblob(...)`) cannot be copied across the WIT boundary without
   limit; a statement/transaction timeout; executed off the UI thread. Explicit
   `CREATE TEMP …` is **denied** (a temp table lives in a process temp file,
   outside `scratch.db` and `maxBytes`); a query's own transient spill is freed
   when the statement ends and is bounded by the timeout.
5. **Separate permission with an explicit warning.**

**Enforcement is gated by hardening (all of it is implemented; the stability
label stays `draft`):**

| Defence | Mechanism |
|---|---|
| No native code | `load_extension()` stays unauthorized (the per-connection flag is off by default) and the authorizer denies the function outright |
| No external files | authorizer denies `SQLITE_ATTACH` / `SQLITE_DETACH` |
| No **persistent** temp storage | authorizer denies `CREATE TEMP …` (a temp file is outside `maxBytes`); a query's transient spill is deadline-bounded |
| No dangerous schema writes | `SQLITE_DBCONFIG_DEFENSIVE` |
| No arbitrary PRAGMA | authorizer denies non-allow-listed pragmas (and the *writes* to `page_size`/`encoding`) |
| Reserved namespace | authorizer denies reads, writes and schema ops on `_slaq_*` |
| Complexity and value limits | `SQLITE_LIMIT_SQL_LENGTH`, `..._COMPOUND_SELECT`, `..._EXPR_DEPTH`, `..._COLUMN`, `SQLITE_LIMIT_LENGTH` |
| Timeout + quota | `progress_handler` (one deadline per call), `max_page_count`, per-result row cap (`maxResultRows`), aggregate result-byte cap |

`store.sql` is scoped to the mod's own `scratch.db`. It is **not** a door onto
Slack messages: those stay behind the `messages.read` capability (scoped) and
the `search.read` capability.

---

## Declarative UI

Mods never draw. They submit **data** the host renders in both front ends
(egui and ratatui). The content schema is a small, purpose-built subset — *not*
Slack's Block Kit: slaq parses incoming Block Kit to render messages, but a
decoration or dialog is a separate surface. Block types (`section`, `context`,
`message-ref`, `divider`) are specified in `api-v0.md`.

Static contributions are declared in the manifest `ui` object, each gated by its
capability:

| `ui` field | Capability |
|---|---|
| `messageActions[]` | `ui.message_action` |
| `contextMenu[]` | `ui.context_menu` |
| `composerActions[]` | `ui.composer_action` + `messages.read` (the read scope bounds the draft) |
| `views[]` | `ui.view` |
| `theme` | `ui.theme` |

`ui.dialog` has no manifest field: a mod declares the capability and shows a
modal at run time with `ui-show-dialog` (see `api-v0.md`).

`ui.theme` is an object of role → `#rrggbb` overrides applied to the default
palette (`design-system/slaq/MASTER.md` lists the roles). It is **lenient**: an
unknown role or a malformed value is ignored, so a partial theme is fine, and the
host does not police contrast — a theme that makes the app unreadable is the
user's to remove.

Declaring a non-empty `settings` array requires `ui.settings` (the tab that
renders it). The host rejects a manifest that contributes UI, or settings,
without the matching capability.

---

## Versioning and vocabulary extensibility

- `apiVersion` is `major.minor`. The host advertises its supported range; a mod
  outside it does not load.
- Capabilities are **namespaced** (`domain.action`) and **additive**. Adding one
  is a minor bump; removing or changing semantics is a major bump.
- A manifest may request only `stable`/`draft` capabilities the host implements.
  `reserved` capabilities are refused; unknown ones too (fail closed).
- The host exposes the **granted** capability set through the `grants.list` host
  import (spelled `list-grants` in the slice-1 WIT) so mods can branch on partial
  consent.

This is what lets v0 be broad without over-committing: the vocabulary can grow,
and a mod's dependency is always on something a host of its `apiVersion` will
actually run.

---

## Implementation slices

The slices are in **[`capabilities.md`](capabilities.md)** (slice 1 substrate,
2 chat-ops, 3 files, 4 `store.sql`, 5 reserved → stable). Slice 1 is the only
one that must be right before opening the runner; the rest are additive.

---

## Not in v0 (by design)

Raw SQL on host data · raw filesystem access (only user-mapped, host-contained
`filesystem` slots) · direct sockets · Slack credentials ·
arbitrary UI code · boolean permissions · auto-update · silent outbound
transform.

---

## Validation against the mod classes

v0 must hold for *representative* mods, not just the first two: a translation mod
(read + network + decoration), a memory mod (read + storage + view + references),
a command mod (slash + reply), an image mod (files.upload/download + network),
and a file mod (upload + storage).
