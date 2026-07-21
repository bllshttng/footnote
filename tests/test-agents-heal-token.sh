#!/usr/bin/env bash
# End-to-end check of the registry-miss heal (x-da8c): a stored-but-unregistered
# session must be addressable by the Rust lifecycle verbs, which resolve through
# a `fno agents heal-token` shellout. The Rust unit tests cover the pure parts;
# only this tree can exercise the shellout itself.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/crates/fno-agents/target/debug/fno-agents"
VENV_PY="$ROOT/cli/.venv/bin/python"

# Exit 77, not 0: a skip must not read as a green run. `smoke.sh --only '*heal*'`
# selects this step without the build/sync steps that produce its prerequisites,
# and exit 0 there would report "passed" having asserted nothing.
if [[ ! -x "$BIN" ]]; then
  echo "SKIP: no debug fno-agents binary (cargo build --manifest-path crates/fno-agents/Cargo.toml)"
  exit 77
fi
if [[ ! -x "$VENV_PY" ]]; then
  echo "SKIP: no cli venv (uv sync --project cli)"
  exit 77
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

# `resume` checks its provider CLI is on PATH and exits 14 BEFORE rendering, even
# under --print-command. A CI runner has no claude, so without this stub the
# harness dies at the first assertion having tested nothing. Only ever reached by
# a --print-command call, which returns before any exec, so the stub never runs.
printf '%s\n%s\n' '#!/bin/sh' 'exit 0' > "$tmp/bin/claude"
chmod +x "$tmp/bin/claude"

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
# The contract code, not merely non-zero: `-ne 0` would also pass on a panic.
[[ $rc -eq 13 ]] || fail "ambiguous token exited $rc, want 13 (logs' refusal code)"
grep -q "$CLAUDE_UUID" <<<"$err" || fail "ambiguity message omits the claude candidate: $err"
grep -q "$TWIN_UUID" <<<"$err" || fail "ambiguity message omits the codex candidate: $err"
[[ ! -s "$REGISTRY" ]] || fail "an ambiguous token adopted a row"

# 6. attach resolves through the same wrapper. A healed row's liveness comes from
#    probing reality (locate_session + socket), never the row's `status`, so this
#    fixture - a stored session with no live supervisor - must reach the
#    dead-revivable pointer rather than the pre-heal "no agent matching".
rm -f "$REGISTRY" "$tmp/codex/2026/07/20/rollout-2026-07-20T10-00-00-$TWIN_UUID.jsonl"
err=$("$BIN" attach c655c326 2>&1); rc=$?
[[ $rc -ne 0 ]] || fail "attach of a dead stored session unexpectedly succeeded: $err"
grep -q "no agent matching" <<<"$err" && fail "attach did not heal: $err"
grep -q "has exited" <<<"$err" || fail "attach did not reach the revival pointer: $err"

# 7. The two heal_token guards that no other case reaches. Both swap in a stub
#    `fno`, so they pin the Rust side's parsing rather than the healer's.
stub_fno() { printf '%s\n' '#!/bin/sh' "$1" > "$tmp/bin/fno"; chmod +x "$tmp/bin/fno"; }
restore_fno() {
  cat > "$tmp/bin/fno" <<EOF
#!/bin/sh
exec "$VENV_PY" -c 'from fno.cli import app; app()' "\$@"
EOF
  chmod +x "$tmp/bin/fno"
}
ROW='{"name":"x","harness":"claude","cwd":"/w","log_path":"","short_id":"c655c326","harness_session_id":"'$CLAUDE_UUID'","status":"orphaned"}'

# 7a. Exit code is read BEFORE stdout: a failed heal that prints parseable JSON
#     must still degrade, never yield a half-resolved row.
rm -f "$REGISTRY"   # step 6 adopted the row; the token must be a MISS again
stub_fno "echo '$ROW'; exit 1"
err=$("$BIN" logs c655c326 2>&1); rc=$?
[[ $rc -eq 13 ]] || fail "nonzero heal with parseable JSON exited $rc, want 13"
grep -q "no agent matching" <<<"$err" || fail "nonzero heal did not degrade: $err"

# 7a-bis. An off-contract exit relays ONE labelled line, never the child's raw
#         stderr: a traceback ahead of the refusal is a new error class.
stub_fno "echo 'Traceback (most recent call last):' >&2; echo '  File \"x.py\", line 1' >&2; exit 70"
err=$("$BIN" logs c655c326 2>&1); rc=$?
[[ $rc -eq 13 ]] || fail "off-contract heal exit gave $rc, want 13"
grep -q "no agent matching" <<<"$err" || fail "off-contract heal lost the refusal: $err"
grep -q "heal probe failed (exit 70)" <<<"$err" || fail "off-contract heal hid the cause: $err"
[[ "$(grep -c 'File "x.py"' <<<"$err")" -eq 0 ]] || fail "off-contract heal dumped a raw traceback: $err"

# 7b. A banner ahead of the payload (a first-run `fno` prints setup lines) must
#     not defeat the parse.
# 7c. An exit-0 helper returning a JSON object that is not a usable row must
#     degrade, not resolve: a bare {} would otherwise surface as a confusing
#     missing-cwd failure instead of the clean not-found.
rm -f "$REGISTRY"
stub_fno "echo '{}'; exit 0"
err=$("$BIN" logs c655c326 2>&1); rc=$?
[[ $rc -eq 13 ]] || fail "empty JSON object from an exit-0 heal exited $rc, want 13"
grep -q "no agent matching" <<<"$err" || fail "empty-object heal did not degrade: $err"

rm -f "$REGISTRY"
stub_fno "echo '[setup] path migration complete'; echo '$ROW'; exit 0"
out=$("$BIN" resume c655c326 --print-command 2>&1); rc=$?
[[ $rc -eq 0 ]] || fail "banner ahead of the row broke the parse (exit $rc): $out"
grep -q -- "--resume $CLAUDE_UUID" <<<"$out" || fail "banner case lost the row: $out"
restore_fno

# 8. The heal adopts into the registry the CALLER read, not whichever one the
#    Python side would resolve on its own. Rust honors FNO_AGENTS_HOME and the
#    Python path resolver does not, so without the forwarded --registry the row
#    lands in the default file: the verb still works, so nothing warns, but the
#    roster never gains the row and every later call re-heals.
rm -f "$REGISTRY"
ALT_HOME="$tmp/alt-agents"
mkdir -p "$ALT_HOME"
out=$(FNO_AGENTS_HOME="$ALT_HOME" "$BIN" resume c655c326 --print-command 2>&1) || \
  fail "heal under FNO_AGENTS_HOME failed: $out"
[[ -s "$ALT_HOME/registry.json" ]] || fail "adopted row did not land in FNO_AGENTS_HOME"
[[ ! -s "$REGISTRY" ]] || fail "adopted row leaked into the default registry"

echo "PASS"
