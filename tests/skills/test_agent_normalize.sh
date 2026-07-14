#!/usr/bin/env bash
# test_agent_normalize.sh - verify skills/agent/scripts/normalize.sh
# (input normalization for /fno:agent spawn verb, task 1.2).
#
# Covers:
#   AC4-EDGE   smart quotes (U+201C/201D/2018/2019) -> straight; name derived
#              spawn-<full-node-id>-<slug> for a node with no --name; the dispatch
#              that the raw shell command would have split succeeds.
#   AC2-ERR    invalid --provider -> status=error (no spawn), valid list shown.
#   Boundaries empty / whitespace-only task -> status=error.
#   Locked #4  explicit -> config (resolve_dispatch_target stub) -> claude.
#   plus: node detection, free-form slug naming, no-merge default + --allow-merge,
#         explicit /command passthrough, explicit --name sanitization.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
NORM="$REPO_ROOT/skills/agent/scripts/normalize.sh"

TMP=$(mktemp -d -t dispatch-norm.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

[[ -f "$NORM" ]] || { echo "normalize script missing: $NORM" >&2; exit 1; }
bash -n "$NORM" || { echo "bash -n rejected $NORM" >&2; exit 1; }

# Pin provider resolution to a deterministic stub so the default-provider path
# never depends on a real ~/.fno provider config.
STUB_EMPTY="$TMP/stub-empty.sh"; printf '#!/usr/bin/env bash\nexit 0\n' > "$STUB_EMPTY"; chmod +x "$STUB_EMPTY"
STUB_CODEX="$TMP/stub-codex.sh"; printf '#!/usr/bin/env bash\necho codex\n' > "$STUB_CODEX"; chmod +x "$STUB_CODEX"
STUB_GARBAGE="$TMP/stub-garbage.sh"; printf '#!/usr/bin/env bash\necho not-a-provider\n' > "$STUB_GARBAGE"; chmod +x "$STUB_GARBAGE"
# Pin node-slug resolution too, so the derived name never depends on a real graph
# read (mirrors the provider stub). Default: empty (no slug). STUB_SLUG echoes a
# fixed slug for the case that asserts the <verb>-<id>-<slug> tail.
STUB_SLUG="$TMP/stub-slug.sh"; printf '#!/usr/bin/env bash\necho dashless-spawn\n' > "$STUB_SLUG"; chmod +x "$STUB_SLUG"
export NODE_SLUG_RESOLVER="$STUB_EMPTY"

field() { printf '%s\n' "$1" | sed -n "s/^$2=//p" | head -1; }

# Smart-quote bytes for assertions (bash 3.2 safe: printf octal, no \u).
LDQ=$(printf '\342\200\234'); RDQ=$(printf '\342\200\235')
LSQ=$(printf '\342\200\230'); RSQ=$(printf '\342\200\231')

# --- AC4-EDGE: smart quotes normalized, name derived tgt-<id8>, no --name ---
SMART="$(printf 'add %sCSV export%s to dashboard' "$LDQ" "$RDQ")"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "$SMART")"
if [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" message)" == *'"CSV export"'* ]] \
   && ! printf '%s' "$OUT" | grep -q "$LDQ" \
   && ! printf '%s' "$OUT" | grep -q "$RDQ"; then
  pass "AC4-EDGE smart double-quotes -> straight, none remain"
else
  fail "AC4-EDGE smart double-quotes: $OUT"
fi

OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "$(printf "it%ss fine" "$RSQ")")"
if [[ "$(field "$OUT" message)" == *"it's fine"* ]] && ! printf '%s' "$OUT" | grep -q "$RSQ"; then
  pass "AC4-EDGE smart single-quote -> straight apostrophe"
else
  fail "AC4-EDGE smart single-quote: $OUT"
fi

# Node with no --name -> spawn-<full-node-id>-<slug> (verb prefix + full id +
# title-derived slug), node captured, /target message. Slug resolution is stubbed
# (NODE_SLUG_RESOLVER) for hermeticity, mirroring the provider stub.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" NODE_SLUG_RESOLVER="$STUB_SLUG" bash "$NORM" --input "ab-deadbeef" --provider claude)"
if [[ "$(field "$OUT" node)" == "ab-deadbeef" ]] \
   && [[ "$(field "$OUT" name)" == "spawn-ab-deadbeef-dashless-spawn" ]] \
   && [[ "$(field "$OUT" message)" == "/target ab-deadbeef no-merge" ]]; then
  pass "AC4-EDGE node id -> node captured + spawn-<full-id>-<slug> name + /target message"
else
  fail "AC4-EDGE node derivation: $OUT"
fi

# A node with no resolvable slug degrades to spawn-<full-node-id> (no trailing dash).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" NODE_SLUG_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude)"
[[ "$(field "$OUT" name)" == "spawn-ab-deadbeef" ]] \
  && pass "node with no resolvable slug -> spawn-<full-id> (no trailing dash)" \
  || fail "node no-slug degrade: $OUT"

# --- AC2-ERR: invalid provider -> error, no node/spawn fields, valid list ---
OUT="$(bash "$NORM" --input "ab-deadbeef" --provider banana)"
if [[ "$(field "$OUT" status)" == "error" ]] \
   && printf '%s' "$OUT" | grep -q "claude" \
   && printf '%s' "$OUT" | grep -q "codex" \
   && printf '%s' "$OUT" | grep -q "gemini"; then
  pass "AC2-ERR invalid provider -> error + valid-provider list"
else
  fail "AC2-ERR invalid provider: $OUT"
fi

# --- Boundaries: empty / whitespace-only task -> error ---
OUT="$(bash "$NORM" --input "")"
[[ "$(field "$OUT" status)" == "error" ]] && pass "Boundary empty task -> error" || fail "Boundary empty: $OUT"
OUT="$(bash "$NORM" --input "$(printf '   \t  ')")"
[[ "$(field "$OUT" status)" == "error" ]] && pass "Boundary whitespace task -> error" || fail "Boundary whitespace: $OUT"

# --- Locked #4: provider resolution explicit -> config -> claude ---
# Explicit wins.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_CODEX" bash "$NORM" --input "ab-deadbeef" --provider gemini)"
[[ "$(field "$OUT" provider)" == "gemini" ]] && pass "Locked#4 explicit provider wins" || fail "explicit provider: $OUT"
# No explicit -> defer to resolver (codex).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_CODEX" bash "$NORM" --input "ab-deadbeef")"
[[ "$(field "$OUT" provider)" == "codex" ]] && pass "Locked#4 defers to resolver (codex)" || fail "resolver defer: $OUT"
# Resolver empty -> claude default.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef")"
[[ "$(field "$OUT" provider)" == "claude" ]] && pass "Locked#4 empty resolver -> claude" || fail "claude default: $OUT"
# Resolver returns garbage (not a valid provider) -> claude, never the garbage.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_GARBAGE" bash "$NORM" --input "ab-deadbeef")"
[[ "$(field "$OUT" provider)" == "claude" ]] && pass "Locked#4 garbage resolver -> claude" || fail "garbage resolver: $OUT"

# --- free-form -> empty node + spawn-<slug> + /target wrap ---
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "fix the flaky login redirect")"
if [[ -z "$(field "$OUT" node)" ]] \
   && [[ "$(field "$OUT" name)" == spawn-fix-* ]] \
   && [[ "$(field "$OUT" message)" == "/target fix the flaky login redirect no-merge" ]]; then
  pass "free-form -> empty node + spawn-<slug> + /target message"
else
  fail "free-form: $OUT"
fi

# --- explicit --name sanitized + honored ---
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --name 'My Worker!!')"
[[ "$(field "$OUT" name)" == "my-worker" ]] && pass "explicit --name sanitized" || fail "name sanitize: $OUT"

# --- --allow-merge suppresses no-merge injection ---
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --allow-merge)"
[[ "$(field "$OUT" message)" == "/target ab-deadbeef" ]] && pass "--allow-merge -> no no-merge" || fail "allow-merge: $OUT"

# --- explicit slash command passed through (no /target wrap) ---
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "/check-pr 42")"
[[ "$(field "$OUT" message)" == "/check-pr 42" ]] && pass "explicit /command passthrough (no /target, no no-merge)" || fail "passthrough: $OUT"

# --- explicit /target ... keeps no-merge default applied once (idempotent) ---
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "/target L ab-deadbeef no-merge")"
[[ "$(field "$OUT" message)" == "/target L ab-deadbeef no-merge" ]] && pass "explicit /target no-merge not duplicated" || fail "idempotent no-merge: $OUT"

# --- explicit /target command exposes its node id (so it gets validated) -----
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "/target ab-deadbeef")"
[[ "$(field "$OUT" node)" == "ab-deadbeef" ]] && pass "node extracted from explicit /target command" || fail "node-in-/target: $OUT"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "/target L ab-deadbeef no-merge")"
[[ "$(field "$OUT" node)" == "ab-deadbeef" ]] && pass "node extracted from /target with size+flags" || fail "node-in-/target flags: $OUT"
# A non-/target explicit command carries no node (no spurious extraction).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "/check-pr 42")"
[[ -z "$(field "$OUT" node)" ]] && pass "non-/target command -> no node" || fail "non-/target node: $OUT"

# --- US2: -i / --interactive -> mode=interactive (default exec) ---
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider codex -i)"
[[ "$(field "$OUT" mode)" == "interactive" ]] && pass "US2 -i -> mode=interactive" || fail "-i mode: $OUT"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider codex --interactive)"
[[ "$(field "$OUT" mode)" == "interactive" ]] && pass "US2 --interactive -> mode=interactive" || fail "--interactive mode: $OUT"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider codex)"
[[ "$(field "$OUT" mode)" == "exec" ]] && pass "US2 default -> mode=exec" || fail "default mode: $OUT"

# --- US3: --yolo opt-in, sandboxed default, claude guard ---
# AC3-EDGE: default -> yolo=0 (never inferred).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider codex)"
[[ "$(field "$OUT" yolo)" == "0" ]] && pass "AC3-EDGE default -> yolo=0 (not inferred)" || fail "default yolo: $OUT"
# AC3-HP/UI: explicit --yolo for codex -> yolo=1 (skill renders it in CONFIRM/argv).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider codex --yolo)"
[[ "$(field "$OUT" yolo)" == "1" ]] && pass "AC3-HP explicit --yolo (codex) -> yolo=1" || fail "codex yolo: $OUT"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider gemini --yolo)"
[[ "$(field "$OUT" yolo)" == "1" ]] && pass "AC3-HP explicit --yolo (gemini) -> yolo=1" || fail "gemini yolo: $OUT"
# AC3-ERR (x-d235): --yolo with claude MAPS to --permission-mode bypassPermissions
# (claude has no --yolo flag; bypassPermissions is its full-auto/no-gates
# equivalent), not dropped. yolo is cleared, permission_mode is set, status ok.
ERRF="$TMP/yolo.err"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude --yolo 2>"$ERRF")"
if [[ "$(field "$OUT" yolo)" == "0" ]] && [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" permission_mode)" == "bypassPermissions" ]]; then
  pass "AC3-ERR --yolo + claude -> mapped to permission_mode=bypassPermissions (yolo cleared), still ok"
else
  fail "AC3-ERR claude yolo: out=$OUT err=$(cat "$ERRF")"
fi
# and an explicit --permission-mode the user passed WINS over the yolo default.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude --yolo --permission-mode acceptEdits)"
[[ "$(field "$OUT" permission_mode)" == "acceptEdits" ]] \
  && pass "AC3-ERR claude yolo + explicit --permission-mode -> explicit wins" \
  || fail "claude yolo explicit permission-mode: $OUT"

# x-b6e2: Tier-3 harness passthrough flags forward opaquely to the emit; the CLI
# maps or fails closed per provider (the skill never validates them).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude \
       --add-dir /work --agent reviewer --tools Read,Edit --deny-tools Bash)"
if [[ "$(field "$OUT" add_dir)" == "/work" ]] \
   && [[ "$(field "$OUT" agent)" == "reviewer" ]] \
   && [[ "$(field "$OUT" tools)" == "Read,Edit" ]] \
   && [[ "$(field "$OUT" deny_tools)" == "Bash" ]]; then
  pass "x-b6e2 tier-3 flags (--add-dir/--agent/--tools/--deny-tools) forward to emit"
else
  fail "x-b6e2 tier-3 forward: $OUT"
fi

# --- US4: payload modes (build / ask / passthrough) + provider-aware messages ---

# AC4-HP: ask mode (--ask) -> prompt VERBATIM, no /target, no no-merge, no brief.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider codex --ask \
       --input "what is the time complexity of bubble sort")"
if [[ "$(field "$OUT" payload_mode)" == "ask" ]] \
   && [[ "$(field "$OUT" message)" == "what is the time complexity of bubble sort" ]]; then
  pass "AC4-HP ask mode -> prompt verbatim (no /target, no no-merge)"
else
  fail "AC4-HP ask verbatim: $OUT"
fi

# sigma-review finding 4: an explicit `ask` verb WINS over leading-/ passthrough.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider codex --ask --input "/target what does this flag do")"
if [[ "$(field "$OUT" payload_mode)" == "ask" ]] \
   && [[ "$(field "$OUT" message)" == "/target what does this flag do" ]] \
   && [[ "$(field "$OUT" status)" == "ok" ]]; then
  pass "finding-4 ask verb beats leading-/ passthrough -> verbatim prompt, not refused"
else
  fail "finding-4 ask-with-slash: $OUT"
fi

# codex passthrough: a footnote slash command is NORMALIZED to the `$fno:` skill
# surface (codex exec expands it), not refused - `/goal` -> `$fno:goal` (x-a5e4).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider codex --input "/goal ship it")"
if [[ "$(field "$OUT" status)" == "ok" ]] && [[ "$(field "$OUT" message)" == '$fno:goal ship it' ]]; then
  pass "passthrough: codex /goal -> \$fno:goal (skill surface, not refused)"
else
  fail "codex passthrough normalize: $OUT"
fi

# A prose-surface provider (opencode) has NO slash/skill surface, so a slash
# passthrough is still refused (a literal /goal would run verbatim as a no-op).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider opencode --input "/goal ship it")"
if [[ "$(field "$OUT" status)" == "error" ]] && printf '%s' "$OUT" | grep -qi "no slash/skill surface"; then
  pass "passthrough: opencode /goal -> refused (no surface)"
else
  fail "opencode passthrough refuse: $OUT"
fi

# AC4-EDGE: normalize does NOT strip a trailing provider word from the prompt;
# the full text is the message (the trailing-word inference is skill-layer).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --ask --input "explain how codex resume works")"
if [[ "$(field "$OUT" message)" == "explain how codex resume works" ]] \
   && [[ "$(field "$OUT" provider)" == "claude" ]]; then
  pass "AC4-EDGE ask prompt kept whole; provider falls back (no in-prompt stripping)"
else
  fail "AC4-EDGE ask whole prompt: $OUT"
fi

# codex build: the native `$fno:target` skill invocation (runs the REAL
# pipeline), no-merge appended - NOT a prose brief (x-a5e4).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider codex --input "ab-deadbeef")"
MSG="$(field "$OUT" message)"
if [[ "$(field "$OUT" payload_mode)" == "build" ]] \
   && [[ "$MSG" == '$fno:target ab-deadbeef no-merge' ]]; then
  pass "build: codex node -> \$fno:target + no-merge (native skill)"
else
  fail "codex node build: $OUT"
fi

# agy is also a slash surface (like claude): native /target, not a prose brief.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider agy --input "ab-deadbeef")"
if [[ "$(field "$OUT" message)" == "/target ab-deadbeef no-merge" ]]; then
  pass "build: agy -> /target + no-merge (slash surface)"
else
  fail "agy build: $OUT"
fi

# A prose-surface provider (gemini/opencode) keeps the prose BRIEF, never a
# literal /target it would run as a no-op.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider gemini --input "add CSV export to the dashboard")"
MSG="$(field "$OUT" message)"
if [[ "$MSG" == *"Implement the following"* ]] && [[ "$MSG" == *"add CSV export to the dashboard"* ]] \
   && [[ "$MSG" == *"do not merge"* ]] && [[ "$MSG" != *"/target"* ]]; then
  pass "build: gemini feature -> prose brief, never /target"
else
  fail "gemini feature brief: $OUT"
fi

# --allow-merge drops the do-not-merge from a prose brief (gemini/opencode).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider gemini --input "add CSV export" --allow-merge)"
MSG="$(field "$OUT" message)"
if [[ "$MSG" == *"open a pull request"* ]] && [[ "$MSG" != *"do not merge"* ]]; then
  pass "build: gemini brief + --allow-merge -> no do-not-merge instruction"
else
  fail "gemini brief allow-merge: $OUT"
fi

# claude build is unchanged: /target wrap + no-merge (payload_mode=build).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "ab-deadbeef")"
if [[ "$(field "$OUT" payload_mode)" == "build" ]] \
   && [[ "$(field "$OUT" message)" == "/target ab-deadbeef no-merge" ]]; then
  pass "build: claude -> /target + no-merge (unchanged)"
else
  fail "claude build unchanged: $OUT"
fi

# claude passthrough that is a /target command still gets no-merge.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "/target ab-deadbeef")"
if [[ "$(field "$OUT" payload_mode)" == "passthrough" ]] \
   && [[ "$(field "$OUT" message)" == "/target ab-deadbeef no-merge" ]]; then
  pass "passthrough: claude /target -> no-merge injected"
else
  fail "claude /target passthrough no-merge: $OUT"
fi

# ===========================================================================
# US1 - Em-dash tolerance (ab-27541df5): a phone-mangled flag is canonicalized
# as an argv token, or fails loud when it survives in the task prose.
# ===========================================================================
EMDASH=$(printf '\342\200\224')   # U+2014 (iOS smart-punct for `--`)
ENDASH=$(printf '\342\200\223')   # U+2013

# AC1-HP: an em-dash flag as an argv token -> canonicalized -> yes=1, clean message.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude "${EMDASH}yes")"
if [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" yes)" == "1" ]] \
   && [[ "$(field "$OUT" message)" == "/target ab-deadbeef no-merge" ]] \
   && [[ "$(field "$OUT" message)" != *"yes"* ]]; then
  pass "AC1-HP em-dash argv flag -> --yes canonicalized (yes=1, no flag text in message)"
else
  fail "AC1-HP em-dash argv flag: $OUT"
fi

# en-dash variant also canonicalizes.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude "${ENDASH}allow-merge")"
if [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" message)" == "/target ab-deadbeef" ]]; then
  pass "AC1-HP en-dash argv flag -> --allow-merge canonicalized (no-merge dropped)"
else
  fail "AC1-HP en-dash argv flag: $OUT"
fi

# AC1-ERR: a mangled flag surviving in the task TEXT fails loud, names the token,
# suggests the shorthand. No spawn fields.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "fix the ${EMDASH}allow-merge handling")"
if [[ "$(field "$OUT" status)" == "error" ]] \
   && printf '%s' "$OUT" | grep -q "${EMDASH}allow-merge" \
   && printf '%s' "$OUT" | grep -q -- "-m"; then
  pass "AC1-ERR mangled flag in prose -> status=error, names token, suggests -m"
else
  fail "AC1-ERR mangled-flag prose: $OUT"
fi

# AC1-UI: the error message tells the operator how to re-send.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "${EMDASH}yes ship it")"
if [[ "$(field "$OUT" status)" == "error" ]] \
   && printf '%s' "$OUT" | grep -qiE "re-send|separate"; then
  pass "AC1-UI error is actionable (re-send / separate hint)"
else
  fail "AC1-UI actionable error: $OUT"
fi

# AC1-EDGE: an out-of-vocabulary dash token in prose passes through untouched.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "support -v verbose output in the CLI")"
if [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" message)" == "/target support -v verbose output in the CLI no-merge" ]]; then
  pass "AC1-EDGE out-of-vocabulary -v passes as prose, dispatch proceeds"
else
  fail "AC1-EDGE vocabulary scope: $OUT"
fi

# AC1-FR: a glued em-dash token (no surrounding whitespace) is prose, not a flag.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "add a tooltip${EMDASH}yes indicator")"
if [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" message)" == *"tooltip${EMDASH}yes"* ]]; then
  pass "AC1-FR glued em-dash is prose, original text preserved"
else
  fail "AC1-FR glued em-dash: $OUT"
fi

# ===========================================================================
# US2 - Shorthands (ab-27541df5): -y / -m / -n parse to canonical fields.
# ===========================================================================

# AC2-HP: -y -m -n <name> all parse.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude -y -m -n tgt-custom)"
if [[ "$(field "$OUT" yes)" == "1" ]] \
   && [[ "$(field "$OUT" name)" == "tgt-custom" ]] \
   && [[ "$(field "$OUT" message)" == "/target ab-deadbeef" ]]; then
  pass "AC2-HP -y/-m/-n parse (yes=1, name=tgt-custom, no-merge dropped by -m)"
else
  fail "AC2-HP shorthands: $OUT"
fi

# -y / --yes parity.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude --yes)"
[[ "$(field "$OUT" yes)" == "1" ]] && pass "US2 --yes long form -> yes=1" || fail "--yes long: $OUT"
# Default yes=0 when not passed.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude)"
[[ "$(field "$OUT" yes)" == "0" ]] && pass "US2 default -> yes=0" || fail "default yes: $OUT"
# -m / --allow-merge parity.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude -m)"
[[ "$(field "$OUT" message)" == "/target ab-deadbeef" ]] && pass "US2 -m -> allow-merge (no-merge dropped)" || fail "-m allow-merge: $OUT"
# -n / --name parity.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude -n 'My Worker!!')"
[[ "$(field "$OUT" name)" == "my-worker" ]] && pass "US2 -n -> sanitized name" || fail "-n name: $OUT"

# AC2-ERR: a bare trailing -n (no value) hits the empty-name error path.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" -n)"
[[ "$(field "$OUT" status)" == "error" ]] && pass "AC2-ERR bare -n -> status=error (empty-name path)" || fail "AC2-ERR bare -n: $OUT"

# AC2-EDGE: combined shorts (-ym) are unsupported -> unknown-argument error.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" -ym)"
if [[ "$(field "$OUT" status)" == "error" ]] \
   && printf '%s' "$OUT" | grep -qi "unknown argument"; then
  pass "AC2-EDGE combined shorts -ym -> unknown-argument error"
else
  fail "AC2-EDGE -ym: $OUT"
fi

# P2-1 (review): allow_merge is emitted as a first-class field so confirm-decision
# never re-derives merge state from message prose on the silent-launch path.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude -m)"
[[ "$(field "$OUT" allow_merge)" == "1" ]] && pass "P2-1 -m -> allow_merge=1 field emitted" || fail "allow_merge=1: $OUT"
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --input "ab-deadbeef" --provider claude)"
[[ "$(field "$OUT" allow_merge)" == "0" ]] && pass "P2-1 default -> allow_merge=0 field emitted" || fail "allow_merge=0: $OUT"

# P2-2c (review): ask payloads are exempt from the vocabulary scan - a flag token
# in a question is part of the prompt, not a mangled dispatch flag.
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider codex --ask --input "what does grep -i do")"
if [[ "$(field "$OUT" status)" == "ok" ]] \
   && [[ "$(field "$OUT" message)" == "what does grep -i do" ]]; then
  pass "P2-2c ask payload exempt from vocabulary scan (-i kept verbatim)"
else
  fail "P2-2c ask exemption: $OUT"
fi
# but a BUILD payload with the same token still fails loud (Locked Decision 5:
# a known-vocabulary flag token in build prose is status=error, not silent prose).
OUT="$(DISPATCH_PROVIDER_RESOLVER="$STUB_EMPTY" bash "$NORM" --provider claude --input "make grep -i case insensitive")"
[[ "$(field "$OUT" status)" == "error" ]] && pass "P2-2 build payload still fails loud on -i (Locked Decision 5)" || fail "build -i scan: $OUT"

echo ""
echo "test_agent_normalize: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
