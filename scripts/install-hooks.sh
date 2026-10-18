#!/usr/bin/env bash
#
# Install the repository's Git hooks.
#
# It points Git at the versioned hooks in `scripts/githooks/`, so they travel
# with the repository instead of living (and rotting) in `.git/hooks/`.
#
#     scripts/install-hooks.sh
#
# This sets `core.hooksPath` in the local repository config. Undo it with:
#
#     git config --unset core.hooksPath

set -euo pipefail
cd "$(dirname "$0")/.."

git config core.hooksPath scripts/githooks
echo "Git hooks installed: core.hooksPath = scripts/githooks"
echo "The pre-push hook runs scripts/security-check.sh; skip it with 'git push --no-verify'."
