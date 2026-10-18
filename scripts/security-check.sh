#!/usr/bin/env bash
#
# Run the same security checks as CI, locally. One command:
#
#     scripts/security-check.sh
#
# A missing tool is reported and skipped, not an error — so this works on a
# fresh machine where only some scanners are installed. Exit code is non-zero
# if any check that *did* run failed.

set -uo pipefail
cd "$(dirname "$0")/.."

# Cargo tools and the single-binary scanners commonly land here.
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

RED=$'\e[31m'; GREEN=$'\e[32m'; YELLOW=$'\e[33m'; BOLD=$'\e[1m'; RESET=$'\e[0m'
failures=0
skipped=0

have() { command -v "$1" >/dev/null 2>&1; }

section() { printf '\n%s=== %s ===%s\n' "$BOLD" "$1" "$RESET"; }

# check <label> <command...>
check() {
  local label="$1"; shift
  if ! have "$1"; then
    printf '%s  skip%s %s (missing: %s)\n' "$YELLOW" "$RESET" "$label" "$1"
    skipped=$((skipped + 1))
    return
  fi
  section "$label"
  if "$@"; then
    printf '%s  ok%s  %s\n' "$GREEN" "$RESET" "$label"
  else
    printf '%s FAIL%s %s\n' "$RED" "$RESET" "$label"
    failures=$((failures + 1))
  fi
}

# --- the checks ------------------------------------------------------------

check "build"                 cargo build
check "test"                  cargo test
check "clippy (correctness)"  cargo clippy --all-targets -- -D clippy::correctness -D clippy::suspicious
# The mod host is behind the optional `mods` feature: lint and test it in both
# shapes, exactly as CI does, so a mods-only lint does not reach CI first.
check "clippy (mods)"         cargo clippy --all-targets --no-default-features --features mods -- -D clippy::correctness -D clippy::suspicious
check "test (mods, GUI)"      cargo test --features mods
check "test (mods, TUI)"      cargo test --no-default-features --features mods
check "dependencies (deny)"   cargo deny check
check "advisories (audit)"    cargo audit
check "secrets (gitleaks)"    gitleaks dir . --no-banner
check "SAST (semgrep)"        semgrep scan --config p/rust --config p/security-audit --metrics=off --quiet --severity ERROR --error .
# Skip build output and tooling worktrees: `target/` is our own freshly-built
# artifacts (not dependencies to scan), and trivy times out walking its LLVM
# bitcode; `.slim/` holds oh-my-opencode worktrees with their own `target/`.
check "SCA (trivy)"           trivy fs --scanners vuln,secret,misconfig --severity HIGH,CRITICAL --skip-dirs target --skip-dirs .slim --exit-code 1 --quiet .

# --- summary ---------------------------------------------------------------

printf '\n%s--- summary ---%s\n' "$BOLD" "$RESET"
printf 'failures: %s%d%s   skipped: %d\n' \
  "$([ "$failures" -gt 0 ] && printf '%s' "$RED" || printf '%s' "$GREEN")" \
  "$failures" "$RESET" "$skipped"

if [ "$failures" -gt 0 ]; then
  printf '\nRead docs/security.md for how to triage each kind of finding.\n'
  exit 1
fi
printf '\nAll checks passed.\n'
