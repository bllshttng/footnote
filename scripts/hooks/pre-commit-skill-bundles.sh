#!/usr/bin/env bash
# Pre-commit hook: regenerate skill bundles from canonical sources, then
# re-stage any bundle file that changed. This keeps committed bundles in
# sync with the manifest declaration in skill-bundles.yaml.
#
# Install (one-time, per clone):
#   ln -sf "$(git rev-parse --show-toplevel)/scripts/hooks/pre-commit-skill-bundles.sh" \
#          "$(git rev-parse --git-path hooks)/pre-commit"
#
# Then on every commit, this hook regenerates bundles and re-stages any
# bundle file that was updated. Developers who edit canonical scripts
# (scripts/lib/...) won't have to remember to run the generator manually.
#
# CI enforcement (scripts/lint/check-skill-bundles-fresh.sh) catches the
# case where a developer commits without this hook installed.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"

# Run generator. It writes into skills/*/scripts/.
bash "$REPO_ROOT/scripts/generate-skill-bundles.sh" >/dev/null

# Re-stage any bundle file that's now different from the index. We only
# touch files under the three bundle-dest roots (scripts/, references/,
# agents/) so we never grab unrelated changes the developer didn't intend
# to include. NUL-delimited so paths with spaces or shell-meta chars
# round-trip cleanly (git default-quotes them on plain `--name-only`
# output, breaking direct `git add`).
#
# Process-substitution (not `git diff | while ...`) so `set -e` propagates
# from `git add` failures inside the loop. A piped subshell would silently
# swallow a permission/lock failure and let the commit proceed with
# un-staged regenerated files.
while IFS= read -r -d '' path; do
  [[ -z "$path" ]] && continue
  git add "$REPO_ROOT/$path"
  echo "pre-commit: re-staged regenerated bundle $path" >&2
done < <(git diff --name-only -z \
  -- 'skills/*/scripts/' 'skills/*/references/' 'skills/*/agents/')
