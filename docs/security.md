# Security checks — a practical guide

`slaq` holds your Slack session (the `xoxc` token and `xoxd` cookie). Anyone who
gets them can read and write your Slack as you. This page explains, in plain
terms, how we look for ways the program could leak or misuse them — what each
tool does, the exact command, how to read the output, and what to do next.

You do not need to be a security expert. Follow the steps.

---

## The 10-second version

Push. CI (`.github/workflows/security.yml`) runs every check below on each push
to `main` and each pull request. It fails when something is *really* wrong: a
secret, a known vulnerable library, a denied licence, a CRITICAL/HIGH CVE. The
**pre-push hook** (`scripts/install-hooks.sh`, once) runs the same checks
locally first (`scripts/security-check.sh`), so you find out before pushing.

## The local version

Run this before pushing anything that touches **login, the network, parsing, or
files**. It takes a few minutes and catches most things before CI does.

### 1. One-time setup

```bash
# build tools (Debian/Ubuntu/Mint)
sudo apt install -y build-essential pkg-config nasm cmake \
  libssl-dev libasound2-dev libxcb1-dev libx11-dev libgl1-mesa-dev libegl1-mesa-dev

# scanners
python3 -m venv ~/.venvs/semgrep && ~/.venvs/semgrep/bin/pip install semgrep
cargo install cargo-audit cargo-deny cargo-geiger
# trivy / gitleaks / osv-scanner: single binaries, same version as CI
curl -sSL https://github.com/aquasecurity/trivy/releases/latest/download/trivy_0.75.0_Linux-64bit.tar.gz | tar xz trivy && sudo install -m755 trivy /usr/local/bin/
```

---

### 2. Secrets in the code — `gitleaks`

**What it does.** Scans the source *and its whole git history* for things that
look like passwords, API keys or tokens. A secret committed once is public
forever, even if you delete it later.

```bash
gitleaks git .          # whole history
gitleaks dir .          # just the files on disk right now
```

**How to read it.** `no leaks found` means fine. `leaks found: N` lists the
files. The defaults live in `.gitleaks.toml`; we only added exclusions for test
fixtures and the `target/` build folder.

**If it finds a real secret.** Do not just delete it — *rotate it first* on
Slack (it is already exposed), then remove it from history with
[`git filter-repo`](https://github.com/newren/git-filter-repo).

---

### 3. The libraries we use — `cargo-deny`, `cargo-audit`, `trivy`

**What they do.** They check the third-party crates in `Cargo.lock` against
databases of known vulnerabilities (CVEs / RUSTSEC), flag unmaintained crates,
and check that every dependency's licence is one we accept.

```bash
cargo deny check                     # advisories + licences + bans + sources
cargo audit                          # RUSTSEC, human-readable
trivy fs --scanners vuln,secret,misconfig --severity HIGH,CRITICAL .
```

**How to read it.** Each line names a crate, a CVE id and a *fixed version*.

**What to do.** In order of preference:

1. **Update the crate** — usually the whole fix:
   ```bash
   cargo update -p some-crate            # within the allowed version range
   cargo update -p some-crate --precise 1.2.3   # pin an exact fixed version
   ```
   Then `cargo build && cargo test` and commit the new `Cargo.lock`.
2. **No fix exists and the crate is unmaintained/transitive** — add the advisory
   id to `deny.toml` under `[advisories] ignore`, **with a written reason**
   saying why it is not exploitable in our code. Never silence one without a
   reason; the file is the audit trail.

---

### 4. Bugs in *our* code — `semgrep`

**What it does.** Reads `src/` looking for risky patterns: shell/command
construction, unsafe cryptography, hardcoded secrets, `unsafe` blocks,
temporary-file misuse. It is a linter with security knowledge.

```bash
semgrep scan --config p/rust --config p/security-audit --metrics=off .
```

**How to read it.** Each finding prints a `severity`:

- `ERROR` — treat as a bug, fix it;
- `WARNING` — likely needs a small change;
- `INFO` — just a heads-up. Most of ours are the expected `unsafe` blocks in the
  call/screen-share code and hints about temporary directories. Open the file,
  decide, and move on.

CI **fails only on `ERROR`**, so informational findings are reviewed by a human
instead of blocking everyone.

---

### 5. Code smells and hotspots — SonarQube / SonarCloud

**What it does.** Measures maintainability: very long functions, duplicated code,
and "security hotspots" (places worth a second look). It does not replace the
tools above; it tells you where the code is getting hard to reason about.

Natively supports Rust since 2025.1. Local setup (Docker):

```bash
docker run -d --name slaq-sonarqube -p 9000:9000 \
  -e SONAR_ES_BOOTSTRAP_CHECKS_DISABLE=true sonarqube:community
# open http://localhost:9000 (admin/admin), change the password,
# then My Account -> Security -> Generate Tokens
docker run --rm --network host -v "$PWD:/usr/src:ro" -w /usr/src \
  sonarsource/sonar-scanner-cli \
  -Dsonar.host.url=http://localhost:9000 -Dsonar.token=<TOKEN>
```

Project settings are in `sonar-project.properties`. In CI the `sonar` job
publishes to SonarCloud when the `SONAR_TOKEN` secret exists, and is skipped
otherwise.

---

### 6. Compiler and `unsafe` — `clippy`, `cargo-geiger`

```bash
cargo clippy --all-targets -- -D clippy::correctness -D clippy::suspicious
cargo geiger
```

**What they do.** `clippy` is the Rust linter; we make *bugs* (correctness,
suspicious) fail the build, while style warnings stay warnings. `cargo geiger`
counts `unsafe` per crate: it is expected in `webrtc`, `xcb`, `rav1d` and in
`slaq`'s screen-capture code, and reviewed there.

---

## The one rule you must not break

**The `xoxd` session cookie may only ever be sent to Slack's own hosts.**
Everything else — an image URL, a file link, an avatar, a future link preview —
can be influenced by someone else, and sending the cookie there hands them your
session.

That rule is implemented by one function, `net::is_slack_host` (see
`src/net.rs`), used by every download that carries the cookie
(`src/app/images.rs`, `src/app/download.rs`), and covered by unit tests. **Any new
code that sends that cookie must go through it.** If you add a feature that
fetches a URL and you are unsure, ask.

The same rule holds for **mods**: they never see the token or the cookie.
- Slack calls made for a mod (`history`, `chat-send`, `file-get`, …) are made
  by slaq itself.
- A mod's own `http-fetch` has no way to name one of slaq's secrets.
- A new host function for mods must keep it that way. The mod threat model is
  `docs/threat-model.md`, T8.

---

## Fuzzing the parsers (occasional)

The two parsers that eat untrusted bytes are fuzzed with coverage-guided fuzzing
(`cargo-fuzz`), see [`../fuzz/README.md`](../fuzz/README.md). Run a bounded pass
before a release, or after touching `src/app/mrkdwn.rs` or
`src/calls/signaling.rs`:

```bash
cargo +nightly-2026-09-15 fuzz run mrkdwn -- -max_total_time=300
cargo +nightly-2026-09-15 fuzz run signaling -- -max_total_time=300
```

A panic closes the client (release is `panic = "abort"`), so a crash here is a
real defect — reproduce it and add the input to `fuzz/corpus/`.

Besides the fuzzers, the `naughty_strings_never_panic` test (`src/main.rs`) runs
the whole [`big-list-of-naughty-strings`][blns] through the parsers on every
`cargo test`: unicode edge cases, control characters, RTL, combining marks.

[blns]: https://github.com/minimaxir/big-list-of-naughty-strings

---

## UI regression snapshots

`cargo test` renders the interface with `egui_kittest` and compares the image
against `tests/snapshots/`. This catches visual regressions (font sizes,
colours, spacing, rounding) that a logic test cannot see.

The real window is driven headlessly from the offline `--demo` workspace, so
the screens are covered without a network: `main_conversation`, `thread_panel`,
`login_screen`, `find_bar`, `notification_center`, `profile_popover`,
`search_results`, `channel_info`, `emoji_picker`, `editing_a_message`,
`self_mention`, `symbols`, plus a theme/widget slice and the `bold_family` font
check. A `Gui` is built with the same `Gui::new` the app uses (`Gui::draw` is
`update` minus the eframe window), so the tests exercise the production drawing
path.

Two global inputs are pinned so the images do not move for reasons unrelated to
the interface: the UI language is forced to `it` (the value every other test
sets, in `rust_i18n`'s process-global locale), and the version line in the
profile popover is fixed under `cfg(test)` (it embeds the build date and git
hash, which change every commit).

The renderer is wgpu. On a machine without a GPU it uses the Vulkan **Lavapipe**
software driver (`mesa-vulkan-drivers` on Debian/Ubuntu), so snapshots also run
headless; install that package if rendering fails. Pixel differences are
tolerated up to a small threshold, but a different anti-aliasing implementation
can still shift pixels — a snapshot that flakes across machines is a renderer
difference, not a UI change.

After an *intended* UI change:

```bash
UPDATE_SNAPSHOTS=1 cargo test gui::snapshot_tests
# then review tests/snapshots/*.png and commit the new baseline
```

`tests/snapshots/*.new.png` and `*.diff.png` are throwaway and gitignored.

The layout-invariant test `thread_panel_does_not_grow` needs no GPU: it drives
the panels for two frames and asserts the central area does not shrink (the
"thread panel grows until it fills the screen" bug).

The gap *between* panels is guarded separately, because a golden snapshot would
just bake the bug in: `panels_leave_no_unpainted_column` renders the window
headless and asserts that no column of pixels is left transparent top-to-bottom
(the renderer clears transparent and every panel is drawn opaque, so a
transparent column is a strip no panel painted). It covers both boundaries —
sidebar/chat, and chat/thread with the thread open. That is the "white (or
black) column between the chat list and the chat": it shows up whenever a
sidebar row grows wider than its panel, pushing the central panel right. Keep
every row within the panel width (see the dynamic truncation in `channel_row`).

`top_bar_fits_the_profile` covers the other kind of clipping: it asserts the
profile button sits inside the top bar with room above and below. The bar once
used egui's 2px default vertical margin, which left the 28px avatar and the
bell badge flush against the panel edges, so they looked cut.

---

## Before you commit — checklist

```bash
cargo build --locked
cargo test
cargo deny check
gitleaks dir .
semgrep scan --config p/rust --config p/security-audit --metrics=off .
```

All green (or every warning understood and noted) → commit. The full gate,
the one the pre-push hook runs, is `scripts/security-check.sh`.

## When a check fails and you are stuck

- **Cannot update a crate** (would need a breaking change): open an issue,
  describe it, and add the advisory to `deny.toml` with a reason in the
  meantime.
- **Not sure whether a semgrep finding is real**: err on the side of asking.
- **A finding looks wrong (false positive)**: add a *narrow* exclusion — a
  specific rule id or file, never a whole directory — and say why in a comment.
