#!/usr/bin/env bash
set -u

repo_root="$PWD"
test_root="${TMPDIR:-/tmp}/fno-mail-pane-test-$$"

if [[ ! -f "$repo_root/hooks/hooks.json" || ! -f "$repo_root/hooks/mail-pane.test.ts" ]]; then
  echo "test-mail-pane: run from the Footnote repository root" >&2
  exit 2
fi
if [[ -e "$test_root" ]]; then
  echo "test-mail-pane: refusing existing path $test_root" >&2
  exit 2
fi

mkdir -p "$test_root/.claude-plugin" "$test_root/hooks" || exit 2
printf '%s\n' '{"name":"fno","version":"0.4.1","description":"Isolated mail pane test host."}' > "$test_root/.claude-plugin/plugin.json"
printf '%s\n' '{"modules":["./register.mjs"]}' > "$test_root/hooks/hooks.json"
if ! cp "$repo_root/hooks/register.mjs" "$repo_root/hooks/mail-pane.mjs" "$repo_root/hooks/mail-pane.test.ts" "$test_root/hooks/"; then
  echo "test-mail-pane: could not stage source into $test_root" >&2
  exit 2
fi

test_rc=0
claude plugin test "$test_root" || test_rc=$?

rm -f "$test_root/.claude-plugin/plugin.json" "$test_root/hooks/hooks.json" \
  "$test_root/hooks/register.mjs" "$test_root/hooks/mail-pane.mjs" \
  "$test_root/hooks/mail-pane.test.ts"
rmdir "$test_root/.claude-plugin" "$test_root/hooks" "$test_root" 2>/dev/null || true
exit "$test_rc"
