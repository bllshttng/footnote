#!/usr/bin/env bash
set -euo pipefail
TMPHOME=$(mktemp -d)
trap 'rm -rf "$TMPHOME"' EXIT
REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
export HOME="$TMPHOME"

# First install
OUT=$(bash "$REPO_ROOT/scripts/install-drain-prompt.sh")
echo "$OUT" | grep -q "^installed: " || { echo "first install: expected 'installed:' got: $OUT"; exit 1; }
[[ -f "$TMPHOME/.fno/inbox-drain-prompt.md" ]] || { echo "target missing after install"; exit 1; }
diff -q "$TMPHOME/.fno/inbox-drain-prompt.md" "$REPO_ROOT/scripts/templates/inbox-drain-prompt.md" >/dev/null || { echo "content mismatch"; exit 1; }

# Idempotent re-install: must NOT clobber
echo "user-customized" > "$TMPHOME/.fno/inbox-drain-prompt.md"
OUT2=$(bash "$REPO_ROOT/scripts/install-drain-prompt.sh")
echo "$OUT2" | grep -q "^already installed: " || { echo "re-install: expected 'already installed:' got: $OUT2"; exit 1; }
grep -q "user-customized" "$TMPHOME/.fno/inbox-drain-prompt.md" || { echo "user customization was clobbered"; exit 1; }

echo "OK"
