#!/usr/bin/env bash
# End-to-end check of the registry-miss heal (x-da8c): a stored-but-unregistered
# session must be addressable by the Rust lifecycle verbs, which resolve through
# a `fno agents heal-token` shellout. The Rust unit tests cover the pure parts;
# only this tree can exercise the shellout itself.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/crates/fno-agents/target/debug/fno-agents"
VENV_PY="$ROOT/cli/.venv/bin/python"

if [[ ! -x "$BIN" ]]; then
  echo "SKIP: no debug fno-agents binary (cargo build --manifest-path crates/fno-agents/Cargo.toml)"
  exit 0
fi
if [[ ! -x "$VENV_PY" ]]; then
  echo "SKIP: no cli venv (uv sync --project cli)"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/projects/-repo-one" "$tmp/codex/2026/07/20" "$tmp/home"

# Shim `fno` to this worktree's source, so the shellout exercises the code under
# test rather than whatever version happens to be installed.
cat > "$tmp/bin/fno" <<EOF
#!/bin/sh
exec "$VENV_PY" -c 'from fno.cli import app; app()' "\$@"
EOF
chmod +x "$tmp/bin/fno"

CLAUDE_UUID=c655c326-1111-2222-3333-444455556666
TWIN_UUID=c655c326-9999-8888-7777-666655554444
printf '{"type":"summary"}\n{"type":"user","cwd":"/repo/one"}\n' \
  > "$tmp/projects/-repo-one/$CLAUDE_UUID.jsonl"

export HOME="$tmp/home"
export FNO_CLAUDE_PROJECTS_DIR="$tmp/projects"
export FNO_CODEX_SESSIONS_DIR="$tmp/codex"
export PYTHONPATH="$ROOT/cli/src"
export PATH="$tmp/bin:$PATH"
REGISTRY="$tmp/home/.fno/agents/registry.json"

fail() { echo "FAIL: $1"; exit 1; }

# 1. A session-shaped token the store knows resolves and is ADOPTED. `resume
#    --print-command` is the assertion surface: it proves resolution AND that the
#    healed row carries the uuid the dead arm needs (`logs` would hand off to
#    claude's own job store, which knows nothing about a reaped session).
out=$("$BIN" resume c655c326 --print-command 2>&1) || fail "resume of a stored session: $out"
grep -q -- "--resume $CLAUDE_UUID" <<<"$out" || fail "dead arm did not build the uuid argv: $out"
grep -q "cd /repo/one" <<<"$out" || fail "healed row lost its recorded cwd: $out"
"$VENV_PY" - "$REGISTRY" <<'PY' || fail "adopted row is wrong (see above)"
import json, sys
rows = json.load(open(sys.argv[1]))["agents"]
assert len(rows) == 1, rows
r = rows[0]
# Store membership proves the session EXISTS, never that it runs.
assert r["status"] == "orphaned", r
assert r["short_id"] == "c655c326", r
assert r["cwd"] == "/repo/one", r
PY

# 2. A name-shaped token never probes: byte-identical refusal, nothing adopted.
before=$(cat "$REGISTRY")
err=$("$BIN" logs reviewer 2>&1); rc=$?
[[ $rc -eq 13 ]] || fail "name miss exited $rc, want 13"
[[ "$err" == "no agent matching 'reviewer'; accepted forms: name, 8-hex short id, or full session id" ]] \
  || fail "name miss message drifted: $err"
[[ "$(cat "$REGISTRY")" == "$before" ]] || fail "a name-shaped miss mutated the registry"

# 3. A broken heal path degrades to the ORIGINAL not-found error, never a new
#    error class -- here by taking `fno` off PATH entirely.
err=$(PATH=/usr/bin:/bin "$BIN" logs deadbeef 2>&1); rc=$?
[[ $rc -eq 13 ]] || fail "degraded heal exited $rc, want 13"
[[ "$err" == "no agent matching 'deadbeef'; accepted forms:"* ]] \
  || fail "degraded heal did not reproduce today's error: $err"

# 4. A registry write failure does not block the verb, and does not hide either:
#    reaching the session wins, but the operator must see that the roster did not
#    get the row. Skipped when the chmod does not actually deny us (e.g. root).
rm -f "$REGISTRY"
chmod 500 "$(dirname "$REGISTRY")"
if ! { : > "$REGISTRY"; } 2>/dev/null; then
  out=$("$BIN" resume c655c326 --print-command 2>&1); rc=$?
  chmod 700 "$(dirname "$REGISTRY")"
  [[ $rc -eq 0 ]] || fail "unwritable registry blocked the verb (exit $rc): $out"
  grep -q -- "--resume $CLAUDE_UUID" <<<"$out" || fail "verb did not reach the session: $out"
  grep -qi "WARN" <<<"$out" || fail "failed registration was silent: $out"
else
  chmod 700 "$(dirname "$REGISTRY")"
fi

# 5. Ambiguity refuses loudly with EVERY candidate and adopts nothing.
printf '{"type":"session_meta","payload":{"id":"%s","cwd":"/repo/two"}}\n' "$TWIN_UUID" \
  > "$tmp/codex/2026/07/20/rollout-2026-07-20T10-00-00-$TWIN_UUID.jsonl"
rm -f "$REGISTRY"
err=$("$BIN" logs c655c326 2>&1); rc=$?
[[ $rc -ne 0 ]] || fail "ambiguous token was resolved instead of refused"
grep -q "$CLAUDE_UUID" <<<"$err" || fail "ambiguity message omits the claude candidate: $err"
grep -q "$TWIN_UUID" <<<"$err" || fail "ambiguity message omits the codex candidate: $err"
[[ ! -s "$REGISTRY" ]] || fail "an ambiguous token adopted a row"

echo "PASS"
