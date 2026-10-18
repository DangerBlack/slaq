# slaq mods — execution model (rework)

Status: **shipped in 0.8.0** (2026-10-18), except Slice 4. Slices 1a, 1b, 2 and 3
are implemented and on `main`; Slice 4 (agent bridge) has not started.

> **What shipped, and where it differs from the proposal below.** The sections
> that follow are the proposal as written, kept as the design record. The
> implementation took a simpler path in places:
>
> - **One OS thread per mod**, not lanes: a mod is one instance on its own
>   `slaq-mod-<id>` thread, so a slow mod blocks only itself.
> - **Push callbacks stay.** The pull loop (`next-event`), the bounded event
>   queue and the separate worker instance were **deferred** (§6, Slice 2
>   re-scope). There was no ABI change, so `apiVersion` is still **`0.1`**, not
>   `0.2`.
> - **Background work** is the `background` capability: the host wakes the mod
>   with `background.tick` through the existing `on-event`, every
>   `intervalSecs` (0.1 s to 30 days).
> - **Limits:**
>   - Fuel is gone.
>   - The epoch is a kill-switch (checked about every 0.5 s) with a generous
>     backstop, `SLAQ_MOD_DEADLINE_SECS` (default 300 s; `0` disables it).
>   - Guest memory is `SLAQ_MOD_MEMORY_MB` (default 512 MiB).
>   - The per-store caps were **kept**: `maxBytes` is clamped to 64 MiB per
>     store, and vectors to 65,536.
> - **Cancellable I/O** (a stop lands in ~0.1 s even mid-request) and the GUI's
>   **Running mods** window (state, Stop, Reload). The TUI has no panel yet.
> - **Since then, in later releases:**
>   - **0.8.1:** the user chooses a mod's channels and hosts in the app.
>   - **0.8.2:** saved secrets survive a mod update.
>   - **0.9.0:** mods can read channel history (`history`, `thread-replies`)
>     and delete vectors.
>
>   See [`capabilities.md`](capabilities.md) and [`api-v0.md`](api-v0.md).

This document proposes a rework of **how mods run** — the execution model, the
limits philosophy, and the event delivery interface. It does **not** change the
capability vocabulary (what a mod may touch), which stays normative in
[`capabilities.md`](capabilities.md), nor the manifest shape
([`manifest.schema.json`](manifest.schema.json)).

When this was written slaq was unreleased and no mod existed in the field, so
there was **no compatibility burden**: the plan was a rework of the current
implementation, with the reference mods rewritten against a new `0.2` contract.
(As shipped, the contract did not need to change: see the status box above.)

**What this supersedes**

- The *execution model* and *trust model* sections of
  [`00-analysis.md`](00-analysis.md).
- The per-callback fuel + epoch + hard-cap enforcement in
  [`../../src/mods/mod.rs`](../../src/mods/mod.rs) (`FUEL`, `MAX_DISPATCH_FUEL`,
  `EPOCH_TICKS_PER_CALL`) and the 64 MiB store ceilings in
  [`../../src/mods/capability.rs`](../../src/mods/capability.rs)
  (`MAX_VECTOR_BYTES_CAP`, `MAX_KV_BYTES_CAP`, `MAX_SQL_BYTES_CAP`,
  `MAX_VECTORS_CAP`).

The WIT contract in [`api-v0.md`](api-v0.md) is amended by §5 below.

---

## 1. Why

The current model treats a mod as **untrusted code that must be prevented from
hurting the user**: every callback runs on a borrowed slice of the shared async
runtime, resets a fuel budget (`FUEL = 10_000_000`), and must finish inside an
epoch deadline of `EPOCH_TICKS_PER_CALL × EPOCH_TICK_MS ≈ 1s` of wall-clock.
Storage is clamped to 64 MiB per store. The guest cannot start a thread, cannot
run a background job, and cannot make a call that takes longer than a second.

That is the wrong contract, for two reasons the brain mod made concrete:

1. **It is paternalistic.** Installing a mod is an explicit, consent-gated act.
   A user who grants `net.egress` and `store.sql` is a power user accepting
   risk, exactly as they do when they run an agent with shell access on the same
   machine. The safeguard that matters is *"I choose what it can touch"*
   (capabilities) and *"if it misbehaves I delete it"* (lifecycle control) — not
   a fixed CPU or disk budget chosen by slaq.
2. **It is self-defeating.** The second-brain mod traps at `http-fetch`: a
   single LLM round-trip plus embedding blows the ~1s epoch deadline, even
   though the endpoint answers in 0.1s, because the deadline is a *budget*, not
   a *timeout*. The mod cannot do the one thing it exists to do. Fuel, in
   particular, was a mistake from the start: it penalises honest computation and
   buys no safety a kill-switch does not.

The goal of the rework: **mods are power-user tools.** They can run for a long
time, run in the background forever, and compute as hard as the machine allows —
isolated well enough that a bad one is *deletable*, not *survivable-at-all-costs*.
If the mod system cannot offer that, it offers nothing a native feature or an
external agent does not, and should be turned off. See §4 for what it offers that
those do not.

---

## 2. The model

### 2.1 Lanes: one OS thread per instance

A **lane** is one wasm component instance (its own wasmtime `Store`) running on
its own dedicated **OS thread**. A mod is one or more lanes:

- a **handler** lane — event-driven and responsive (ingest a new message,
  answer an `Insight` click, decorate);
- an optional **worker** lane — background and long-lived (re-embed the backlog,
  summarise threads, reorganise an index, compact storage), running for as long
  as the mod is enabled.

This follows from a hard constraint: a wasmtime `Store` is `!Sync` and must be
driven from a single thread; one instance cannot be driven from two threads. So
concurrency inside a mod is **more instances**, not shared memory. Lanes of the
same mod share **capabilities** and the **per-mod data directory / stores**
(`store.sql`, `store.kv`, `store.vector` at
`~/.local/share/slaq/mods/<id>/`) — they coordinate through that database, never
through guest memory. A worker writes rows; the handler reads them. That is the
whole coordination model, and it is the same durable store the mod already owns.

Today all mods are driven synchronously from the caller thread (the `&mut self`
`invoke_*` methods in `src/mods/mod.rs`, with host I/O bridged onto the runtime
via `Handle::spawn_blocking` + a blocking `rx.recv()`). The rework moves each
lane onto its own thread so that a slow guest call blocks **only that lane**, not
the app and not other mods.

### 2.2 Event delivery: a pull loop with bounded backpressure

Today the host *pushes*: it calls `on-message`, `on-event`, `on-reaction`,
`action`, `command`, … one export per kind, resetting fuel and epoch each time.
A background lane has no place in that shape.

The rework inverts it. Each lane runs a **guest loop** and pulls:

```
next-event(timeout-ms: u32) -> option<event>
```

- a **handler** lane loops on `next-event` with a blocking timeout and processes
  what it gets;
- a **worker** lane does its own work and either ignores events or polls
  `next-event(0)` occasionally.

The host keeps a **bounded per-mod queue** (size declared/configurable). When it
fills, the policy is **drop-oldest**, and a dropped-count is surfaced to the mod
and to the control panel (§2.5) so backpressure is visible, not silent. This
caps the one shared channel that can otherwise saturate, without capping the
mod's own compute.

### 2.3 The fence that survives

Everything that constrains a mod is now one of these, and nothing else:

| Fence | Role | Notes |
|---|---|---|
| **Capabilities** | *what* it may touch | Unchanged. Default-deny, manifest-declared, consent-gated. This is the real contract. |
| **Memory ceiling** | bound RAM per lane | Kept, but **generous and configurable** (per-mod, with a sane default), not a fixed 64 MiB. Enforced via `StoreLimits`. |
| **Epoch** | **kill-switch only** | Repurposed. No longer a ~1s budget reset per call. slaq bumps the epoch to *abort* a lane on demand (disable/kill/shutdown); it is never a deadline on honest work. |
| **Cancellable I/O** | unblock a lane stuck in a host call | Host-side I/O (egress, fs, store) observes a per-lane cancellation token so a kill does not wait for a 10s HTTP timeout. |
| **Process exit** | backstop | Lanes are threads in the slaq process; when slaq exits they exit. A lane that ignores cooperative cancellation dies with the process. |

**Dropped entirely:** fuel (`FUEL`, `MAX_DISPATCH_FUEL`), the per-call epoch
*budget*, and the fixed store byte/vector caps. Storage is bounded only by the
user's disk — if saturating the disk brings value, that is the user's call.

### 2.4 Lifecycle and cancellation

- **Owned by slaq.** The host holds each lane's thread handle, its cancellation
  token, and its epoch handle.
- **Kill = cooperative, with a backstop.** Cancel in-flight host I/O (token) +
  bump the epoch to abort the guest loop + join with a timeout. If the lane does
  not unwind in time, it is abandoned and reaped at process exit. A guest stuck
  in a pure-compute spin is caught by the epoch; a guest stuck in host I/O is
  caught by the token.
- **Die with slaq.** On app shutdown every lane is cancelled and joined
  (bounded); the process exits regardless.
- **No trusted tiers.** Every mod is treated uniformly. "Trusted by default"
  does not exist; trust is the set of capabilities the user granted, nothing
  more.

### 2.5 Control panel (the "just delete it" surface)

The safety story is **observability + a kill button**, not a budget. A mods
panel in the GUI lists each mod and its lanes with:

- state (running / waiting on event / in a host call / blocked / crashed),
- resident memory and queue depth (incl. dropped-event count),
- **disable**, **kill**, and **delete** actions.

This is what makes "if the mod misbehaves you simply delete it" a real,
one-click operation instead of a slogan.

---

## 3. Keeping simple mods simple

The lane/loop model must not make a translation mod hard to write. It does not,
because the **host** exposes only the raw primitives (lanes + `next-event`),
while the **guest SDK** (the `wit-bindgen` wrapper the reference mods and
[`HOW_TO_WRITE_MODS.md`](HOW_TO_WRITE_MODS.md) build on) ships a default
handler-lane loop that dispatches to familiar callbacks:

- **Simple mod** (translation, a helper, a decorator): implement `on_message` /
  `on_action` and nothing else. The SDK's default loop owns the `next-event`
  call. The author never sees a thread or a queue.
- **Powerful mod** (second brain): opt out of the default loop, drive
  `next-event` directly, and declare a `worker` lane for background curation.

So the thread model *lowers* the floor for small mods (the ambient boilerplate is
in the SDK) while raising the ceiling for big ones.

---

## 4. What the model is for

Two archetypes, both first-class, justify the system over "just run an agent":

- **Self-contained mods** — translation, helpers, the brain with a company LLM. Stream
  access + per-mod store + an LLM over `net.egress` + native UI. These need
  nothing outside slaq; the thread model makes them easy (§3).
- **Agent-bridge mods** — a thin mod that is the *eyes, hands, and UI inside
  Slack* for a capable local agent (the brains) reachable over a localhost
  endpoint. v1 rides the existing `net.egress`-to-localhost; a first-class
  `agent.bridge` capability with context-shipping helpers is a later slice.
  This is the archetype that **cannot exist outside slaq** — it is ambient in
  the message stream, renders in the native UI, and is scoped by the same
  consent model.

The moat, in both cases, is **ambient stream presence + native UI surface +
scoped-by-consent access + an always-on lifecycle** — a shareable, scoped
automation an external script cannot replicate without being slaq.

---

## 5. What changes in the code

All under the `mods` feature flag; the default build is unaffected.

| Area | Change |
|---|---|
| **WIT** ([`api-v0.md`](api-v0.md), `wit/slaq-mod.wit`) | Replace the push callbacks (`on-message`, `on-event`, `on-reaction`, `action`, `command`, …) with a lane entrypoint + `next-event(timeout-ms) -> option<event>`. `event` is a variant over the current callback payloads. `apiVersion` → `0.2`. |
| **Host** ([`src/mods/mod.rs`](../../src/mods/mod.rs)) | One OS thread per lane; per-lane `Store`; per-mod bounded event queue + drop-oldest; cancellation token + epoch-as-killswitch; thread/lifecycle ownership. Remove fuel and the per-call epoch budget. |
| **Limits** ([`src/mods/capability.rs`](../../src/mods/capability.rs)) | Remove the fixed 64 MiB / 65 536-vector caps; replace with a configurable, generous per-mod memory ceiling. Storage bounded by disk. |
| **Manifest** ([`manifest.schema.json`](manifest.schema.json)) | Declare lanes (handler always; `worker` optional), optional queue size and memory-ceiling override. |
| **GUI** ([`src/gui/mod.rs`](../../src/gui/mod.rs)) | A mods control panel (§2.5). This is the one piece that touches the otherwise-frozen GUI and needs explicit sign-off. |
| **Reference mods + brain** | Rewritten against the loop ABI; the brain gains a `worker` lane for backlog embedding/curation. |

---

## 6. Implementation slices

Each slice is a coherent, validated, inert-without-the-flag PR onto
`feat/mods-rework`, in order. Each has tests.

1a. **Unleash the limits (done).** Drop fuel entirely (`consume_fuel` off; every
   `set_fuel`/`get_fuel` site, the `dispatch_fuel_cap` budget and the multi-pass
   loop removed — a dispatch now runs the whole window in one pass). The epoch
   stays as a *generous, configurable* wall-clock backstop instead of a ~1 s
   budget, and the guest memory ceiling becomes generous + configurable. Two env
   knobs, both with generous defaults:
   - `SLAQ_MOD_DEADLINE_SECS` — per-callback wall-clock deadline, default **300 s**
     (`0` disables it). This is what was trapping the brain's LLM round-trips.
   - `SLAQ_MOD_MEMORY_MB` — guest linear-memory ceiling, default **512 MiB** (was
     16 MiB).
   Kept the existing dispatch path (off-UI `spawn_blocking` + the global
   `mods_lock`): no ABI change, no threading change. → **unblocks the brain
   immediately** and makes every mod safe-to-be-slow. Known limitation it does
   *not* fix: a slow mod still holds `mods_lock`, so it blocks other mods'
   dispatch until it returns — that is what 1b removes.
1b. **One OS thread per mod + true concurrency (done).** Each mod's `Store` runs
   on a dedicated `slaq-mod-<id>` thread; every guest-calling `Host` method is a
   job the coordinator sends and awaits. Landed in three green steps:
   - *part 1* — the thread + job loop + `ModHandle`/`ModWorker` split, an epoch
     kill-switch callback (checks a `kill` flag every ~0.5 s + the generous
     backstop), and `Drop` that stops workers before a reload touches their files;
     the blocking I/O bridge uses a captured runtime handle.
   - *part 2a* — Host dispatch through `&self` with the dedup/decoration cache
     behind an internal `Mutex` (taken only for brief bookkeeping, never while a
     guest runs); `mods` stays an immutable `Vec`.
   - *part 2b* — the app holds `Arc<Host>` (not `Arc<Mutex<Host>>`) and the global
     `mods_lock` is gone, so a slow mod blocks only its own thread.
   - *review fixes* — an adversarial review found that without the global lock
     two same-channel dispatches could both see a new message as unseen, run its
     `on-message` twice and append its decoration twice to the Host cache. A pass
     now **claims** each message (marks it seen, retracts its old output) under the
     lock that selects it, before any guest runs; the per-dispatch notify/API-write
     budget is kept per dispatch id on each worker (concurrent dispatches no longer
     reset each other's tally); a worker that dies is quarantined; the cache lock
     recovers from poisoning. A pass also fans every `(message, mod)` job out at
     once, so within one pass a slow mod no longer delays the others.
   Accepted trade-off: a dispatch's cache snapshot and its `AppState` publish are
   not atomic, so a rare `run_mods`/gesture overlap can briefly overwrite a
   decoration in `AppState`; it self-heals on the next dispatch (the Host cache is
   authoritative and consistent). Known, bounded, deferred to Slice 3 (in-flight
   host-I/O cancellation): dropping a mod mid-request waits for that request (up to
   ~30–60 s for Slack API calls), and across a reload an old worker still finishing
   a request can briefly share the mod's SQLite files with the new one (SQLite
   locking + a 5 s busy timeout prevent corruption).
2. **Background lane (done; re-scoped).** A mod that requests `background`
   (`{"intervalSecs": N}`, default 60, min 0.1) and subscribes to
   `background.tick` is woken on its own worker thread every N seconds through the
   **existing** `on-event` export — no WIT change, no rebuild. The worker loop
   waits with a deadline (`recv_timeout`), checked before every receive, so a busy
   event stream cannot starve the lane; fixed delay, so a long tick never queues a
   catch-up burst. Gated by the user's `background` grant (its own consent group).
   → the brain gets its background curator.
   *Re-scope:* the original slice also inverted delivery into a guest pull loop
   (`next-event` + bounded queue + drop-oldest) on a **separate** worker instance.
   That needs a required WIT export (rebuilding every mod) and buys little yet: the
   handler callbacks already react immediately, and dispatch is already
   backpressured (each coordinator call blocks on its reply). It is deferred; the
   guest contract (`on-event("background.tick")`) stays valid if ticks later move
   to a parallel lane instance.
3. **Control panel + cancellable I/O (done).** Every blocking host bridge
   (http-fetch, DNS, file-get/-upload, chat-send/-forward, reactions) waits through
   `await_io`, polling the mod's `kill` flag every 100 ms and aborting the
   in-flight request — so stopping a mod (or a reload/exit dropping it) lands in
   ~0.1 s even mid-request, closing the two items the 1b review deferred. Workers
   keep lock-free counters (calls, ticks, busy-since); `Host::statuses()` reads
   them every frame and `Host::stop(id)` quarantines a mod and cuts its call. The
   GUI's settings window gains **Running mods…**: per mod, its state, how long its
   current call has run, calls/ticks, background interval, a **Stop** button, and
   **Reload mods** (a fresh host; stopped mods start again). Memory per mod and a
   TUI equivalent are not shown yet. → "if it misbehaves, stop it" is one click.
   A second adversarial review (of slices 2–3) led to: `intervalSecs` capped at 30
   days (an unbounded value overflowed `Instant + d` — a client-wide abort in
   release); a stopped worker refuses jobs already queued and host bridges refuse
   to *start* a request once stopped; claims carry a token, so a pass stores only
   while its claim is current (racing edits no longer leave both texts' output) and
   never overwrites output a newer gesture stored; background ticks share one
   notify/write budget per minute; dropping a host signals every worker first and
   joins them on a reaper thread (it used to block the GUI thread); `run_mods` no
   longer publishes an old host's output after a reload and prunes against what is
   on screen *now*; overlapping (re)loads are coalesced; message jobs share the
   index instead of copying each text per mod. Still accepted: a reload re-runs the
   on-screen messages as new (at-least-once; pre-existing), and the extreme
   concurrency edges (more than 64 interleaved dispatches on one worker, more than
   20 000 newer claims during one pass) degrade to at-least-once.
4. **Agent bridge.** First-class `agent.bridge` capability + helpers to ship
   thread context to a local agent and stream results into the UI. → the
   archetype that cannot exist outside slaq.

1a pays for itself immediately (the brain works); 1b proves the thread model;
2–4 build on it.

---

## 7. Decisions still open

- **Default memory ceiling** and whether the override lives in the manifest, in
  settings, or both. (For `net.egress` and the read scope this is settled: the
  manifest offers a default, the user configures the rest in the app or with
  `slaq mods scope|egress`.)
- **Event variant shape**: one `event` variant vs. a small set of typed
  pull calls. Leaning: one variant, simplest for the loop.
- **Worker scheduling**: does a worker lane also receive events (so it can react
  to a burst) or is it strictly self-driven? Leaning: it *may* poll
  `next-event(0)` but is not required to.
- **`agent.bridge`**: a dedicated capability vs. leaning on `net.egress` to
  localhost for the whole life of the feature. Leaning: `net.egress` for v1,
  promote to a first-class capability in slice 4 once the shape is proven.

---

## 8. Scope and non-goals

- **In scope:** the execution model (threads/lanes), limits philosophy, event
  delivery, lifecycle/cancellation, the control panel, and the ABI change they
  require.
- **Not in scope:** the capability *vocabulary* (unchanged), the marketplace /
  signing (still gated, see [`roadmap.md`](roadmap.md)), and any new data
  capability.
- **Frozen-app caveat:** the host and GUI changes are to the main application,
  which is otherwise frozen. This document is a proposal; **no implementation
  lands without explicit approval**, slice by slice, and the GUI control panel
  in particular is called out separately in §5.

---

## Glossary

- **Lane** — one wasm instance (one `Store`) on one dedicated OS thread.
- **Handler lane** — the responsive, event-driven lane every mod has.
- **Worker lane** — an optional background lane that runs for the mod's lifetime.
- **Pull loop** — the guest driving `next-event` rather than the host pushing
  callbacks.
- **Kill-switch epoch** — wasmtime epoch interruption used only to abort a lane
  on demand, never as a per-call budget.
