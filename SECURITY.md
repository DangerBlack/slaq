# Security Policy

`slaq` is an unofficial Slack client. It holds session credentials (`xoxc`/`xoxd`)
that grant full access to your account, so security bugs here are taken
seriously. This document says what is in scope, how to report, and how the
project keeps itself honest with automated tools.

## Reporting a vulnerability

**Do not open a public issue.** Use GitHub's private reporting:

1. go to the **Security** tab of this repository;
2. **Report a vulnerability** (GitHub Security Advisories).

If you cannot use GitHub, email the maintainer listed in `Cargo.toml`. Expect an
acknowledgement within **72 hours** and a first assessment within **7 days**.

Please include: affected version/commit, a description, a proof of concept if
you have one, and the impact you believe it has.

We will credit you in the advisory unless you ask otherwise.

## What counts as a vulnerability

- exfiltration of `xoxc`/`xoxd` (to a network host, a log, a crash report, a
  file readable by other users);
- a way to make the client talk to a host we did not intend, or to bypass TLS
  verification;
- remote code execution, or command injection, driven by data that arrives from
  Slack (a message, a file name, a file's URL, an attachment);
- path traversal, or writing/opening files outside the documented locations;
- denial of service that a crafted message or a hostile server can trigger
  (panic on the main paths, unbounded allocation, panic in a parser);
- memory-safety issues in `slaq`'s own `unsafe` code;
- for **mods**:
  - a mod escaping its sandbox;
  - a mod using a capability, a channel, a host or a folder the user did not
    grant;
  - a mod obtaining slaq's session or another mod's secrets;
  - a mod getting text past the host's sanitizing.
  See `docs/threat-model.md`, T8.

## What is out of scope

- the fact that your **workspace administrators can see that an unofficial
  client connected** — this is documented and intentional;
- using the client at all against Slack's Terms of Service;
- the content of the messages shown (that is Slack's data, not ours);
- what a mod does **within** what the user granted it (a mod allowed to read
  messages and reach a host can send what it reads there; the consent dialog
  says so);
- issues that require an already-compromised machine (a local attacker who can
  read your process memory, your keyring or your `$HOME` is out of the threat
  model — see `docs/threat-model.md`).

## How the project protects itself

Every push to `main` and every pull request runs, in CI
(`.github/workflows/security.yml`):

| Tool | What it checks |
|---|---|
| `cargo-deny` | advisories (RUSTSEC), licences, banned/duplicate crates, sources |
| `cargo-audit` | RUSTSEC advisory database |
| `gitleaks` | secrets committed to the tree and its history |
| `semgrep` | SAST rules for Rust, secrets and the security-audit ruleset |
| `trivy` | dependency CVEs, secrets and misconfiguration (fails on CRITICAL/HIGH) |
| `cargo clippy` | `correctness`/`suspicious` lints deny the build |
| SonarCloud | code smells, duplication, security hotspots (when configured) |

Run the same checks locally with `scripts/security-check.sh` (the pre-push hook
runs it; install it with `scripts/install-hooks.sh`), or one at a time:

```bash
cargo deny check
cargo audit
semgrep scan --config p/rust --config p/security-audit .
trivy fs --scanners vuln,secret,misconfig .
gitleaks git .
```

The design decisions behind the threat model — what we trust and what we
deliberately do not — are written down in [`docs/threat-model.md`](docs/threat-model.md).
