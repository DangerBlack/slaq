# slaq mods — analysis and recommendation

Status: the **analysis** (2026-10-10), kept as the decision record. The mod
system it recommends shipped (from 0.6, in `src/mods/`).
- **What changed since:** how mods *run* was reworked in 0.8; that supersedes
  the execution and trust sections here (see
  [`execution-model.md`](execution-model.md)).
- **The contract** actually implemented is [`api-v0.md`](api-v0.md). It
  diverges in naming from the full design below.

This document answers one question: *should slaq grow an extension ("mod")
system, and if so, how?* It is grounded in the current code (references point at
real files and functions) and in what users say about Slack.

Related documents:

- [`manifest-v0.md`](manifest-v0.md) — the mod manifest specification.
- [`manifest.schema.json`](manifest.schema.json) — machine-readable schema.
- [`examples/translate-mod.md`](examples/translate-mod.md) — a worked example.

---

## TL;DR

**Recommendation: treat extensibility as the product, but build it in stages —
seams first, mods second, marketplace (maybe) third.**

- Mods are worth it, but *not* because they deliver the first two features
  faster. They are worth it because they are the cure for the failure mode that
  kills every alternative Slack client: a long tail of features one maintainer
  cannot cover.
- Mods are **100% untrusted code**. The host is the only privileged actor.
  Mods never see `xoxc`/`xoxd`, files, sockets, or the host database.
- Execution model: **WASM** (wasmtime + component model). Rust has no stable
  ABI, so `dlopen`-style plugins are out; a WASM host gives structural
  isolation instead of a promise.
- The valuable asset is **the contract** (manifest + host API + declarative UI),
  not the loader. Design the contract first, dogfood it with built-in features.
- Sequence: (1) internal seams, (2) translation + memory as first-party mods,
  (3) a **local** runner (load a mod from a folder, no marketplace), (4) a
  marketplace only if a real community appears.
- Before any of that: the core must be fast and reliable. Mods do not fix the
  complaints users actually voice loudest (performance, crashes).

---

## 1. Context

`slaq` is a native Slack client in Rust. Its original wedge was memory
footprint (~113 MiB vs ~1 GB for the official Electron client), but the market
signal is broader: people want **a Slack they can shape**. The official client
is a web page the user cannot change, cannot revert, and increasingly cannot
even load quickly.

The question is not "native features or mods" as a binary. It is whether
extensibility is *a feature* or *the identity* of the product. For a client that
wants to be "the Slack you can remake", it is the identity — and then the mod
system is the thing you build.

## 2. How slaq is built (the seams that matter)

Two front ends over one core:

- Core: `src/app/mod.rs` — `App` + `AppState` (`Arc<RwLock<AppState>>`), mutated
  by background tokio tasks; `wake()` signals the front end. The core does not
  know whether a GUI or a TUI is attached.
- GUI: `src/gui/mod.rs` — each frame copies state into a `Snapshot`
  (`Snapshot::refresh_from`) and draws. Message text goes
  `Gui::message` → `Gui::body` → `mrkdwn::parse` → `Gui::spans`.
- TUI: `src/ui/mod.rs` — same core, different renderer. This is why *mod UI must
  be declarative*: code cannot draw into two different toolkits.
- Store: `src/store/mod.rs` — SQLite cache, messages kept as raw JSON. The DB is
  explicitly a *cache*, deletable at any time. Any derived data (translations,
  embeddings) must therefore live in separate, rebuildable tables.
- i18n: `rust-i18n`, `locales/<surface>/<lang>.yml` (source language Italian,
  fallback `it`). Mods will need to localize their strings the same way.
- Network: `src/api/client.rs` (Slack only). One hard rule in
  `src/net.rs::is_slack_host`: the `xoxd` cookie goes to Slack hosts only.

Natural extension seams, by function:

| Concern | Point in code |
|---|---|
| Ingest events | `App::apply_ws_update` |
| Write to cache | `Store::upsert_messages` |
| Render a message | `Gui::message` / `Gui::body` / `mrkdwn::parse` |
| Send | `App::send_message`, `Gui::send` |
| Slash commands | `split_slash_command` |
| Read data | `Store::messages`, `Store::thread_messages` (cached); `App::search` → `Client::search_messages` (network) |
| Extra views | `CenterView` (Mentions/Threads/Saved…) |

## 3. What users say about Slack

Sources are linked; treat them as qualitative community signal, not hard market
data.

### 3.1 The complaints are real and cross-cutting

- **Performance / bloat / crashes**: "300–500 ms to switch a channel", daily
  crashes, Electron overhead. Slack itself publishes
  ["Reducing Slack's memory footprint"](https://slack.engineering/reducing-slacks-memory-footprint/).
- **Imposed UI**: the 2023 redesign produced a documented backlash
  ([Fast Company](https://www.fastcompany.com/90972862/four-ways-the-confusing-slack-redesign-is-making-it-hard-to-work-today),
  [VentureBeat](https://venturebeat.com/business/slack-lash-slack-defends-controversial-redesign-amid-sharp-criticism)),
  and the sharpest line is *"Slack does not allow users to revert"* — the pain
  is loss of control, not the pixels.
- Pricing, 90-day history limits, support.

None of this is fixed by mods. These are **fundamentals**: the core must be
fast and reliable first.

### 3.2 The finding that favors mods

Every **alternative client** dies the same death:

- [Shrugs.app](https://news.ycombinator.com/item?id=31926594) (native macOS
  client): wanted, but the thread is a litany of missing features — edit,
  XOXC, search, huddles, screen sharing; "maintenance mode"; *"Would not
  recommend."*
- Ripcord: one developer, missing search and huddles. A representative comment:
  *"I'm tired of being the person who doesn't have a specific feature."*

**The way alternative Slack clients die is exactly the long tail of features one
maintainer cannot cover.** Mods are the mechanism that covers that long tail
with a community. This is a stronger argument for mods than the RAM angle.

### 3.3 The counter-finding you must accept

Slack's moat is **network effects + integrations**, not the client:

- ["OpenAI should build Slack"](https://news.ycombinator.com/item?id=47012553):
  *"chat is a commodity"*, *"the real reason everyone is on Slack is network
  effects and integrations"*.
- [Show HN: Dock](https://news.ycombinator.com/item?id=46671952): *"the Slack
  app ecosystem and the general ease of integrating literally anything into it
  is the revolution of the whole thing."*

So mods make **your** client better; they do not move your team. They win the
"my Slack" segment, not the "replace Slack" segment.

### 3.4 The market also validates the architecture

- [The Anatomy of an Extension Point](https://getforge.com/blog/anatomy-of-an-extension-point/)
  (2026) cites Slack's **Block Kit** as the exemplary pattern: a *declarative
  schema*, not a render API; apps cannot inject arbitrary UI or touch other
  messages. Extension points become leverage only "backed by a stable contract,
  with a clear boundary on both sides."
- Slack's own
  [optional scopes](https://slack.dev/introducing-optional-scopes-for-slack-apps/)
  (2026): installing an app is "a trust decision", and all-or-nothing scopes
  make users walk away — the same logic behind per-capability consent.
- [Juggler](https://news.ycombinator.com/item?id=48883305) (2026): a native
  desktop app (no Electron) with JavaScript plugins — a working precedent.
- **AI memory is table stakes in 2026**: Den (YC), OneCamp "Catch Me Up", Slack
  AI. The "intelligent memory" feature is not a whim; it is expected.

## 4. Native vs mod — the honest comparison

| | Native | Mod system + marketplace |
|---|---|---|
| Time to ship the two features | short | **long** (infrastructure first) |
| Cost per future feature | low | low (mods) / high (host) |
| Third-party ecosystem | none | yes |
| Security surface | contained | **multiplied** |
| Binary | grows with features | lean core, downloaded features |
| Maintenance | low | high and ongoing (API, sandbox, registry) |

The mod system does not ship the two features sooner; it enables an ecosystem.
It is justified by the *identity*, not by these two cases. The concrete needs
that motivate it (S3 upload, encryption) are themselves reminders of the two
categories below.

## 5. Execution model: why WASM

Rust has **no stable ABI**: you cannot safely `dlopen` a Rust `.so` and call
trait objects across versions. Realistic options:

1. **WASM Component Model** (`wasmtime` + WIT) — real sandbox, cross-language,
   capability-based, typed interfaces. **Chosen.**
2. **Out-of-process sidecar + JSON-RPC** — no ABI issues, crash isolation, but
   not a sandbox by itself (needs OS isolation).
3. Embedded scripting (rhai/lua/quickjs) — language lock-in, weaker sandbox.
4. `libloading` + C ABI — **rejected**: no sandbox, fragile ABI, `unsafe`, at
   odds with this project's security posture.

### 5.1 Performance (wasmtime, Linux x86_64)

| Dimension | Overhead vs native |
|---|---|
| CPU steady-state | ~10% (median), up to ~20% |
| Host-call I/O | 2–3× per call (every "syscall" goes through a host shim) |
| Boundary copy | the real cost — batch, don't cross per item |
| Startup | AOT-precompiled **<1 ms**; JIT 15–50 ms |
| Memory | ~1–3 MiB per instance |

For thin orchestration mods (translation, memory) the overhead is
**imperceptible**: the dominant cost is the network round trip, not the sandbox.
Mitigations: AOT-precompile at install, pooling allocator, **fuel metering +
epoch interruption** (which double as timeouts/anti-DoS), a small per-instance
memory cap, and coarse-grained (batched) host APIs.

## 6. Trust model

Mods are **100% untrusted code**. Consequences, all non-negotiable:

1. **No ambient authority, no credentials in the guest.** A mod cannot reach the
   Slack session, the filesystem, the store or the network except through
   declared host functions. (The `filesystem` capability is no exception: the
   host performs the I/O against user-mapped slots the guest never sees a path
   for.) A wasmtime embedding runs the guest in the same OS
   process as the host (which holds `Credentials`), so WASM gives **memory**
   isolation and a capability boundary, not OS process isolation; the
   credentials are never handed to the guest. Hard process isolation — a
   separate credential-free runner over an IPC boundary — is a permitted
   deployment choice, not a v0 requirement.
2. **Host-mediated I/O.** Egress is declared or granted at runtime; secrets the
   host owns are never exposed.
3. **Declarative UI.** Mods contribute data; the host renders in egui *and*
   ratatui.
4. **Namespaced, quota'd storage.** Never the shared DB.

### 6.1 Secrets: two classes

| Class | Example | Visible to a mod? |
|---|---|---|
| **Mod secret** | the API key the user typed for *that* mod | yes (labelled) |
| **Host secret** | `xoxc`/`xoxd`, shared org keys | **never** |

A mod may own its key (blast radius = that key). A host secret is never handed
out.

### 6.2 Outbound rule

> Toward Slack, a mod may **stop** and **propose**, but **never rewrite** — and
> never anything **the user cannot see**.

This yields three distinct outbound primitives and excludes one:

- `chat.send` — the mod sends (target: only after a visible user action; slice 1
  ships it **ambient** — see the note below).

> **Slice-1 note (2026-10-12):** `chat.send` ships **ambient** — the capability
> grant **plus the channel scope** is the authorization, with **no** activation
> token and **no** preview (a deliberate divergence, decided with the user). A mod
> therefore posts as the user without a per-message visible action; the consent
> dialog (Write group) is the gate, and the user's own messages are **not**
> delivered back to mods, so a mod cannot re-trigger itself. No slice-1 write keeps
> the activation token yet: `chat.forward` and `files.upload` shipped **ambient**
> as well, like `chat.send`.
- `ui.set_composer_preview` / `ui.set_composer_text` — the mod proposes; the
  user applies.
- `chat.guard` (v1) — the mod may veto/warn, never edit.
- ~~silent outbound transform~~ — does not exist.

### 6.3 Threat-model delta

Extend `docs/threat-model.md`:

- New asset: mod-owned secrets (bounded; per-mod).
- New trust boundary: **the mod** is untrusted input/code, same class as a
  message body.
- New rule: message content may leave the machine only if the user enabled an
  egress path and the mod declared it.
- Dangerous combinations are surfaced at consent time: `messages.read` +
  `net.egress` = exfiltration; `chat.guard` + `net.egress` = every keystroke
  leaves.

## 7. The contract is the asset

What is hard, and what is worth building first, is **the interface**:

- a **manifest** that declares capabilities before code runs (for consent and
  review);
- a set of **host functions** (RPC) that are the only way a mod touches the
  world;
- a **declarative UI** descriptor so one contribution renders in two front ends;
- a **capability model** with scope, revocation, and dangerous-combination
  flags.

The loader is a deployment detail. If the contract is right, the execution model
(sidecar vs WASM) can change without rewriting mods.

### 7.1 Broad vocabulary, sliced implementation

A capability vocabulary cut to the first two features would make a system that
can host only those two features — i.e. a useless mod system. The vocabulary
must cover **the union of what any foreseeable mod needs** (read, write, network,
storage, declarative UI, commands, events), and it must be **extensible**
(namespaced, additive, versioned) so it can grow without breaking.

That breadth does not translate into breadth of *risk*, because two axes are
independent:

- **declared** (in the manifest vocabulary) — cheap, on paper;
- **enforced** (the host performs it) — expensive, and only a few are dangerous.

A capability the host does not implement is **rejected at load** (fail closed):
a mod never depends on an operation that is not there. So the manifest is
delivered in **slices** (see `capabilities.md`), each a PR, and each capability
carries a stability level (`stable` / `draft` / `reserved`). The prompt groups
capabilities so a large vocabulary does not become a large consent form.

## 8. Recommended sequence

```
1. Internal seams  (traits: TranslateProvider, EmbeddingProvider, UploadProvider…)
2. First-party mods → translation + memory written against the host API
3. Local runner     → load a mod from a folder; no marketplace, no signing
4. Marketplace      → only if a real third-party mod exists to distribute
```

- Step 1 captures most of the benefit (isolated, optional, swappable features)
  at a fraction of the cost, and keeps the door open.
- Step 2 dogfoods the API against the two reference features before freezing it.
- Step 3 is "make a mod, drop it in a folder" with zero trust cost: whoever
  writes a mod owns its risk, and the sandbox bounds it.
- Step 4 is deferred, possibly forever.

**Trigger to revisit the marketplace:** a real third party wants to distribute
a mod; or more than ~3–4 external features are wanted together; or the product
goal explicitly becomes "a platform".

## 9. Open questions

- A host helper for mrkdwn segmentation (`text.segments`) vs mods doing it.
- The streaming shape for large file and HTTP bodies (today `list<u8>`).
- `channel.member_joined` event (reserved) — needed by the memory mod's onboarding
  digest.
- Marketplace timing, signing, registry.
- Reserved / v1 candidates: `presence.read`, `pins.*`, `saved.*`,
  `profile.status.write`, `huddle.*`, `webhook.receive`, `chat.guard`,
  `messages.sql`.

Resolved since the first draft: `chat.send` consent (target: a host-minted,
single-use activation token; slice 1 ships it **ambient** — see §6.2);
`store.sql` (shipped in slice 4 with the hardening of `manifest-v0.md`; its
label stays `draft`); the WIT
signatures and the declarative UI content schema (now specified in `api-v0.md`).

## 10. Risks

- **Niche of a niche**: people willing to run a non-official client *and* write
  or install a mod. The staged plan makes this cheap to discover.
- **Reliability beats customization**: the same people who love the *idea* of
  Shrugs abandon it at the first bug. A moddable client that crashes loses to a
  reliable one, mods or not.
- **Maintainer load**: a moddable core, if versioned poorly, turns every core
  change into broken mods and user blame. Mitigation: versioned vocabulary,
  capability announcements, compatibility CI, and additive-only changes.
- **Vocabulary and consent**: a broad vocabulary can become an unusable consent
  form and a stability trap. Mitigation: stability levels, grouped consent
  (see `capabilities.md`), and additive versioning (§7.1).
- **Security**: a mod surface on a client holding a Slack session is a phishing
  magnet. Signing, permissions, and a curated first-party set are mandatory
  before anything public.
