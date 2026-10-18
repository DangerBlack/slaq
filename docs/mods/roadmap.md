# slaq mods — roadmap

Status: Phases 0–6 shipped (2026-10-18; first written 2026-10-10). The **engineering plan**: the order of work, what each
step proves, and — for every capability — *when* it ships. The capability list,
its stability and its slices are normative in [`capabilities.md`](capabilities.md);
this document says when each one lands. The *why* is in
[`00-analysis.md`](00-analysis.md); the contract is in [`api-v0.md`](api-v0.md)
(WIT) and [`manifest.schema.json`](manifest.schema.json).

## How work lands

- Every step is a **short PR onto `main`**, never a long-lived feature branch.
- Every step is behind the **`mods` feature flag (off by default)**: the default
  build is unaffected, so landing on `main` is safe. The flag *is* the isolation.
- A step lands only if it is **coherent**, **validated** (compiles and runs) and
  **inert without the flag**.
- **Cadence**: the *foundation* steps (1a–1d) bundle the capabilities that cannot
  stand alone — nothing is provable without the spine, the UI and enforcement.
  After the foundation, capabilities land **one (or a small coherent group) per
  PR**.

## Phase 0 — decision record ✅

Analysis, manifest v0, WIT, schema, registry, example. Merged as PRs #2 and #4.

## Phase 1 — substrate (slice 1)

**Foundation PRs** (not capabilities — they make capabilities possible):

| PR | What | Proves | Status |
|---|---|---|---|
| 1a | Host spine, headless, behind `mods` | wasmtime/WIT/toolchain; the spine runs | ✅ host spine + out-of-process compile worker (PR #7) |
| 1b | GUI wiring + **real** messages | integration + the payoff | ✅ mods receive the on-screen messages; decorations render (PR #7) |
| 1c | **Consent + capability enforcement** | the security model (the crux) | ✅ enforcement + grants store + `slaq mods` CLI + in-app consent prompt (GUI/TUI) (PRs #8, #9) |
| 1d | Real events (WebSocket subscriptions) | mods react live | ✅ the `events` subscription + delivery: `message.created`/`.updated` (per-event, via `on-message`), `channel.opened`/`.updated` (via `on-event`) and `reaction.added`/`.removed` (via `on-reaction`), fed by the app's flows; a dedicated Slack WebSocket relay is future work |

**Capabilities delivered in Phase 1** — each its own small PR, once 1c is in:

| Capability | Group | Status |
|---|---|---|
| `messages.read` (scoped) | Read | ✅ |
| `channels.list` | Read | ✅ |
| `users.list` | Read | ✅ |
| `net.egress` | Network | ✅ |
| `store.kv` | Storage | ✅ |
| `store.vector` | Storage | ✅ |
| `ui.settings` | Interface | ✅ |
| `ui.view` | Interface | ✅ |
| `ui.message_action` (+ `action` callback) | Interface | ✅ |
| `ui.message_decoration` | Interface | ✅ |

## Phase 2 — chat-ops (slice 2)

**Complete** (all except `search.read`, deferred); each its own PR:

| Capability | Group | Status |
|---|---|---|
| `commands.slash` (+ `command` callback) | Commands | ✅ |
| `chat.send` | Write | ✅ |
| `reactions.write` | Write | ✅ |
| `ui.composer_action` | Interface | ✅ |
| `ui.context_menu` | Interface | ✅ |
| `ui.dialog` | Interface | ✅ |
| `notify` | Interface | ✅ |
| `search.read` | Read | ☐ deferred (no consumer yet; a mod can search what it already reads) |

## Phase 3 — files (slice 3)

**Complete**; each its own PR:

| Capability | Group | Status |
|---|---|---|
| `files.upload` (slice 1: `file-upload`; payload-bound token is a follow-up) | Write | ✅ |
| `files.download` (opaque file id; slice 1: `file-get`) | Read | ✅ |
| `chat.forward` (slice 1: `chat-forward`, ambient) | Write | ✅ |

## Phase 4 — arbitrary SQL (slice 4)

**Complete** — `store.sql` ships with the hardening table of
[`manifest-v0.md`](manifest-v0.md) implemented and tested (defensive mode,
authorizer denials incl. `_slaq_*`, `SQLITE_LIMIT_*`, quotas). Own PR.

## Phase 5 — filesystem (slice 5)

**Complete** — `filesystem` ships: named slots the **user** maps to real
directories, with the host performing all disk I/O and containing every access
(no `..`, no absolute path, no symlink escape, slaq's own config/data dirs
denied, per-slot access, `create_new` writes, bounded list/read/search). The
guest never sees a path. Own PR.

| Capability | Group | Status |
|---|---|---|
| `filesystem` (`fs-list`/`fs-read`/`fs-write`/`fs-search`; user-mapped slots) | Filesystem | ✅ |

## Phase 6 — the runtime and the user's control (0.8–0.9) ✅

How mods **run** and how much the **user** controls them; the design is in
[`execution-model.md`](execution-model.md).

| What | Release | Status |
|---|---|---|
| No CPU budget: fuel dropped; generous, configurable backstops (`SLAQ_MOD_DEADLINE_SECS`, `SLAQ_MOD_MEMORY_MB`) | 0.8.0 | ✅ |
| One OS thread per mod; mods run concurrently | 0.8.0 | ✅ |
| `background` (+ the `background.tick` event): a consented mod is woken every `intervalSecs` | 0.8.0 | ✅ |
| Cancellable host I/O; **Running mods** window (state, Stop, Reload) | 0.8.0 | ✅ |
| The user picks a mod's channels and hosts in the app (`slaq mods scope`/`egress`), beyond the manifest | 0.8.1 | ✅ |
| Saved secrets survive a mod update (one-click keep) | 0.8.2 | ✅ |
| `history` / `thread-replies` (past messages, paced per mod) and `vec-delete` | 0.9.0 | ✅ |
| Secrets in the real OS keyring (they were held in memory before 0.9.0) | 0.9.0 | ✅ |
| Agent bridge (`agent.bridge`) | — | proposed (execution-model Slice 4) |

## Reserved — names held, not shipped

Requested manifests are refused at load. Promoted only when a real mod needs
them (see the reserved table in `capabilities.md`): `chat.edit`, `chat.delete`,
`presence.read`, `pins.*`, `saved.*`, `profile.status.write`, `scheduled.*`,
`huddle.*`, `webhook.receive`, `messages.sql`, plus the `channel.member_joined`
event and the `ui.*` descriptors not yet needed.

## Later — marketplace (gated)

Signing, registry, review. **Do not start** unless: a real third party wants to
distribute a mod; or more than ~3–4 external mods are wanted together; or the
product explicitly becomes a platform.

## Reference mods (dogfooding)

The first-party features are the mods that validate the contract — and the
reason Phases 1b/1c must be right:

- **translation** (Phase 1–3: read + network + decoration, then composer + send),
- **memory / decision ledger** (Phase 4: storage + SQL + view + references),
- **docs search** (Phase 5: `filesystem` — on each message it searches the
  user-mapped `docs` slot and decorates the first hit with file and line; the
  `filesystem` reference mod).
- **Second Brain** (Phase 6: history + background + vector/SQL stores +
  filesystem + network: cited notes from Slack, code and meeting notes; its own
  repository, published on the download page).
- **light theme** (`ui.theme`: the example in
  [`examples/light-theme/`](examples/light-theme/), loaded by the test suite).

## Out of scope for now

- **Internal seams** (`TranslateProvider`, `EmbeddingProvider`, …): a separate
  track; the mod system does not replace them.
- **Marketplace / signing** (gated above).
- **Huddles, presence, calls** as mod capabilities (reserved names only).
