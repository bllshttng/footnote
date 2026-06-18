#!/usr/bin/env bash
# test_confirm.sh - free-lane confirm posture for /agents spawn (ab-994222ee US2).
#
# spawn is a FREE, reversible lane: it does NOT confirm by default. Only the
# cautious-operator opt-in `config.agents.confirm: always` re-introduces a
# confirm. Caveats (yolo / merge / exec-stall) become WARNINGS, not confirm-
# forcers. chat (billed) and stop (destructive) keep their own always-confirm
# gates and do NOT route through confirm-decision.sh. Self-contained: a stubbed
# posture reader, no real fno. Run:
#
#   bash skills/agent/tests/test_confirm.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEC="$HERE/../scripts/confirm-decision.sh"
TMP="$(mktemp -d -t agents-confirm.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
field() { printf '%s\n' "$1" | sed -n "s/^$2=//p" | head -1; }
mk_reader() { local f="$TMP/$1.sh"; printf '#!/usr/bin/env bash\n%s\n' "$2" > "$f"; chmod +x "$f"; echo "$f"; }
run() { DISPATCH_CONFIRM_READER="$1" bash "$DEC" "${@:2}"; }
ok()  { local l="$1" g="$2" w="$3"; if [[ "$g" == "$w" ]]; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); printf 'FAIL: %s (want %q got %q)\n' "$l" "$w" "$g"; fi; }

R_AUTO="$(mk_reader auto 'echo auto')"
R_ALWAYS="$(mk_reader always 'echo always')"
R_NEVER="$(mk_reader never 'echo never')"
R_FAIL="$(mk_reader fail 'exit 1')"
R_TYPO="$(mk_reader typo 'echo atuo')"

# --- AC2-HP: confirm unset/false (auto default) -> free lane does NOT confirm --
out="$(run "$R_AUTO" --node ab-deadbeef --provider claude --payload-mode build --mode exec)"
ok 'AC2-HP auto node build no confirm' "$(field "$out" confirm_required)" '0'
out="$(run "$R_AUTO" --provider claude --payload-mode build --mode exec)"
ok 'AC2-HP auto free-form no confirm (was confirm pre-ab-994222ee)' "$(field "$out" confirm_required)" '0'

# --- free lane: caveats DO NOT force a confirm; they surface as a warning ------
out="$(run "$R_AUTO" --node ab-deadbeef --provider gemini --payload-mode build --mode exec)"
ok 'auto gemini exec no confirm' "$(field "$out" confirm_required)" '0'
ok 'auto gemini exec caveat set' "$(field "$out" caveat)" '1'
if printf '%s' "$out" | grep -qi "caveat applies"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: gemini exec caveat not surfaced as warn: $out"; fi
out="$(run "$R_AUTO" --node ab-deadbeef --provider codex --payload-mode build --mode exec --yolo 1)"
ok 'auto yolo no confirm' "$(field "$out" confirm_required)" '0'
out="$(run "$R_AUTO" --node ab-deadbeef --provider claude --payload-mode build --mode exec --allow-merge 1)"
ok 'auto merge grant no confirm' "$(field "$out" confirm_required)" '0'

# --- AC2-EDGE: config.agents.confirm: always -> cautious opt-in confirms -------
out="$(run "$R_ALWAYS" --node ab-deadbeef --provider claude --payload-mode build --mode exec)"
ok 'AC2-EDGE always confirms even the free lane' "$(field "$out" confirm_required)" '1'
out="$(run "$R_ALWAYS" --provider claude --payload-mode build --mode exec)"
ok 'AC2-EDGE always confirms free-form too' "$(field "$out" confirm_required)" '1'

# --- AC2-FR: -y accepted and ignored (no error, no behavior change) ------------
out="$(run "$R_AUTO" --node ab-deadbeef --provider claude --payload-mode build --mode exec --yes 1)"
ok 'AC2-FR -y under auto still 0 (no behavior change)' "$(field "$out" confirm_required)" '0'
ok 'AC2-FR -y status ok (never errors)' "$(field "$out" reason | wc -l | tr -d ' ')" '1'
# -y also suppresses the opt-in confirm (explicit per-invocation intent).
out="$(run "$R_ALWAYS" --node ab-deadbeef --provider claude --payload-mode build --mode exec --yes 1)"
ok 'AC2-FR -y suppresses the always opt-in confirm' "$(field "$out" confirm_required)" '0'

# --- never posture: skip ------------------------------------------------------
out="$(run "$R_NEVER" --node ab-deadbeef --provider claude --payload-mode build --mode exec)"
ok 'never skips' "$(field "$out" confirm_required)" '0'

# --- degraded / invalid read -> no confirm (free lane) + staleness warn --------
out="$(run "$R_FAIL" --node ab-deadbeef --provider claude --payload-mode build --mode exec)"
ok 'degraded read -> no confirm (free lane, not "always")' "$(field "$out" confirm_required)" '0'
ok 'degraded posture is auto, not always' "$(field "$out" posture)" 'auto'
if printf '%s' "$out" | grep -qi "fno update"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: degraded missing staleness hint: $out"; fi
out="$(run "$R_TYPO" --node ab-deadbeef --provider claude --payload-mode build --mode exec)"
ok 'invalid enum (typo) -> no confirm (auto), never silently confirms' "$(field "$out" confirm_required)" '0'

# --- ask payload never confirms ------------------------------------------------
out="$(run "$R_ALWAYS" --provider codex --payload-mode ask --mode exec)"
ok 'ask never confirms even under always' "$(field "$out" confirm_required)" '0'

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
