# Threat model

`slaq` is a native Slack client that speaks to Slack's private API with **your**
session credentials. This document records what we protect, what we trust, and
the trade-offs we made, so that a contributor (or an auditor) can tell a
deliberate decision from an oversight.

## Assets

| Asset | Where it lives | Why it matters |
|---|---|---|
| `xoxc` session token, `xoxd` session cookie | OS keyring, or `~/.config/slaq/credentials.json` (`0600`) | Full access to the account: read all channels/DMs, post as you, in every workspace you are in |
| Message & channel cache | `~/.local/share/slaq/cache.db` (SQLite) | Plaintext history of what you have read |
| Avatars, emoji, previews | `~/.cache/slaq/images/` | Low sensitivity |
| Downloaded attachments | the user's download folder | Files the user chose to save |
| Live call audio | process memory, never written | Two-way conversation |
| Mod secrets (e.g. an LLM API key) | OS keyring, bound to the exact mod package | Access to the third-party service the user configured |
| Mod grants and settings | `~/.config/slaq/mods-grants.json`, `mods-settings.json` | What each mod may touch; widening them widens what a mod can read or reach |
| Mods' private data | `~/.local/share/slaq/mods/<id>/` | Whatever a mod keeps; an indexing mod (Second Brain) holds plaintext copies of messages, like the cache |

## Trust boundaries

1. **Slack API and WebSocket** (`*.slack.com`) — trusted for authenticity (TLS),
   **not trusted for content**. Everything that arrives in a message, a file
   object, an attachment or a *blocks* payload is untrusted input.
2. **The Chime/SFU signaling server** — untrusted: it feeds us protobuf and SDP.
3. **Local disk / keyring / notification daemon (D-Bus)** — semi-trusted: on a
   shared machine another user must not be able to read our secrets, but a
   process running as us is assumed to already have our privileges.
4. **The browser we launch for `login --browser`** — trusted to be Chromium, but
   the DevTools endpoint it exposes is local-only.
5. **Slack-hosted media vs. any other host** — the crucial boundary for the
   session cookie (see T2).
6. **Mods** — **untrusted code**, written by someone else, that the user chose to
   run. A mod is sandboxed and can do only what the user granted (see T8).

## Threats and how they are handled

### T1 — Credential theft at rest
- The keyring is preferred (Secret Service on Linux, Keychain on macOS,
  Credential Manager on Windows). Before 0.9.0 the keyring library was built
  without a backend and silently held secrets in process memory; the test
  `keyring_store_persists` now fails if the store would not outlive the process.
- The file fallback is created with `0600` **at
  creation** (`OpenOptions::mode(0o600)`, `O_NOFOLLOW`), never widened later
  (`src/auth/token.rs::write_credentials_file`).
- `Credentials` has a hand-written `Debug` that redacts `xoxc`/`xoxd`, so a log
  line or a panic message can never print them (`src/auth/token.rs`).
- `.gitignore` excludes `credentials.json`, `*.db`, `.env`, keys and SDP dumps.

### T2 — Credential leakage over the network / SSRF
- The session cookie is sent **only to Slack-owned hosts**. Image fetches
  (`src/app/images.rs`) and attachment downloads (`src/app/download.rs`) consult
  `net::is_slack_host`, an allow-list over `slack.com`, `slack-edge.com`,
  `slack-msgs.com`, `slack-imgs.com` (host or subdomain, trailing dot stripped).
  A URL that a third party can influence — a file's `url_private`, an avatar, a
  custom emoji, a future link preview — therefore cannot exfiltrate the cookie.
- The API client sends the token/cookie to the workspace host only, and its base
  URL is derived from credentials that originate from `auth.test`.

### T3 — Man-in-the-middle
- HTTPS via `reqwest` (native-tls) and WSS via `tokio-tungstenite` +
  `rustls`/`webpki-roots`. Certificate verification is **never** disabled: there
  is no `danger_accept_invalid_certs` and no pinned/added root anywhere.
- The user agent is honest (`slaq/<version>`); we do not impersonate the official
  client, which is both an ethics and an attribution choice (see README).

### T4 — Hostile content from Slack
- **Rendering** is `mrkdwn` to egui/ratatui primitives: no HTML, no embedded
  browser, no JavaScript is ever executed from a message.
- **File names** are sanitized before touching the filesystem
  (`src/app/download.rs::sanitize`): path separators and `..` are stripped, and
  the destination is always inside the downloads folder. Covered by tests.
- **Image URLs** are hashed to a fixed extension, so no URL controls the file
  name on disk (`src/app/images.rs::file_name`).
- Opening a downloaded attachment uses the OS (`xdg-open`) and is always
  **user-initiated**; the client never auto-opens a file from a message.

### T5 — A malicious or buggy dependency
- `cargo-deny` (advisories, bans, licences, sources), `cargo-audit` and `trivy`
  gate every push to `main` and every pull request (the GitHub workflow), and run
  locally before a push (the pre-push hook, `scripts/security-check.sh`). Known unmaintained transitive crates are listed with a reason in
  `deny.toml`, not silently ignored.
- `cargo-geiger` is available to track `unsafe`. `slaq`'s own `unsafe` is
  concentrated in screen capture/AV1 (`src/calls/screen/*`, `src/calls/video.rs`)
  and is reviewed as such.

### T6 — The browser login flow
- `login --browser` launches Chromium with a **dedicated temporary profile** and
  `--remote-debugging-port=0` (an OS-assigned port), then drives it over CDP to
  capture the token/cookie the login flow produces. It reads credentials from a
  browser we started, not from an arbitrary page.
- The flow records only the token/cookie; debug output prints lengths and
  booleans, never the values (`src/auth/browser.rs`).

### T7 — Huddle media
- Audio is Opus over SRTP, with DTLS keyed through the signaling protocol. The
  DTLS handshake uses `webrtc-dtls`; the SRTP profile negotiation is documented
  in `docs/huddle-protocol.md` (including the GCM profile mistake that cost
  weeks — left in on purpose).

### T8 — Mods
A mod is a WebAssembly component. What constrains it, and where:
- **Sandbox.** Components are built for `wasm32-unknown-unknown`, so they import
  no WASI: no files, sockets, clock or environment of their own. Everything goes
  through the host API (`wit/slaq-mod.wit`), and every import is gated by a
  capability (`src/mods/mod.rs`).
- **Default deny, per capability.** Nothing is granted until the user consents,
  one capability at a time, in the Mods dialog or with `slaq mods`. The
  `min(consent, manifest)` rule (`src/mods/consent.rs`) means a package update
  cannot widen a grant on its own. The user may widen a mod's channels or hosts
  themselves; that is deliberate, and recorded as the user's own choice.
- **Reads are scoped.** `messages.read` is enforced per channel on every read,
  including `history`/`thread-replies` (paced at 30 reads a minute per mod) and
  event delivery.
- **The session never reaches a mod.** Slack calls are made by the host with
  its own client. `http-fetch` has no way to name a host secret, so a mod can
  never obtain or send the token or cookie (T2 still holds).
- **Network** (`net.egress`) is an allow-list enforced per connection:
  - the name must resolve to a public address (DNS rebinding is refused), except
    a literal IP or `localhost` the user listed explicitly;
  - redirects are not followed;
  - request and response sizes are capped.
- **Files** (`filesystem`) are reachable only through slots the user mapped to
  directories. The host does the I/O and contains it: no `..`, no absolute paths,
  no symlink escapes, and slaq's own directories are refused (`src/mods/fs.rs`).
- **Secrets** are typed by the user into slaq's form, stored in the keyring,
  read-only to the mod, and bound to the exact package (a hash of its manifest
  and component). An update or a replacement reusing the id gets them only after
  the user clicks *Keep*, on a prompt that shows the hosts the new package can
  reach.
- **Resources.**
  - Each mod runs on its own thread with a guest-memory cap
    (`SLAQ_MOD_MEMORY_MB`, 512 MiB) and a per-call wall-clock backstop
    (`SLAQ_MOD_DEADLINE_SECS`, 300 s).
  - Its stores have byte quotas.
  - The user can stop it at once from **Running mods**; in-flight requests are
    cancelled.
  - A trapping mod is quarantined without affecting the others.
- **Output.** Everything a mod shows is size-capped and sanitized by the host
  before a front end gets it (`sanitize` / `sanitize_multiline` in
  `src/mods/mod.rs`): its notes, views, dialogs, names and labels.
  - Control characters, bidi overrides and zero-width characters are stripped,
    so a mod's text cannot be made to read differently from what it is.
  - Only line breaks and tabs survive, in multi-line text.

## Accepted risks / limitations

- **A mod the user lets read messages *and* reach the network can send what it
  reads to those hosts.** This is inherent to such a mod (an LLM assistant needs
  both), so it is not prevented. The consent dialog flags the combination, and
  the user chooses the channels and the hosts.
- **Content is untrusted input to LLM-backed mods.** Anyone who can post in the
  workspace can try to steer a model that reads messages (prompt injection). A
  mod that only writes private notes limits the damage to a misleading note. A
  mod that drives a local agent with tools must forward content only on an
  explicit user gesture (`docs/mods/HOW_TO_WRITE_MODS.md`).
- **Mods are unsigned.** The package hash protects secrets across a
  replacement, but nothing proves who wrote a mod. Install mods from people you
  trust; signing is gated on a real need (`docs/mods/roadmap.md`).

- **A local attacker running as the same user** can read the cache, the keyring
  and process memory. Out of scope.
- **Slack can change the private API, and workspace admins can see the
  connection.** Documented in the README; not a vulnerability.
- **`lru`** (via `ratatui`) had two RUSTSEC advisories; resolved by upgrading
  `ratatui` to 0.30, which uses a fixed `lru`.
- **Unmaintained transitive crates** (`paste`, `bincode`, `audiopus_sys`,
  `fxhash`, `ttf-parser`): no maintained replacement exists without an upstream
  bump; each is recorded with its rationale in `deny.toml`.

## Open follow-ups

- [x] Bump `ratatui` 0.29 → 0.30 to drop the `lru` advisories (done: 0.30 uses
      `lru` 0.18, no advisory).
- [x] Fuzz the parsers that eat untrusted bytes: `mrkdwn` and the Chime
      signaling decode have `cargo fuzz` targets (`fuzz/`); SDP handling is not
      fuzzed yet.
- [ ] Add a clippy job that denies `unwrap`/`expect` in the non-test,
      network-fed paths.
- [ ] Turn `sonar-project.properties` into a required check on the default
      branch once a SonarCloud project exists.
