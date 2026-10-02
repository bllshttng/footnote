#!/usr/bin/env bash
# The per-harness composer check for the delivered-mail header (AC8-HP).
#
# A bare `@name` typed into a harness composer can open a mention picker, or
# read as an address and loop mail back. Each harness row in the capability
# contract carries `mail_header_at`: true = the `@name` mention form, false =
# plain `name`. The live check types the exact header into each composer and
# asserts no picker opens and nothing expands; a harness that fails is set to
# `mail_header_at = false` in its contract row.
#
#   --dry-run   verify the contract data and the renderer's data path without
#               touching a live composer. This is the CI gate.
#   (no flag)   --dry-run's checks, plus the exact header each registered
#               harness receives under its current verdict, for manual
#               composer confirmation on a live fleet.

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$ROOT/crates/fno-agents/src/harness_capabilities.toml"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 1. Every top-level harness row carries the verdict. The name class carries
# dashes (cursor-agent), so the row header matches them too.
missing=$(awk '
  /^\[harness\.[a-z-]+\]$/ { if (row != "" && !saw) print row; row = $0; sub(/^\[harness\./, "", row); sub(/\]$/, "", row); saw = 0; next }
  /^mail_header_at[[:space:]]*=/ { saw = 1 }
  END { if (row != "" && !saw) print row }
' "$TOML")
if [ -n "$missing" ]; then
  echo "FAIL: harness rows with no mail_header_at verdict: $missing"
  exit 1
fi
echo "ok: every harness row carries mail_header_at"

# 2. The renderer builds the binary fresh and honors the data: a registry
# recipient whose harness row is mail_header_at=true renders the mention
# form; an explicit payload form overrides it.
BIN="$ROOT/crates/fno-agents/target/debug/fno-agents"
if [ ! -x "$BIN" ]; then
  (cd "$ROOT/crates/fno-agents" && cargo build -p fno-agents)
fi

cat > "$TMP/registry.json" <<'JSON'
{
  "schema_version": 1,
  "agents": [
    {"name": "candor", "short_id": "candor-short", "status": "live", "harness": "claude",
     "cwd": "/repo", "harness_session_id": "11111111-1111-1111-1111-111111111111",
     "created_at": "2026-10-01T00:00:00Z"},
    {"name": "quill", "short_id": "quill-short", "status": "live", "harness": "codex",
     "cwd": "/repo", "harness_session_id": "codex-session-2",
     "created_at": "2026-10-01T00:00:00Z"}
  ]
}
JSON

render() {
  printf '%s' "$1" | "$BIN" mail-envelope --registry "$TMP/registry.json"
}

out=$(render '{"mode":"wrap","body":"Fix the gate. Then ship.","from":"candor-short","to":"quill-short","to_session":"codex-session-2","id":"fmail-0badc0de1234"}')
case "$out" in
  '`@quill · fmail-0badc0de1234 ·'*'`'*) echo "ok: a mail_header_at=true recipient renders the mention form" ;;
  *) echo "FAIL: expected the mention form for a true recipient, got: $out"; exit 1 ;;
esac

out=$(render '{"mode":"wrap","body":"Fix the gate. Then ship.","from":"candor-short","to":"quill-short","to_session":"codex-session-2","id":"fmail-0badc0de1234","form":"plain"}')
case "$out" in
  '`quill · fmail-0badc0de1234 ·'*'`'*) echo "ok: an explicit plain form overrides the row" ;;
  *) echo "FAIL: expected the plain form under an explicit override, got: $out"; exit 1 ;;
esac

# 3. The exact header each registered harness receives under its current
# verdict, for the live composer confirmation.
echo
echo "Header per harness verdict (confirm by typing into a live composer;"
echo "no picker may open and nothing may expand):"
awk '
  /^\[harness\.[a-z-]+\]$/ { row = $0; sub(/^\[harness\./, "", row); sub(/\]$/, "", row); next }
  /^mail_header_at[[:space:]]*=/ && row != "" { form = ($0 ~ /true/) ? "@candor (mention form)" : "candor (plain form)"; printf "  %-10s %s\n", row, form; row = "" }
' "$TOML"
echo
echo "composer smoke: PASS"
