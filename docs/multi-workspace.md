# Multi-workspace — analysis

Status: **analysis only**, no code yet. Decisions taken 2026-10-10, then reviewed
three times — two Copilot PR passes and one internal architecture review (§14).
Everything raised is folded in below.

The short version: **all enabled workspaces stay live and listening at the same
time; the UI shows one at a time ("focused"); you switch from a left rail, and a
workspace can be turned off without logging out.** Nothing is torn down on a
switch — a call keeps running in the background, it just loses focus.

---

## 1. Goal

Work with several Slack workspaces at once: see and be notified by all of them,
switch instantly from a left rail, and put one to sleep without logging out.

## 2. Decisions taken

- **All workspaces listen concurrently** (own socket, own presence, own
  notifications).
- **One focused at a time**; switching changes focus, it does **not** stop the
  previous one. A background workspace keeps its call, its unread, its socket.
- **A left rail** is the switcher.
- **A workspace can be turned off without logout**: disconnected, runtime
  stopped (memory freed), credentials kept, and it can be turned back on.
- **Memory is not a concern**: ~100 MiB of *data* per live workspace is accepted
  (a background call costs more — its own thread, tokio runtime and audio threads;
  see §9).
- **Storage is one DB file per workspace** (§6.1).

## 3. Non-goals

- A single merged sidebar / message list across workspaces.
- Channel/search/favourites sharing across workspaces.

---

## 4. How it works today (relevant facts)

- **Credentials are mostly per-workspace, with two gaps** (§6.3): `xoxc` is keyed
  by workspace but `xoxd` is a single global key; and the keychain path returns
  before writing `credentials.json`, so keychain-backed workspaces are not
  enumerable.
- **`App` is single-workspace, immutable-credentialed**, and `Clone` (shared
  handles): `creds`, `api_client`, `state: Arc<RwLock<AppState>>`,
  `store: Arc<Mutex<Store>>`, `presence_tx`, `demo`.
- **The store is one global file** `~/.local/share/slaq/cache.db`, tables
  `channels`/`messages`/`users`/`meta`, no `team_id`; WAL; migrations via
  `PRAGMA user_version`; dir `0700`, file `0600`.
- **The identity needed to name a DB is not available when the store opens.**
  Both storage backends return empty `user_id`/`team_id`; `state.team_id` comes
  from `meta["team_id"]`, written *later* by `spawn_bootstrap`; `Credentials.team_id`
  is only filled on the `--from-desktop` path. So the DB key must come from the
  **registry**, not from `Credentials` at open time (§6.1).
- **Settings are global but live in the per-workspace store**: `ui_lang` (calls
  the global `rust_i18n::set_locale`), `notify` (a global atomic), `ui_scale`
  (a global egui zoom), last-focused/last_channel. With N stores, startup order
  would decide locale and notifications (§6.2).
- **The GUI holds one `App`**, one image loader (`Images::with_cookie`), and one
  global, URL-keyed GPU texture cache; the waker is registered once, on the single
  focused app.
- **Network work is detached and untracked**, with no shutdown: `spawn_ws_listener`
  (reconnect loop), `spawn_ws_pump`, `spawn_scheduler` (an infinite loop that
  writes the store), `refresh_presence`, and **dozens** of per-action `tokio::spawn`s
  that hold `Arc<Mutex<Store>>`/state clones.
- **A call owns an OS thread** with its own 2-worker tokio runtime; `Call` has no
  `Drop` that hangs up, and closing its command channel does not stop the loop.
- **The CLI/TUI open the legacy `cache.db` directly** in many commands (channels,
  history, huddle, cache status/clear, …) — they will break when the file is
  renamed (§6.4).

---

## 5. Review findings folded in

### From the Copilot PR review (2026-10-10)

1. **`xoxd` global** → per-workspace key + migration (§6.3).
2. **Raw URL as filename** → a validated key, never a raw URL (§6.1).
3. **"Let loads finish"** → cancel + a barrier over **all** tasks (§6.4).
4. **Only `self.app` swapped** → per-workspace image loader/cookie (§6.6).
5. **Per-workspace `meta` scoping globals** → a shared settings store (§6.2).
6. **Dropping `App` doesn't drop a call** → explicit hang-up + confirm (§9).
7. **Registry cannot see keychain workspaces** → a non-secret registry (§6.3).

### From the internal review (§14)

8. **DB key undecidable at store-open** → resolve it from the registry; normalize
   the URL; else always use the URL hash (§6.1).
9. **The legacy migration is a controller step, not per-`open_for`** → do it once,
   before building any `App` (§6.0).
10. **The barrier misses most writers** → route **every** spawn through a tracked
    set; the scheduler and action tasks must observe the token (§6.4).
11. **Global settings have no home in `App`** → a `Settings` handle passed into
    `build_with_store`; include `ui_scale` too (§6.2).
12. **CLI/TUI hardcode the legacy path** → a shared key resolver; enumerate
    `cache-*.db` (§6.4).
13. **Per-workspace waker** so background rail badges/notifications repaint (§6.5).
14. **Turn-off can't be awaited on the egui thread** → make it async with a
    "turning off…" state (§6.4).
15. **Per-workspace transient GUI state** → reset the dialog/transient set on focus
    change; key only the drafts (§6.6).
16. **Global texture cache keyed by URL** → key by `(workspace, url)` or clear on
    switch (§6.6).
17. **Registry/keychain lifecycle gaps** → always upsert the registry; delete the
    per-workspace `xoxd`; persist `team_id` at login (§6.3).
18. **`load_credentials(None)` → `app.slack.com` is a trap** → resolve `None` via
    the registry (§6.3).
19. **Zero workspaces / focus bounds / turning off the last one** → the controller
    may hold zero and render the login screen (§6.0).
20. **`Gui::new` / test harness** → keep a single-workspace constructor for tests
    and `--demo`; add the multi-workspace one (§6.6).

Nice-to-have, also folded: notification provenance (title = workspace name, §8),
the settings UI reading the shared value (§6.2), the per-call cost note (§9), and
rail "mention" accuracy (§8).

### From the second Copilot PR review (2026-10-10)

21. **A bare `JoinSet` cannot supervise tasks spawned by cloned `App` values**
    (`JoinSet::spawn` needs `&mut`, while `App`/helpers carry only handles) → a
    cloneable per-workspace supervisor handle, passed into `App` and nested code
    (§6.4).
22. **A `team_id`-only cache key exposes the previous account** after a different
    user logs into that workspace → the cache identity includes `user_id` (§6.1).
23. **The registry needs an upgrade path for existing keychain installs** (no
    `credentials.json`, non-enumerable keyring) → a one-time recovery/indexing
    before `load_credentials(None)` moves to the registry (§6.3).
24. **A timed-out call cannot be force-stopped** (its OS thread) → keep the
    workspace in `turning off` and report/retry; do not complete the transition
    (§6.4, §9).
25. **The `security.yml` change contradicts the "analysis only" scope** → split it
    into its own PR, or fix the PR metadata (§12).
26. **`enabled` has no persisted source** → persist it (default on), build only
    enabled entries, and commit the toggle only after shutdown/start succeeds
    (§6.3).
27. **The key resolver is not stable across the first run** (URL-hash first,
    `team_id` later) → persist a `db_key` in the registry and read it, never
    recompute (§6.1).
28. **`last_channel` is not process-global** — each live workspace needs its own
    selected channel → it is per workspace (§6.2).
29. **Logout never removes the registry entry** → logout removes the entry and its
    per-workspace cookie; turn-off keeps the entry (§6.3).

### From the third Copilot PR review (2026-10-10)

Consistency fixes with no new architectural blocker:

30. **The gitleaks failure was a permission, and the action also *reads* the PR**
    → grant `pull-requests: read` (needed by `ScanPullRequest` to scope the scan,
    and available even on a fork PR) and keep PR comments off
    (`GITLEAKS_ENABLE_COMMENTS: "false"`, since commenting would need *write*). The
    old "scans fine on a push" aside was also wrong: this workflow has no `push`
    trigger.
31. **The migrated cache name must include `user_id`** — the composite name of §6.1.
32. **A keychain-only install with an empty `credentials.json` cannot be
    enumerated** → an explicit `slaq login --workspace` fallback, never a silent
    logout (§6.3).
33. **`last_channel` must not be routed through the shared `Settings`** in the task
    table (§6.2, §10).

### From the fourth Copilot PR review (2026-10-10)

Found only in the review body ("Previously missed"), with no inline thread — the
reason the CLI fetch must read the overview too:

34. **`Gui.app` must be optional** — the model permits zero workspaces (login/off)
    while `Gui.app` was mandatory; resolve the focused app through the controller
    (§6).
35. **`focus` must point at an enabled, built workspace**, not merely an in-range
    index — turning off the focused one must move focus to an enabled peer (§6.0).
36. **Image downloads must be supervised** — `Images::request` spawns directly
    (`src/app/images.rs`); `Images` must be handed the supervisor, or an in-flight
    authenticated download survives the barrier (§6.4).
37. **`cache clear` must not delete `settings.db`** — it holds preferences, not
    cache (§6.4).
38. **The workspace goes in the notification *heading*** — `heading()` prefers the
    channel `subtitle`, so setting `title` alone would not show it (§8).
39. **Guarantee the call's completion signal on every thread exit** — it is not set
    when the call runtime fails to build (`src/calls/live.rs`); a timeout would
    otherwise be indistinguishable from a missing signal (§9).

### From the fifth Copilot PR review (2026-10-10)

Again body-only ("Previously missed"):

40. **`Images` must be optional and dropped on turn-off** — it holds the cookie, an
    HTTP client, the `seen` set and thousands of entries (`src/app/images.rs`), so an
    off workspace must not keep it (§6.6).
41. **Recovery must resolve identity offline** — legacy credentials carry empty
    `team_id`/`user_id`; take them from the legacy cache `meta` or `auth_test`, and
    fall back to the `db_key`-only cache when neither is available (§6.3).
42. **Validate `user_id` in filesystem paths** like the key (it comes from an
    arbitrary string in `auth_test`, `src/api/client.rs`) (§6.1).
43. **Namespace the on-disk image cache by account** — `Images::fetch` reads the
    global `~/.cache/slaq/images` by URL *before* authenticating, so one account
    could be served another's private image (§6.6).
    (Plus a count correction in §14, now fixed.)

---

## 6. Architecture: all live, one focused

One window. A **controller** owns every workspace runtime and which is focused:

```
Controller
 ├─ settings: Settings                 // shared store + cached globals (§6.2)
 ├─ workspaces: Vec<Workspace>
 │    ├─ info: { url, team_id, user_id, name }
 │    ├─ db_key: String                // persisted in the registry, never recomputed (§6.1)
 │    ├─ app: Option<App>              // None while off / not built
 │    ├─ images: Option<Images>        // per-workspace loader + cookie; dropped on off (§6.6)
 │    ├─ supervisor: Supervisor        // cloneable; EVERY spawn goes through it (§6.4)
 │    ├─ shutdown: CancellationToken
 │    └─ enabled: bool                 // persisted (§6.3)
 └─ focus: Option<usize>               // None → login screen (§6.0)

Gui
 ├─ controller ref
 ├─ app: Option<App>                   // None → login/off screen (§6.0)
 ├─ textures: keyed by (workspace,url) // or cleared on switch (§6.6)
 └─ snap: Snapshot
```

- **Focus switch is cheap and non-destructive:** `self.app = workspace.app.clone()`
  plus re-pointing the image loader, texture namespace and transient UI state
  (§6.6). Nothing is cancelled.
- **Shutdown:** cancel every token and await every barrier.

### 6.0 Controller bootstrap (global, before any `App`)

All the one-time, global work happens **once, in the controller, before building
any `App`** (doing it lazily inside `open_for`/`build_with_store` reintroduces the
startup-order and race hazards):

1. open/create `settings.db`; load the shared settings (§6.2);
2. **recover the registry if it is empty but credentials exist** (§6.3): from the
   keychain/file of the known workspace(s), write the non-secret entries (with
   `db_key`, `enabled`, `team_id`, `user_id`) and copy the legacy global `xoxd` to
   each recovered entry. Without this an existing keychain-only install looks
   logged out.
3. read the legacy `cache.db` `meta` (if present): migrate the **global** keys
   (`ui_lang`, `notify`, `ui_scale`, last-focused) into `settings.db`; migrate the
   legacy `last_channel` under its owning workspace; resolve the legacy owner from
   `meta.team_id` against the registry and rename `cache.db` (+`-wal`/`-shm`, after
   a `wal_checkpoint`) to that owner's **composite** `cache-<db_key>-<user_id>.db`
   (the same name §6.1 opens with). Guard so it runs once.
4. build an `App` **only for enabled registry entries**, each with its
   per-workspace store (keyed by the persisted `db_key`) and the shared `Settings`
   handle.
5. keep `focus` pointing at an **enabled, built** workspace — it is not an index
   that survives a turn-off: when the focused workspace is turned off while another
   is enabled, move focus to an enabled one; when none remains, hold zero and show
   the login screen.

### 6.1 Storage: one DB per workspace

- **The `db_key` is computed once and persisted** in the registry entry, never
  recomputed — a key that resolves differently between runs would silently open a
  fresh cache:
  - if `team_id` matches `^T[A-Z0-9]{2,}$`, use it; otherwise a short **hash of the
    normalized URL** (strip scheme/port/path/trailing slash, lowercase host), first
    16 hex of SHA-256 — **never** the raw URL;
  - **the preference is fixed once**: a workspace first seen without a `team_id`
    keeps its URL-hash key even after `team_id` arrives.
- **The cache identity includes the user.** `cache-<db_key>.db` is per workspace,
  but a different account logging into the same workspace must not see the previous
  account's channels/DMs/messages: key the file by `db_key` **and `user_id`**
  (`cache-<db_key>-<user_id>.db`), or store the cache owner and clear/archive the DB
  when the credential user changes. **`user_id` is validated too** — it is
  interpolated into a filesystem path and `auth_test` deserializes it as an arbitrary
  string (`src/api/client.rs`); require a single safe Slack-ID component (no
  separators, no `..`) before persisting it or building the path.
- File `…/slaq/cache-….db`; perms `0700`/`0600`; `open_for` opens-or-creates and
  **never migrates** (migration is §6.0, step 3).
- `open_or_memory` / `open_in_memory` unchanged.

### 6.2 Settings: a shared handle

- The **process-global** settings move to `settings.db` and are read through a
  `Settings` handle passed into `build_with_store`: **`ui_lang`, `notify`,
  `ui_scale`, last-focused** (`ui_scale` applies a global egui zoom, so it cannot
  be per workspace).
- `App` must stop reading/writing these from `self.store`; the getters/setters
  (`set_language`, `set_notifications`, `set_ui_scale`) route through `Settings`.
  The settings UI reads the shared value, not the focused app's snapshot, or a
  background workspace shows a stale toggle.
- **`last_channel` is NOT global.** Each live workspace keeps its own selected
  channel, in its per-workspace `meta` (namespaced by `db_key`); a single shared key
  would be overwritten by whichever workspace changed channels and could restore a
  foreign channel id in another. The legacy value migrates under the legacy owner.
- Per-workspace `meta` keeps only workspace data (its own `my_user_id`,
  `last_channel`, …).

### 6.3 Credentials: per-workspace cookies, registry, defaults

- **`xoxd` per workspace** in both the file and the keychain paths, keyed like
  `xoxc`, with a **migration** of the legacy single `xoxd`. `delete_credentials`
  must delete `xoxd:{workspace}`, not a shared key.
- **A non-secret registry** in `credentials.json`, written by **both** storage
  paths (file **and** keychain), one entry per workspace:
  `{url, team_id, user_id, name, db_key, enabled}`. `auth::available_workspaces()`
  reads it; bootstrap builds only `enabled` entries. The login and `--from-desktop`
  paths write `team_id`/`user_id` (from `auth_test`) and the `db_key` once.
- **`enabled` is persisted** (default true for existing entries): a workspace turned
  off stays off across restarts. The toggle is committed only **after** its
  shutdown/start succeeded, so a crash mid-transition does not lose the state.
- **Logout removes the registry entry** (and its per-workspace cookie); turn-off
  keeps it. Without removal, `available_workspaces()` would still list it and
  bootstrap would keep trying to build it with missing credentials.
- **Upgrade path for existing installs.** If the registry is empty but credentials
  exist, recover them once by indexing **whatever source names a workspace**: the
  legacy `credentials.json` `workspaces` map, or the `--workspace` in use. Write the
  registry entries (with `db_key` and the composite `user_id`) and copy the legacy
  global `xoxd` to each recovered entry (§6.0, step 2). **Identity resolution:**
  legacy credentials carry empty `team_id`/`user_id` (`src/auth/token.rs`), so
  recovery takes them from the legacy `cache.db` `meta` (`team_id`, `my_user_id`) or,
  when online, from `auth_test`. **Offline with neither** a cache `meta` nor
  `auth_test`, the composite key cannot be formed: fall back to the `db_key`-only
  cache (no user qualifier) and note the limitation, rather than guessing an id.
  **A purely keychain-only install whose `credentials.json` is empty and with no
  legacy cache cannot be enumerated at all** — `xoxc:{workspace}` entries are not
  listable and nothing names the workspace. For that case the fallback is explicit:
  ask the user to run `slaq login --workspace <url>` once. Never show them silently
  as logged out.
- **`load_credentials(None)` must not default to `app.slack.com`**; after the
  recovery step it resolves through the registry (last-focused, else the sole entry,
  else the login screen). `app.slack.com` stays only as an explicit last resort,
  logged.

### 6.4 Lifecycle: cancel, track everything, then wait

- Each runtime has a `CancellationToken` **and a cloneable `Supervisor`** that every
  task goes through — the ws loop, the pump, the **scheduler**, `refresh_presence`
  **and every per-action task** (`load_channel_messages`, `load_older`,
  `open_thread_with`, `send_message`, `upload`, `save`/`unsave`, `schedule`/
  `cancel`, `delete`, `select_index`/`mark_read`, `jump_in_conversation`,
  `spawn_resolve_bots`, `toggle_favorite`, `spawn_emoji`, **and the authenticated
  image downloads in `Images::request`** — which `tokio::spawn` directly today
  (`src/app/images.rs`), so `Images` must be handed the supervisor too). A bare `JoinSet` is
  not enough: `JoinSet::spawn` needs `&mut`, while `App` and the helpers carry only
  `Arc` handles — so the supervisor must be a **cloneable handle** (an `Arc` around
  a `JoinSet`/mutex, or a channel to an owner) handed to `App` and nested code, that
  **refuses new work once cancelled**, and whose whole set the controller can close
  and await. A barrier over a hand-picked subset is not enough: any untracked task
  keeps the store `Arc` alive and can write after "off".
- **Turn off / rebuild is asynchronous** and must not block the egui thread:
  spawn it, cancel the token, `supervisor.join_all().await`, confirm the call is
  over (§9), then flip `enabled=false`, persist it, and drop the `App`; show a
  "turning off…" state on the rail and refuse re-entry. **Focus switch does not
  wait** (nothing is cancelled).
- **If a call does not confirm in time, do not complete the transition.** The call's
  OS thread cannot be force-stopped, so keep the workspace in `turning off` and
  report/retry, rather than claiming it is off.
- **CLI/TUI** go through the same resolver: `workspace_key_for(entry)` +
  `Store::open_for(key)` in every command. `cache status` may list `cache-*.db` and
  `settings.db`, but **`cache clear` must touch only `cache-*.db`**: `settings.db`
  holds preferences (locale, notifications, zoom, focus), not cache, and clearing it
  would be unexpectedly destructive. Without the resolver the CLI regresses the
  moment the cache is renamed.

### 6.5 Waker / repaint per workspace

- Every workspace installs its own waker (all calling `ctx.request_repaint()`),
  tracked per workspace, and re-armed after a rebuild. Otherwise a background
  workspace's unread/rail badge only refreshes on the 2 s fallback tick.

### 6.6 GUI resources and transient state

- **Image loader/cookie per workspace** (`images: Option<Images>` in the
  `Workspace`), swapped with the focused app, and **dropped with the runtime on
  turn-off** — it holds the cookie, the HTTP client, the `seen` set and up to a few
  thousand loaded entries (`src/app/images.rs`), which an off workspace must not keep.
- **On-disk image cache namespaced by account.** The in-memory loader being
  per-workspace is not enough: `Images::fetch` reads `~/.cache/slaq/images` **by URL
  before authenticating**, so a second account could be served a private image the
  first cached. Namespace that directory by the composite account identity (owner-
  only), not just the in-memory objects.
- **Texture cache** keyed by `(workspace, url)`, or cleared on focus switch
  (re-upload cost is bounded by what is on screen). `icon_textures` are static and
  stay shared.
- **Transient UI state is reset on focus change**, not kept per workspace: a single
  `on_focus_changed()` that clears the dialog/transient set (open thread/composer
  edit, forward, search, manage/kick/new-channel/people-pick, image viewer, emoji
  popup, find bar and its match state, highlights/jump counters, scroll targets,
  the `*_open` flags). Only genuine persistence is keyed per workspace — the
  **drafts** `(workspace, channel)`.
- **Constructors:** keep a single-workspace `Gui::new` for the tests and `--demo`;
  add the multi-workspace one. `run(app)` becomes `run(workspaces)`; the snapshot
  harness builds N in-memory-store apps with a seeded registry + settings so the
  rail snapshot is reproducible. `App::demo` stays single-workspace, network-free.

---

## 7. The left rail

- A narrow vertical bar on the far left: one entry per configured workspace
  (avatar / initial), the focused one highlighted.
- Each entry carries a **badge** for its own unread/mentions, so you see where to
  look without focusing it.
- Clicking focuses that workspace (cheap, §6.6).
- Per-entry menu: **Turn off / Turn on** (with a note if a call is active, §9).
  "Add a workspace…" at the bottom.
- Switching changes the whole window content to the focused workspace.
- The historical "unpainted column" bug applies: no sliver.

---

## 8. Notifications and unread

- **Notifications from all live workspaces.** The path is per-workspace; the
  `notify` **switch** is a global setting (§6.2).
- **Put the workspace in the *displayed* heading.** Setting `title` alone is not
  enough: `DesktopNotification::heading()` prefers the channel `subtitle` when
  present (`src/notify.rs`) and that heading is what is shown. Build it as
  `workspace · channel`, falling back to the workspace name.
- The **rail badge** shows per-workspace unread/mentions. Live events set unread
  but not mentions, so the **mention** part of the badge may undercount until a
  reload — derive it from that workspace's notification centre (or compute it on
  the message path).
- The **notification centre** shows the focused workspace's list; the rail is how
  you go look at another's.
- A turned-off workspace neither listens nor notifies.

---

## 9. Calls

- A call keeps running in the background when you focus another (its OS thread and
  media session stay alive); it is indicated on its rail entry.
- **Turning off a workspace must end its call explicitly.** `Call::join`'s thread
  is invisible to the supervisor, `Call` has no `Drop` that hangs up, and closing
  the command channel does not stop its loop. Call `hang_up()`, then **await the
  call's `finished`** as part of the turn-off barrier; if it does not confirm in
  time, keep the workspace in `turning off` and report/retry (§6.4) — a timed-out
  call cannot be force-stopped. **`finished` is not guaranteed on every exit
  today**: if the call's runtime fails to build, the thread returns without setting
  it (`src/calls/live.rs`), so add a completion guard set on *every* thread exit —
  only then does a timeout mean a genuinely live thread. On logout/login re-exec,
  either hang up every call or accept the exit explicitly.
- **Cost of a background call:** its own OS thread + a 2-worker tokio runtime +
  `cpal` threads, on top of the ~100 MiB of data. Consider limiting how many
  background calls can be live.

---

## 10. Task breakdown

| # | Task | Size |
|---|------|------|
| 1 | Controller **bootstrap**: `settings.db`, global-settings migration, legacy `cache.db` rename, zero-workspace path (§6.0) | M |
| 2 | **Stable key** resolution (registry `team_id` or normalized-URL hash) + `Store::open_for` + URL normalization (§6.1) | S–M |
| 3 | **Shared `Settings`** handle + route `ui_lang`/`notify`/`ui_scale`/last-focused through it; `last_channel` stays per workspace (§6.2) | M |
| 4 | **`xoxd` per workspace** (file + keychain) + legacy migration + `delete` per workspace (§6.3) | M |
| 5 | Non-secret **registry** (both paths) + `available_workspaces()` + `load_credentials(None)` via registry (§6.3) | M |
| 6 | **Track every spawn** (`JoinSet`) + token through ws/pump/scheduler/presence/**all actions** (§6.4) | **L** |
| 7 | **Async turn-off/rebuild** with the barrier + "turning off…" state (§6.4) | M |
| 8 | **Calls**: explicit hang-up + confirm on turn-off; rail indicator (§9) | M |
| 9 | **CLI/TUI** shared key resolver; `cache status/clear` over all DBs (§6.4) | S–M |
| 10 | **Per-workspace waker** (§6.5) | S |
| 11 | **Per-workspace GUI**: image loader/cookie, texture namespacing, `on_focus_changed` reset (§6.6) | M |
| 12 | **Left rail**: entries, badges, focus, per-entry menu, "Add…" (§7) | M |
| 13 | **Notifications**: per-workspace title, badge accuracy (§8) | S |
| 14 | **Constructors/harness**: multi-workspace `Gui::new`, `run(workspaces)`, snapshot harness (§6.6) | S–M |
| 15 | Tests + snapshots + README/roadmap | S–M |

Rough total: **L, ~1.5–2 weeks** of focused work. Task 6 (tracking every spawn and
honouring the token) is the long pole; the controller (1, 7, 14) and the rail (12)
follow.

## 11. Verification plan

No CI on `main`; the pre-push hook runs `scripts/security-check.sh`.

- **Unit:** key resolution (valid team id, hashed URL, hostile input rejected, URL
  normalization idempotent); the once-only legacy migration (global settings →
  `settings.db`, `cache.db` → `cache-<key>.db`, second workspace untouched);
  `available_workspaces` including a keychain-only entry; `xoxd` round-trip and
  legacy migration; `load_credentials(None)` resolution.
- **Behaviour:** two fake workspaces receive independently; a focus swap does not
  stop the other; **turn off cancels and the barrier waits — no further writes** to
  the old state/store; turn on rebuilds; a call is confirmed stopped on turn-off;
  turning off the last workspace shows the login screen.
- **Concurrency:** assert an untracked-writer regression is impossible (a spawned
  store writer is cancelled by turn-off).
- **Snapshot:** the rail (two workspaces: focused, one with a badge, one off).
- **Manual:** `login --workspace` twice, switch from the rail, confirm each shows
  its own channels, notifications arrive from both (with the right title),
  background call survives a focus switch, and turning one off stops its
  notifications without touching the other.

## 12. Risks & mitigations

- **Cross-workspace leakage** — worst failure: per-workspace stores/state, a test
  that two states never cross.
- **Detached writers after turn-off** — track **every** spawn and honour the token;
  test it (task 6).
- **DB-key instability** — a key that changes between runs silently resets the
  cache; resolve from the registry and keep normalization identical everywhere.
- **Credentials clobbering** — the shared `xoxd` today is the sharpest edge; fix
  before running anything concurrently.
- **Global settings ownership** — must be split before the first per-workspace DB,
  or a workspace silently owns the locale/zoom.
- **Login re-exec** — re-exec on "Add a workspace" would kill every other runtime
  and any background call; prefer building the new `App` in-process (needs `App`
  to be constructible at runtime — it is not hot-swappable today because creds are
  immutable).
- **Focus-swap state** — audit every `Gui` field for one-workspace assumptions
  (§6.6); a missed one leaks context across a switch.
- **PR scope** — the `security.yml` fix (finding 25) is a real code change mixed
  into an "analysis only" PR; either split it into its own PR, or keep the PR
  title/description explicit that it also fixes the gitleaks permission.

## 13. Open questions

1. **Rail details** — width, badge count vs dot, always visible or collapsing.
2. **"Turn off" vs "mute"** — turning off, or also a per-workspace *mute*?
3. **Calls** — block "turn off" during a call, or warn and end it?
4. **Default focus at startup** — last focused (global setting), or `--workspace`?
5. **Adding a workspace** — browser login only, or also pick from the registry?
6. **Background calls** — allow several, or limit how many can be live at once?

## 14. Reviews

- **Copilot PR review, pass 1** (2026-10-10): 7 findings, folded in (§5, items 1–7).
- **Internal architecture review** (2026-10-10): §5 items 8–20, including five the
  first draft missed (DB-key timing, migration scope, the barrier covering all
  writers, the settings handle, the CLI/TUI path). Its cross-cutting conclusion —
  the controller does all global setup before building any `App` — is §6.0.
- **Copilot PR review, pass 2** (2026-10-10): 9 findings, folded in (§5, items
  21–29), including the cloneable supervisor, the cache identity per user, the
  keychain-install upgrade path, the persisted `db_key`/`enabled`, and `last_channel`
  being per workspace.
- **Copilot PR review, pass 3** (2026-10-10): its **new** findings are items 30–33
  (plus one relisted carry-over) — consistency fixes with no new architectural
  blocker (the migration name, the keychain-recovery limitation, `last_channel` in
  the task table, and two workflow nuances).
- **Copilot PR review, pass 4** (2026-10-10): 1 open carry-over (the PR-scope one,
  addressed in the metadata) plus **6 findings reported only in the review body**
  ("Previously missed", no inline thread) — the `Gui.app`/focus invariant, supervised
  image downloads, `settings.db` excluded from `cache clear`, the notification
  heading, and the call completion guard (§5, items 34–39).
- **Copilot PR review, pass 5** (2026-10-10): 0 open, but **5 body-only findings**
  again (§5, items 40–43 + a count fix): drop `Images` on turn-off, resolve recovery
  identity offline, validate `user_id` in paths, and namespace the on-disk image
  cache by account. The recurring theme is still internal drift, but each round keeps
  surfacing real privacy/ lifecycle details.
- **Copilot PR review, pass 6** (2026-10-10): 0 open, **1 nit** — the legacy key is
  `last_channel`, not `last-channel`. Fixed; nothing structural. This is the
  "only nitpicks remain" stopping point.
