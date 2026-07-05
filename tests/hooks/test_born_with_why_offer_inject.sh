#!/usr/bin/env bash
# Smoke test for hooks/born-with-why-offer-inject.sh (x-af8d, part 2).
#
# Verifies:
#   AC2-HP  : a fresh think_offered event -> <system-reminder> naming the node,
#             cursor advances past it.
#   AC2-ERR : same event on a later turn (cursor past it) -> silent (fires once).
#   AC2-EDGE: a malformed/truncated events line is skipped, hook exits 0 and
#             still surfaces a valid later offer.
#   Silent  : no events file, or only non-offer events -> no output, rc=0.
#   Wiring  : hooks.json registers the hook under UserPromptSubmit, valid JSON.
#
# Exit codes: 0 pass, 1 assertion failed, 77 skipped (missing deps).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/born-with-why-offer-inject.sh"
HOOKS_JSON="${REPO_ROOT}/hooks/hooks.json"

log()  { printf '[born-why-offer-hook] %s\n' "$*"; }
fail() { printf '[born-why-offer-hook] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[born-why-offer-hook] PASS: %s\n' "$*"; }
skip() { printf '[born-why-offer-hook] SKIP: %s\n' "$*" >&2; exit 77; }

command -v python3 &>/dev/null || skip "python3 not on PATH"
command -v git     &>/dev/null || skip "git not on PATH"

[[ -f "$HOOK" ]] || fail "hook not found at $HOOK"
[[ -x "$HOOK" ]] || fail "hook not executable at $HOOK"
bash -n "$HOOK" || fail "bash -n rejected $HOOK"

# Helper: extract the injected additionalContext (empty string if none).
extract_ctx() {
    python3 -c '
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    print(""); sys.exit(0)
try:
    print(json.loads(raw)["hookSpecificOutput"]["additionalContext"])
except Exception:
    print("")
'
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git -C "$WORK" init -q || fail "git init failed"
mkdir -p "$WORK/.fno"
EVENTS="$WORK/.fno/events.jsonl"
CURSOR="$WORK/.fno/.think-offer-cursor"
# offered_line carries an offer_line WITH the origin-transcript comment, exactly
# as spawn_think records it for a resolved offer.
offered_line()    { printf '{"ts":"%s","type":"think_offered","source":"backlog","data":{"node_id":"%s","offer_line":"/think %s  # origin transcript: /tmp/%s.jsonl"}}\n' "$1" "$2" "$2" "$2"; }
offered_no_line() { printf '{"ts":"%s","type":"think_offered","source":"backlog","data":{"node_id":"%s"}}\n' "$1" "$2"; }
other_line()      { printf '{"ts":"%s","type":"think_spawned","source":"backlog","data":{"node_id":"%s"}}\n' "$1" "$2"; }

# Stub `fno` so the resolve/in-progress guard's `fno backlog get <id>` is
# deterministic:
#   - id in $FNO_STUB_PHANTOM     -> exit 1 (unresolvable / phantom)
#   - id in $FNO_STUB_INPROGRESS  -> exit 0 + JSON with a PR + claimed status
#                                    (work already underway)
#   - otherwise                   -> exit 0 + empty stdout (resolves; the hook
#                                    treats an unparseable/empty body as
#                                    not-underway and surfaces -> fail safe,
#                                    which keeps the pre-existing scenarios below
#                                    unchanged)
# Prepended to PATH in run_hook so it shadows any real installed fno.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/fno" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-}" == "backlog" && "${2:-}" == "get" ]]; then
  for p in ${FNO_STUB_PHANTOM:-}; do [[ "${3:-}" == "$p" ]] && exit 1; done
  for w in ${FNO_STUB_INPROGRESS:-}; do
    [[ "${3:-}" == "$w" ]] && { printf '{"pr_number":207,"_status":"claimed"}\n'; exit 0; }
  done
fi
exit 0
STUB
chmod +x "$WORK/bin/fno"

# FNO_STUB_PHANTOM / FNO_STUB_INPROGRESS are read from the outer env per-test
# (both default empty -> every id resolves as a fresh, offerable node, so the
# pre-existing scenarios below are unaffected).
run_hook() { ( cd "$WORK" && PATH="$WORK/bin:$PATH" \
    FNO_STUB_PHANTOM="${FNO_STUB_PHANTOM:-}" \
    FNO_STUB_INPROGRESS="${FNO_STUB_INPROGRESS:-}" \
    bash "$HOOK" </dev/null ); }

# ── Silent: no events file ───────────────────────────────────────────
out="$(run_hook)" || fail "hook nonzero with no events file"
[[ -z "$out" ]] || fail "expected silence with no events file, got: $out"
pass "silent when events.jsonl absent"

# ── AC2-HP: a fresh offer surfaces once, cursor advances ─────────────
offered_line "2026-06-30T04:00:00Z" "x-aaaa1111" > "$EVENTS"
out="$(run_hook)" || fail "hook nonzero on fresh offer"
ctx="$(printf '%s' "$out" | extract_ctx)"
[[ "$ctx" == *"<system-reminder>"* ]] || fail "AC2-HP: no system-reminder emitted"
[[ "$ctx" == *"x-aaaa1111"* ]]        || fail "AC2-HP: reminder does not name the node"
# Surfaces the event's authoritative offer_line verbatim (incl. its comment),
# not a reconstructed bare `/think <id>` (codex P2 on PR #102).
[[ "$ctx" == *"/think x-aaaa1111  # origin transcript: /tmp/x-aaaa1111.jsonl"* ]] \
    || fail "AC2-HP: reminder did not surface the event's offer_line verbatim"
[[ -f "$CURSOR" ]] || fail "AC2-HP: cursor file not written"
exp="$(wc -c < "$EVENTS" | tr -d ' ')"
[[ "$(tr -d ' \n' < "$CURSOR")" == "$exp" ]] || fail "AC2-HP: cursor did not advance to EOF"
pass "AC2-HP: fresh offer surfaced with event offer_line, cursor advanced"

# ── AC2-ERR: same event again -> silent (fires once) ─────────────────
out="$(run_hook)" || fail "hook nonzero on second run"
[[ -z "$out" ]] || fail "AC2-ERR: offer re-surfaced on second turn: $out"
pass "AC2-ERR: consumed offer does not re-surface"

# ── AC2-EDGE: malformed line skipped, later valid offer still surfaces ─
printf '{this is not json\n' >> "$EVENTS"
offered_line "2026-06-30T05:00:00Z" "x-bbbb2222" >> "$EVENTS"
out="$(run_hook)" || fail "AC2-EDGE: hook nonzero on malformed line"
ctx="$(printf '%s' "$out" | extract_ctx)"
[[ "$ctx" == *"x-bbbb2222"* ]] || fail "AC2-EDGE: did not surface the valid later offer"
pass "AC2-EDGE: malformed line skipped, later offer surfaced, rc=0"

# ── Fallback: offer event without offer_line -> router-valid dispatch form ─
offered_no_line "2026-06-30T05:30:00Z" "x-dddd4444" >> "$EVENTS"
out="$(run_hook)" || fail "hook nonzero on offer without offer_line"
ctx="$(printf '%s' "$out" | extract_ctx)"
[[ "$ctx" == *"/think dispatch x-dddd4444"* ]] \
    || fail "fallback: missing offer_line did not fall back to /think dispatch <id>"
pass "fallback: offer_line absent -> router-valid /think dispatch <id>"

# ── Silent: only non-offer events in the new tail ────────────────────
other_line "2026-06-30T06:00:00Z" "x-cccc3333" >> "$EVENTS"
out="$(run_hook)" || fail "hook nonzero on non-offer tail"
[[ -z "$out" ]] || fail "expected silence for non-offer events, got: $out"
pass "silent when only non-offer events appended"

# ── Resolve-guard: a phantom offer (node no longer resolves) is suppressed ──
offered_line "2026-06-30T07:00:00Z" "ab-phantom9" >> "$EVENTS"
out="$(FNO_STUB_PHANTOM="ab-phantom9" run_hook)" || fail "hook nonzero on phantom offer"
[[ -z "$out" ]] || fail "resolve-guard: phantom offer surfaced (should be suppressed): $out"
pass "resolve-guard: phantom (unresolvable) offer suppressed"

# ── Resolve-guard: a real offer still surfaces (guard drops only phantoms) ──
offered_line "2026-06-30T07:30:00Z" "x-eeee5555" >> "$EVENTS"
out="$(run_hook)" || fail "hook nonzero on real offer after phantom"
ctx="$(printf '%s' "$out" | extract_ctx)"
[[ "$ctx" == *"x-eeee5555"* ]] || fail "resolve-guard: real offer after a phantom did not surface"
pass "resolve-guard: real offer still surfaces after a suppressed phantom"

# ── In-progress guard: an offer for a node already underway is suppressed ──
# (x-a83a) A claimed / PR-open node re-offering a born-with-why /think is the
# duplicate-session bug: the node resolves fine, but the work already started.
offered_line "2026-06-30T08:00:00Z" "x-ffff6666" >> "$EVENTS"
out="$(FNO_STUB_INPROGRESS="x-ffff6666" run_hook)" || fail "hook nonzero on in-progress offer"
[[ -z "$out" ]] || fail "in-progress guard: offer for a claimed/PR node surfaced (should be suppressed): $out"
pass "in-progress guard: offer for an already-underway node suppressed"

# ── In-progress guard: a just-born (fresh) node still surfaces ────────
# The guard must NOT over-suppress: a not-yet-started node is exactly the case
# born-with-why exists for. (Default stub = resolvable, not underway.)
offered_line "2026-06-30T08:30:00Z" "x-7777aaaa" >> "$EVENTS"
out="$(run_hook)" || fail "hook nonzero on fresh offer after in-progress"
ctx="$(printf '%s' "$out" | extract_ctx)"
[[ "$ctx" == *"x-7777aaaa"* ]] || fail "in-progress guard: a fresh node's offer was wrongly suppressed"
pass "in-progress guard: a just-born node still surfaces (no over-suppression)"

# ── Wiring: hooks.json registers the hook under UserPromptSubmit ──────
python3 -c "import json; json.load(open('$HOOKS_JSON'))" || fail "hooks.json failed JSON parse"
python3 - "$HOOKS_JSON" <<'PYEOF' || fail "hook not registered under UserPromptSubmit"
import json, sys
data = json.load(open(sys.argv[1]))
ups = data.get("hooks", {}).get("UserPromptSubmit", [])
hit = any(
    "born-with-why-offer-inject.sh" in h.get("command", "")
    for group in ups for h in group.get("hooks", [])
)
sys.exit(0 if hit else 1)
PYEOF
pass "hooks.json registers the hook under UserPromptSubmit"

log "all scenarios passed"
exit 0
