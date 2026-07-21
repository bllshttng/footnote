#!/usr/bin/env bash
# test_normalize.sh - bash harness for the dashless bareword grammar (ab-994222ee).
#
# Verifies the deterministic trailing-run posture parse + dash-flag back-compat
# in skills/agent/scripts/normalize.sh. Self-contained: no pytest, no fno
# (provider resolution degrades to claude when fno is absent). Run:
#
#   bash skills/agent/tests/test_normalize.sh
#
# Exit 0 = all pass; non-zero = at least one failure (names printed).

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NORM="$HERE/../scripts/normalize.sh"

PASS=0
FAIL=0

# field <output> <key> -> prints the value for key=... (first match)
field() { printf '%s\n' "$1" | sed -n "s/^$2=//p" | head -1; }

# run <input> [extra argv...] -> echoes normalize.sh stdout
run() { bash "$NORM" --input "$1" "${@:2}"; }

# run_nofno shadows `fno` with an exit-1 stub so assertions about BUILTIN
# defaults (no-merge posture, static fallback tables) hold regardless of the
# host's installed fno and its config (e.g. dispatch.auto_merge=true would
# otherwise flip allow_merge via the x-4391 rung-2 read).
_de43_stub="$(mktemp -d)"
# Guard the mktemp: an empty _de43_stub would write the stub to /fno.
[[ -n "$_de43_stub" && -d "$_de43_stub" ]] || { echo "mktemp -d failed" >&2; exit 1; }
printf '#!/usr/bin/env bash\nexit 1\n' > "$_de43_stub/fno"; chmod +x "$_de43_stub/fno"
trap 'rm -rf "$_de43_stub"' EXIT
run_nofno() { PATH="$_de43_stub:$PATH" bash "$NORM" --input "$1" "${@:2}"; }

# run_guarded caps a run at 5s when a timeout binary exists (coreutils `timeout`,
# or `gtimeout` on macOS), so a regressed $#-guard that spins is caught, not hung.
# Where neither exists (bare macOS), it runs unguarded - the status/value asserts
# still prove correctness, and CI (Linux, has `timeout`) enforces the no-hang.
TIMEOUT_BIN="$(command -v timeout 2>/dev/null || command -v gtimeout 2>/dev/null || true)"
run_guarded() {
  if [[ -n "$TIMEOUT_BIN" ]]; then "$TIMEOUT_BIN" 5 bash "$NORM" --input "$1" "${@:2}"
  else bash "$NORM" --input "$1" "${@:2}"; fi
}

check_eq() {
  local label="$1" got="$2" want="$3"
  if [[ "$got" == "$want" ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want: %q\n  got:  %q\n' "$label" "$want" "$got"
  fi
}

check_contains() {
  local label="$1" hay="$2" needle="$3"
  if [[ "$hay" == *"$needle"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  expected to contain: %q\n  in: %q\n' "$label" "$needle" "$hay"
  fi
}

check_not_contains() {
  local label="$1" hay="$2" needle="$3"
  if [[ "$hay" != *"$needle"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  expected NOT to contain: %q\n  in: %q\n' "$label" "$needle" "$hay"
  fi
}

# --- AC1-HP: full dashless spawn ---------------------------------------------
out="$(run 'ab-1234abcd codex yolo as billing-worker merge')"
check_eq   'AC1-HP status'   "$(field "$out" status)"   'ok'
check_eq   'AC1-HP node'     "$(field "$out" node)"     'ab-1234abcd'
check_eq   'AC1-HP provider' "$(field "$out" provider)" 'codex'
check_eq   'AC1-HP yolo'     "$(field "$out" yolo)"     '1'
check_eq   'AC1-HP name'     "$(field "$out" name)"     'billing-worker'
check_eq   'AC1-HP merge'    "$(field "$out" allow_merge)" '1'

# --- AC1-ERR: empty task ------------------------------------------------------
out="$(run '')"
check_eq   'AC1-ERR empty status' "$(field "$out" status)" 'error'

# --- AC1-ERR: bareword-only payload (no task) collapses to empty -> error -----
out="$(run 'codex yolo merge')"
check_eq   'AC1-ERR bareword-only status' "$(field "$out" status)" 'error'

# --- AC1-EDGE: posture word mid-task stays task text --------------------------
out="$(run_nofno 'spawn the node that will merge two branches')"
check_eq   'AC1-EDGE merge not consumed' "$(field "$out" allow_merge)" '0'
check_contains 'AC1-EDGE merge stays in message' "$(field "$out" message)" 'merge two branches'

# --- x-2c27: bg / headless substrate posture words ----------------------------
out="$(run 'build the thing bg')"
check_eq   'x-2c27 bg substrate'       "$(field "$out" substrate)" 'bg'
check_contains 'x-2c27 bg task trimmed' "$(field "$out" message)"  'build the thing'
out="$(run 'fix it headless codex')"
check_eq   'x-2c27 headless substrate' "$(field "$out" substrate)" 'headless'
check_eq   'x-2c27 headless+codex provider' "$(field "$out" provider)" 'codex'
out="$(run 'just build the feature')"
check_eq   'x-2c27 no posture -> empty substrate (pane default)' "$(field "$out" substrate)" ''
# A mid-task 'bg' stays task text (right-anchored run; a non-posture token follows)
out="$(run 'make the bg job run faster')"
check_eq   'x-2c27 mid-task bg not consumed' "$(field "$out" substrate)" ''
check_contains 'x-2c27 mid-task bg stays in message' "$(field "$out" message)" 'bg job run faster'

# --- x-ffc3: a LEADING posture word + /command is refused, not buried in a seed -
# Posture words (bg|headless) are TRAILING only. A leading one whose remainder is
# a /command passthrough (the documented repro `bg /goal ...`) means the user
# mis-ordered the substrate; left alone it falls through to the seed default and
# the /command is buried in a verbatim seed instead of dispatched. Refuse with
# the corrective trailing form. Scoped to a /-led remainder so genuine feature
# prose beginning with the word still seeds (codex PR #106 P2).
out="$(run 'bg /goal x-ead3 residual')"
check_eq       'x-ffc3 leading bg+/ -> error'     "$(field "$out" status)"  'error'
check_contains 'x-ffc3 leading bg cites trailing form' "$(field "$out" error)" '/goal x-ead3 residual bg'
# Refusal emits no message= line at all, so nothing is /target-wrapped or launched.
check_eq       'x-ffc3 leading bg emits no message wrap' "$(field "$out" message)" ''
out="$(run 'headless /target billing migration')"
check_eq       'x-ffc3 leading headless+/ -> error' "$(field "$out" status)" 'error'
# Case-insensitive, matching the trailing parser's tr-lowercasing (mobile caps).
out="$(run 'BG /goal x-ead3 residual')"
check_eq       'x-ffc3 leading BG (caps) -> error' "$(field "$out" status)" 'error'
check_contains 'x-ffc3 leading BG hint uses lowercase bg' "$(field "$out" error)" 'residual bg'
# codex PR #106 P2: a leading posture word in genuine FEATURE prose (no /command
# remainder) is NOT refused - it builds, as it did before the guard.
out="$(run 'headless browser screenshots')"
check_eq       'x-ffc3 headless feature prose seeds' "$(field "$out" status)" 'ok'
check_contains 'x-ffc3 headless feature prose verbatim' "$(field "$out" message)" 'headless browser screenshots'
out="$(run 'bg worker cleanup')"
check_eq       'x-ffc3 bg feature prose seeds'    "$(field "$out" status)" 'ok'
# AC1-EDGE: a trailing posture word is unchanged (the guard fires on LEADING only)
out="$(run '/goal x-ead3 residual bg')"
check_eq       'x-ffc3 trailing bg substrate'      "$(field "$out" substrate)" 'bg'
check_contains 'x-ffc3 trailing bg message intact' "$(field "$out" message)" '/goal x-ead3 residual'
check_eq       'x-ffc3 trailing bg status ok'      "$(field "$out" status)" 'ok'
# AC1-ERR: exact-token match -> a feature whose first word merely CONTAINS the
# posture word (bgcolor / background) is not a false positive.
out="$(run 'bgcolor picker for the grid')"
check_eq       'x-ffc3 bgcolor not a false positive' "$(field "$out" status)" 'ok'
out="$(run 'background sync worker')"
check_eq       'x-ffc3 background not a false positive' "$(field "$out" status)" 'ok'

# trailing `merge` after a non-posture token still binds (right-anchored run)
out="$(run 'ab-22222222 merge')"
check_eq   'trailing merge binds' "$(field "$out" allow_merge)" '1'
check_eq   'trailing merge node'  "$(field "$out" node)" 'ab-22222222'

# --- provider bareword alone --------------------------------------------------
# A trailing gemini bareword is parsed as the provider and stripped from the
# message. Free text is now a verbatim seed (x-cbb0), which needs no /target
# skill surface, so gemini is NOT refused here - it seeds a gemini pane. (A node
# build on gemini IS refused; see the x-de43 block below.)
out="$(run 'add a login form gemini')"
check_eq           'gemini bareword -> provider gemini' "$(field "$out" provider)"     'gemini'
check_eq           'gemini bareword seed ok'            "$(field "$out" status)"       'ok'
check_eq           'gemini bareword seed mode'          "$(field "$out" payload_mode)" 'seed'
check_not_contains 'gemini bareword stripped'          "$(field "$out" message)"      'gemini'

# --- interactive/drive bareword ----------------------------------------------
out="$(run 'ab-33333333 codex drive')"
check_eq   'drive -> interactive mode' "$(field "$out" mode)" 'interactive'

# --- as <name> with nothing after = error ------------------------------------
out="$(run 'ab-44444444 as')"
check_eq   'dangling as is error' "$(field "$out" status)" 'error'

# --- `as <reserved-word>` binds the word as a NAME, not as the posture field --
out="$(run 'ab-99999999 as gemini')"
check_eq   'as gemini -> name=gemini'        "$(field "$out" name)" 'gemini'
check_eq   'as gemini -> provider NOT gemini' "$(field "$out" provider)" 'claude'
out="$(run_nofno 'ab-99999999 as merge')"
check_eq   'as merge -> name=merge'           "$(field "$out" name)" 'merge'
check_eq   'as merge -> allow_merge stays 0'  "$(field "$out" allow_merge)" '0'
# a posture word BEFORE the `as <name>` pair still binds normally
out="$(run 'ab-99999999 codex as worker')"
check_eq   'codex as worker -> provider codex' "$(field "$out" provider)" 'codex'
check_eq   'codex as worker -> name worker'    "$(field "$out" name)" 'worker'

# --- provider barewords beyond codex|gemini (data-driven from VALID_PROVIDERS) -
out="$(run 'ab-99999999 opencode')"
check_eq   'opencode -> provider=opencode'     "$(field "$out" provider)" 'opencode'
out="$(run 'ab-99999999 agy')"
check_eq   'agy -> provider=agy'               "$(field "$out" provider)" 'agy'
# an unsupported harness bareword stays task text (not a provider)
out="$(run 'ab-99999999 hermes')"
check_eq   'hermes not a spawn provider'       "$(field "$out" provider)" 'claude'
check_contains 'hermes stays in message'       "$(field "$out" message)" 'hermes'

# --- x-de43: per-harness native invocation (opencode /fno:verb, gemini refused)
# run_nofno (defined at top) forces the STATIC fallback table so the rendered
# surface is asserted against the in-tree mirror, not the installed fno (which
# may lag until redeployed). This is the mirror the parity python test guards.

# AC1-HP: an opencode node-id build renders the plugin-namespaced /fno:target
# (+ no-merge). Free text no longer builds (x-cbb0) - only a resolved node id
# wraps to a per-harness /target, so the build render is exercised via a node id.
out="$(run_nofno 'ab-12345678' --provider opencode)"
check_eq           'opencode build status'       "$(field "$out" status)"  'ok'
check_eq           'opencode build payload_mode' "$(field "$out" payload_mode)" 'build'
check_contains     'opencode build /fno:target'  "$(field "$out" message)" '/fno:target ab-12345678'
check_contains     'opencode build no-merge'     "$(field "$out" message)" 'no-merge'
check_not_contains 'opencode build no prose'     "$(field "$out" message)" 'Implement'

# AC4-EDGE: passthrough renders ANY verb via the single prefix-swap (no allowlist)
out="$(run_nofno '/blueprint quick doc.md' --provider opencode)"
check_eq   'opencode passthrough status'  "$(field "$out" status)"  'ok'
check_eq   'opencode passthrough /fno:'   "$(field "$out" message)" '/fno:blueprint quick doc.md'
out="$(run_nofno '/zzz args' --provider opencode)"
check_eq   'opencode arbitrary verb'      "$(field "$out" message)" '/fno:zzz args'
# idempotent: an already-namespaced command (copied from the palette) is not double-prefixed
out="$(run_nofno '/fno:blueprint quick doc.md' --provider opencode)"
check_eq   'opencode passthrough idempotent' "$(field "$out" message)" '/fno:blueprint quick doc.md'

# claude passthrough unchanged (empty prefix) - regression guard for parity
out="$(run_nofno '/target ship it' --provider claude)"
check_contains     'claude passthrough verbatim' "$(field "$out" message)" '/target ship it'
check_not_contains 'claude no fno: prefix'       "$(field "$out" message)" '/fno:'

# AC2-ERR: a deprecated gemini node-id build AND passthrough refuse loudly,
# naming agy (a node build needs the /target skill surface gemini lacks).
out="$(run_nofno 'ab-12345678' --provider gemini)"
check_eq       'gemini build refused'         "$(field "$out" status)" 'error'
check_contains 'gemini build names agy'       "$(field "$out" error)"  'agy'
out="$(run_nofno '/target ship it' --provider gemini)"
check_eq       'gemini passthrough refused'   "$(field "$out" status)" 'error'
check_contains 'gemini passthrough names agy' "$(field "$out" error)"  'agy'

# --- model <name> two-word posture -------------------------------------------
out="$(run 'ab-99999999 model opus')"
check_eq   'model opus -> model=opus'          "$(field "$out" model)" 'opus'
check_contains     'model opus keeps node in message' "$(field "$out" message)" 'ab-99999999'
check_not_contains 'model opus stripped from message' "$(field "$out" message)" 'opus'
out="$(run 'ab-99999999 codex model gpt-5')"
check_eq   'codex model gpt-5 -> model=gpt-5'   "$(field "$out" model)" 'gpt-5'
check_eq   'codex model gpt-5 -> provider codex' "$(field "$out" provider)" 'codex'
# a model name that is a bareword after the model keyword is the value, not task text
out="$(run 'ab-99999999 model sonnet as worker')"
check_eq   'model sonnet as worker -> model'    "$(field "$out" model)" 'sonnet'
check_eq   'model sonnet as worker -> name'     "$(field "$out" name)" 'worker'
# dash-flag --model wins (idempotent fill), no short -m overload
out="$(run 'ab-99999999' --model haiku)"
check_eq   '--model haiku -> model=haiku'       "$(field "$out" model)" 'haiku'
out="$(run 'ab-99999999' -m)"
check_eq   '-m is allow-merge not model'        "$(field "$out" allow_merge)" '1'
check_eq   '-m leaves model empty'              "$(field "$out" model)" ''
# dangling `model` with nothing after = error
out="$(run 'ab-44444444 model')"
check_eq   'dangling model is error'            "$(field "$out" status)" 'error'
# an inline --model dash-flag surviving in BUILD task text fails loud (defensive
# flag-vocabulary scan), not a silent drop of the override
out="$(run 'ab-99999999 --model opus')"
check_eq   'inline --model in task text is error' "$(field "$out" status)" 'error'
# mid-task `model` stays task text (right-anchored run)
out="$(run 'model the user data carefully')"
check_contains 'mid-task model stays in message' "$(field "$out" message)" 'model the user data carefully'
check_eq   'mid-task model leaves model empty'  "$(field "$out" model)" ''

# --- x-019d: permission-mode / role / timeout / fresh / here dash-flags -------
# Front-half (permission-mode/fresh/here) + two-layer (role/timeout) flags that
# normalize.sh must accept and round-trip to spawn.sh. Value flags use the
# $#-guarded idiom, so a bare trailing flag must stay status=ok and not hang.
out="$(run 'ab-99999999' --permission-mode bypassPermissions)"
check_eq '--permission-mode -> permission_mode=bypassPermissions' "$(field "$out" permission_mode)" 'bypassPermissions'
check_eq '--permission-mode -> status ok'   "$(field "$out" status)" 'ok'
out="$(run 'ab-99999999' --role coordinate)"
check_eq '--role coordinate -> role=coordinate' "$(field "$out" role)" 'coordinate'
out="$(run 'ab-99999999' -r coordinate)"
check_eq '-r coordinate -> role=coordinate' "$(field "$out" role)" 'coordinate'
out="$(run 'ab-99999999' --timeout 900)"
check_eq '--timeout 900 -> timeout=900'      "$(field "$out" timeout)" '900'
out="$(run 'ab-99999999' -t 900)"
check_eq '-t 900 -> timeout=900'             "$(field "$out" timeout)" '900'
out="$(run 'ab-99999999' --fresh)"
check_eq '--fresh -> fresh=1'                "$(field "$out" fresh)" '1'
out="$(run 'ab-99999999' --here)"
check_eq '--here -> here=1'                   "$(field "$out" here)" '1'
out="$(run 'ab-99999999' --in-place)"
check_eq '--in-place -> here=1'               "$(field "$out" here)" '1'
# bare trailing value flag (no value): $#-guard must keep status=ok, no hang,
# empty value. `timeout 5` fails the assertion loudly if the parse spins.
out="$(run_guarded 'ab-99999999' --permission-mode)"
check_eq 'bare --permission-mode -> status ok'  "$(field "$out" status)" 'ok'
check_eq 'bare --permission-mode -> empty value' "$(field "$out" permission_mode)" ''
out="$(run_guarded 'ab-99999999' --role)"
check_eq 'bare --role -> status ok'          "$(field "$out" status)" 'ok'
out="$(run_guarded 'ab-99999999' --timeout)"
check_eq 'bare --timeout -> status ok'       "$(field "$out" status)" 'ok'
# inline in BUILD task text fails loud (defensive flag-vocabulary scan)
out="$(run 'ab-99999999 --permission-mode bypassPermissions')"
check_eq 'inline --permission-mode in task text is error' "$(field "$out" status)" 'error'
out="$(run 'ab-99999999 --role coordinate')"
check_eq 'inline --role in task text is error'  "$(field "$out" status)" 'error'
out="$(run 'ab-99999999 --fresh the thing')"
check_eq 'inline --fresh in task text is error' "$(field "$out" status)" 'error'

# --- effort <value> two-word posture -----------------------------------------
out="$(run 'ab-99999999 codex effort high')"
check_eq   'effort high -> effort=high'          "$(field "$out" effort)" 'high'
check_eq   'effort keeps provider posture'       "$(field "$out" provider)" 'codex'
check_not_contains 'effort stripped from message' "$(field "$out" message)" 'effort high'
out="$(run 'ab-99999999 effort medium' --effort low)"
check_eq   '--effort wins over dashless effort'  "$(field "$out" effort)" 'low'
out="$(run 'ab-44444444 effort')"
check_eq   'dangling effort is error'            "$(field "$out" status)" 'error'
out="$(run 'ab-44444444' --effort)"
check_eq   'bare --effort is error'               "$(field "$out" status)" 'error'
out="$(run 'ab-44444444' --effort '')"
check_eq   'empty --effort is error'              "$(field "$out" status)" 'error'
out="$(run 'tune effort carefully for this worker')"
check_contains 'mid-task effort stays in message' "$(field "$out" message)" 'tune effort carefully for this worker'
check_eq   'mid-task effort leaves effort empty' "$(field "$out" effort)" ''

# --- x-d235: --yolo on claude maps to --permission-mode bypassPermissions -----
# claude has no --yolo flag; its full-auto/no-gates equivalent is
# bypassPermissions. Map it (don't drop it) so a yolo'd claude bg worker runs
# gate-free. An explicit --permission-mode wins; codex/gemini yolo is unchanged.
out="$(run 'ab-99999999' --provider claude --yolo)"
check_eq 'claude yolo -> permission_mode=bypassPermissions' "$(field "$out" permission_mode)" 'bypassPermissions'
check_eq 'claude yolo -> yolo cleared'                      "$(field "$out" yolo)" '0'
out="$(run 'ab-99999999' --provider claude --yolo --permission-mode acceptEdits)"
check_eq 'claude yolo + explicit permission-mode -> explicit wins' "$(field "$out" permission_mode)" 'acceptEdits'
check_eq 'claude yolo + explicit -> yolo still cleared'           "$(field "$out" yolo)" '0'
out="$(run 'ab-99999999' --provider codex --yolo)"
check_eq 'codex yolo -> yolo stays 1'                      "$(field "$out" yolo)" '1'
check_eq 'codex yolo -> no permission_mode injected'       "$(field "$out" permission_mode)" ''
out="$(run 'ab-99999999' --provider claude)"
check_eq 'claude no-yolo -> permission_mode stays empty'   "$(field "$out" permission_mode)" ''

# --- -Y is a --yolo alias (flag, trailing-run, and guard semantics) -----------
# The fno CLI spawn/ask verbs already take -Y for --yolo; the skill layer must
# match or the flag silently degrades (a real spawn launched permission-manual
# because -Y was coerced to -y upstream). Trailing -Y matches the RAW token:
# lowercased it would collide with -y (--yes), which must keep refusing loud.
out="$(run '/think x-1234 model fable bg' -Y --provider claude)"
check_eq '-Y flag status'                "$(field "$out" status)" 'ok'
check_eq '-Y flag claude -> permission_mode' "$(field "$out" permission_mode)" 'bypassPermissions'
out="$(run '/think x-1234 -Y model fable bg' --provider claude)"
check_eq '-Y trailing status'            "$(field "$out" status)" 'ok'
check_eq '-Y trailing consumed from msg' "$(field "$out" message)" '/think x-1234'
check_eq '-Y trailing claude -> permission_mode' "$(field "$out" permission_mode)" 'bypassPermissions'
out="$(run 'ab-1234abcd codex -Y merge')"
check_eq '-Y trailing codex yolo'        "$(field "$out" yolo)" '1'
out="$(run '/fix the -Y handling then ship')"
check_eq 'mid-text -Y in /command -> guard error' "$(field "$out" status)" 'error'
out="$(run '/think x-1234 -y model fable bg')"
check_eq 'trailing lowercase -y stays guarded (never yolo)' "$(field "$out" status)" 'error'

# --- mid-task `as` stays task text -------------------------------------------
out="$(run 'refactor the module as a plugin')"
check_eq   'mid-task as keeps default name' "$(field "$out" name)" "$(field "$(run 'refactor the module as a plugin')" name)"
check_contains 'mid-task as stays in message' "$(field "$out" message)" 'as a plugin'

# --- gemini CRITICAL: a multi-line task whose first line ends in a posture ----
#     word must NOT lose lines 2+ (the read/rebuild must span all lines) -------
# `message` is the LAST emitted field, so everything after `message=` is its
# (possibly multi-line) value.
msg_block() { printf '%s\n' "$1" | sed -n '/^message=/,$p' | sed '1s/^message=//'; }
ml="$(printf 'fix the login flow\nand the signup path codex')"
out="$(run "$ml")"
check_eq       'multiline provider parsed from trailing posture' "$(field "$out" provider)" 'codex'
check_contains 'multiline line1 preserved' "$(msg_block "$out")" 'fix the login flow'
check_contains 'multiline line2 NOT discarded' "$(msg_block "$out")" 'and the signup path'
check_not_contains 'multiline trailing posture stripped from message' "$(msg_block "$out")" 'codex'
# mid-text posture across lines stays task text (right-anchored run)
ml2="$(printf 'refactor the merge module\ninto two files')"
out="$(run_nofno "$ml2")"
check_eq       'multiline mid-merge not consumed' "$(field "$out" allow_merge)" '0'
check_contains 'multiline mid-text keeps both lines' "$(msg_block "$out")" 'into two files'

# --- AC4-HP: desktop dash-flags canonicalize to the same fields ---------------
dash="$(run 'ab-55555555' --yolo -n worker -m)"
bare="$(run 'ab-55555555 yolo as worker merge')"
check_eq 'AC4-HP dash yolo'  "$(field "$dash" yolo)"        "$(field "$bare" yolo)"
check_eq 'AC4-HP dash name'  "$(field "$dash" name)"        "$(field "$bare" name)"
check_eq 'AC4-HP dash merge' "$(field "$dash" allow_merge)" "$(field "$bare" allow_merge)"

# --- AC4-FR: bareword + dash-flag for same field is idempotent ----------------
# (codex target so yolo is not dropped the way it is for a claude target)
out="$(run 'ab-66666666 codex yolo' --yolo)"
check_eq 'AC4-FR yolo idempotent status' "$(field "$out" status)" 'ok'
check_eq 'AC4-FR yolo idempotent value'  "$(field "$out" yolo)" '1'

# --- AC2-FR (back-compat): -y/--yes accepted, never an error ------------------
out="$(run 'ab-77777777' -y)"
check_eq 'AC2-FR -y accepted' "$(field "$out" status)" 'ok'

# --- AC4-ERR: a genuinely unknown dash-flag fails loud ------------------------
out="$(run 'ab-88888888' --frobnicate)"
check_eq 'AC4-ERR unknown dash-flag is error' "$(field "$out" status)" 'error'

# ===========================================================================
# Node slugs + id-free entry modes (ab-f82e8083)
# ===========================================================================

# --- bare hex rides tier 2, never a guessed prefix -------------------
# Tier 3 used to re-prefix a bare 8-hex token to `ab-`, which minted ids that
# need not exist and was wrong outright in a repo with a configured prefix. The
# resolver accepts bare hex directly, so classification hands it over as a query.
out="$(run '1234abcd')"
check_eq   'bare hex status'      "$(field "$out" status)"     'ok'
check_eq   'bare hex not a node'  "$(field "$out" node)"       ''
check_eq   'bare hex is a query'  "$(field "$out" node_query)" '1234abcd'
check_not_contains 'bare hex is not re-prefixed to ab-' "$out" 'ab-1234abcd'

# Over-long hex is not an id either (the shape caps hex at 8), so it reaches the
# resolver as a query rather than a malformed id.
out="$(run '1234abcdef')"
check_eq   '10-hex not a node'        "$(field "$out" node)" ''
check_eq   '10-hex is a slug-candidate' "$(field "$out" node_query)" '1234abcdef'

# --- tier 2 (AC1-HP): a single slug-shaped token is a slug candidate -----------
out="$(run 'dashless-spawn')"
check_eq   'tier2 slug status'      "$(field "$out" status)"     'ok'
check_eq   'tier2 slug node empty'  "$(field "$out" node)"       ''
check_eq   'tier2 slug node_query'  "$(field "$out" node_query)" 'dashless-spawn'
check_eq   'tier2 slug not next'    "$(field "$out" spawn_next)" '0'

# mobile auto-capitalization: a capitalized first letter is still a slug
# candidate (the resolver matches slugs case-insensitively) (gemini review)
out="$(run 'Dashless-spawn')"
check_eq   'tier2 capitalized slug is a candidate' "$(field "$out" node_query)" 'Dashless-spawn'
check_eq   'tier2 capitalized slug not next'       "$(field "$out" spawn_next)" '0'

# a multi-word description is NOT a slug candidate (it is describe-it, tier 4)
out="$(run 'the one about iOS autocorrect')"
check_eq   'tier4 describe node empty'       "$(field "$out" node)"       ''
check_eq   'tier4 describe node_query empty'  "$(field "$out" node_query)" ''
check_eq   'tier4 describe not next'          "$(field "$out" spawn_next)" '0'

# --- tier 5 (AC3-HP/EDGE): next / next all ------------------------------------
out="$(run 'next')"
check_eq   'tier5 next status'      "$(field "$out" status)"     'ok'
check_eq   'tier5 next spawn_next'  "$(field "$out" spawn_next)" '1'
check_eq   'tier5 next scope'       "$(field "$out" next_scope)" 'project'
check_eq   'tier5 next node empty'  "$(field "$out" node)"       ''

out="$(run 'next all')"
check_eq   'tier5 next-all spawn_next' "$(field "$out" spawn_next)" '1'
check_eq   'tier5 next-all scope'      "$(field "$out" next_scope)" 'all'

# `next` as part of a real task is NOT the next-pointer (only the bare word is)
out="$(run 'add a next button to the form')"
check_eq   'tier5 next-in-task not spawn_next' "$(field "$out" spawn_next)" '0'

# --- tier 1 still wins + new fields emitted for an ab-id ----------------------
out="$(run 'ab-1234abcd')"
check_eq   'tier1 exact node'        "$(field "$out" node)"       'ab-1234abcd'
check_eq   'tier1 exact node_query'  "$(field "$out" node_query)" ''
check_eq   'tier1 exact spawn_next'  "$(field "$out" spawn_next)" '0'

# ===========================================================================
# Payload modes: seed (free text, verbatim) + handoff verb (x-cbb0)
# ===========================================================================
# spawn means start a session with what you pass, nothing more (x-cbb0). Free
# text is a verbatim SEED, no longer implicitly /target-wrapped; the surviving
# implicit /target is a resolved node id (build), config not shape inference.

# --- free text is a verbatim SEED: no /target wrap, no no-merge ---------------
out="$(run 'add a dark-mode toggle to settings')"
check_eq           'seed status'           "$(field "$out" status)"       'ok'
check_eq           'seed payload_mode'     "$(field "$out" payload_mode)" 'seed'
check_eq           'seed message verbatim' "$(field "$out" message)"      'add a dark-mode toggle to settings'
check_eq           'seed node empty'       "$(field "$out" node)"         ''
check_not_contains 'seed no /target'       "$(field "$out" message)"      '/target'
check_not_contains 'seed no no-merge'      "$(field "$out" message)"      'no-merge'

# --- the deliberate semantics flip: "fix the X" seeds, does NOT build ---------
out="$(run 'fix the login bug')"
check_eq 'seed fix-verb mode'     "$(field "$out" payload_mode)" 'seed'
check_eq 'seed fix-verb verbatim' "$(field "$out" message)"      'fix the login bug'

# --- a seed keeps trailing posture parsing (launch axes are orthogonal) -------
out="$(run 'talk through the retry design codex')"
check_eq           'seed trailing provider' "$(field "$out" provider)"     'codex'
check_eq           'seed trailing mode'     "$(field "$out" payload_mode)" 'seed'
check_not_contains 'seed provider stripped' "$(field "$out" message)"      'codex'

# --- a former "path"/"question"/"continue" phrasing is now just a seed ---------
# (the deterministic shape classifier + the bare-input build-wrap announce it fed
# are gone; there is no wrap to warn about, so these flow straight to a verbatim
# seed like any other free text)
out="$(run 'should we cache the provider lookup?')"
check_eq 'question-shaped -> seed'   "$(field "$out" payload_mode)" 'seed'
out="$(run 'continue the quarterly outreach work')"
check_eq 'continue-shaped -> seed'   "$(field "$out" payload_mode)" 'seed'

# --- a slash COMMAND is a passthrough, not a seed -----------------------------
out="$(run '/pr check 42')"
check_eq 'slash command is passthrough' "$(field "$out" payload_mode)" 'passthrough'

# --- a resolved node-id is a build (the ONE surviving implicit /target) -------
out="$(run 'ab-1234abcd')"
check_eq 'node-id builds not seeds' "$(field "$out" payload_mode)" 'build'

# --- handoff mode: doc path -> continuation seed, no /target ------------------
out="$(run '/Users/me/handoff-2026.md' --handoff)"
check_eq           'handoff status'         "$(field "$out" status)"       'ok'
check_eq           'handoff default provider' "$(field "$out" provider)"   'claude'
check_eq           'handoff payload_mode'   "$(field "$out" payload_mode)" 'handoff'
check_eq           'handoff node empty'     "$(field "$out" node)"         ''
check_contains     'handoff seed has path'  "$(msg_block "$out")"          '/Users/me/handoff-2026.md'
check_contains     'handoff seed continues' "$(msg_block "$out")"          'Continue the work'
check_contains     'handoff seed guardrail' "$(msg_block "$out")"          'GUARDRAIL'
check_not_contains 'handoff no /target'     "$(msg_block "$out")"          '/target'
check_not_contains 'handoff no no-merge tok' "$(msg_block "$out")"         'no-merge'

# --- handoff path with leading / is NOT passthrough --------------------------
out="$(run '/abs/path/doc.md' --handoff)"
check_eq 'handoff beats passthrough' "$(field "$out" payload_mode)" 'handoff'

# --- handoff name derives from the doc BASENAME, not the full path ------------
out="$(run '/home/user/jobs/project-handoff-2026.md' --handoff)"
check_contains 'handoff name from basename'      "$(field "$out" name)" 'project-handoff'
check_not_contains 'handoff name drops path dirs' "$(field "$out" name)" 'jobs'

# --- handoff relative path works ---------------------------------------------
out="$(run 'docs/handoff.md' --handoff)"
check_eq       'handoff relative payload'      "$(field "$out" payload_mode)" 'handoff'
check_contains 'handoff relative path in seed' "$(msg_block "$out")"          'docs/handoff.md'

# --- handoff empty path -> error ---------------------------------------------
out="$(run '' --handoff)"
check_eq 'handoff empty path error' "$(field "$out" status)" 'error'

# --- explicit codex/gemini handoffs keep the provider-neutral seed -----------
out="$(run '/x/doc.md' --handoff --provider codex)"
check_eq           'handoff explicit codex status'   "$(field "$out" status)"       'ok'
check_eq           'handoff explicit codex provider' "$(field "$out" provider)"     'codex'
check_eq           'handoff explicit codex mode'     "$(field "$out" payload_mode)" 'handoff'
check_contains     'handoff codex seed has path'     "$(msg_block "$out")"          '/x/doc.md'
check_contains     'handoff codex seed guardrail'    "$(msg_block "$out")"          'GUARDRAIL'
check_not_contains 'handoff codex no /target'        "$(msg_block "$out")"          '/target'

out="$(run '/x/gemini.md' --handoff --provider gemini)"
check_eq           'handoff explicit gemini status'   "$(field "$out" status)"       'ok'
check_eq           'handoff explicit gemini provider' "$(field "$out" provider)"     'gemini'
check_eq           'handoff explicit gemini mode'     "$(field "$out" payload_mode)" 'handoff'
check_not_contains 'handoff gemini no /target'        "$(msg_block "$out")"          '/target'

# --- unverified handoff providers stay outside this feature's allowlist ------
out="$(run '/x/doc.md' --handoff --provider agy)"
check_eq       'handoff explicit unsupported error' "$(field "$out" status)" 'error'
check_contains 'handoff unsupported names allowlist' "$(field "$out" error)" 'claude, codex, gemini'

# --- configured codex/gemini routing is honored for a handoff ----------------
_resolver_codex="$(mktemp)"
printf '#!/usr/bin/env bash\necho codex\n' > "$_resolver_codex"
chmod +x "$_resolver_codex"
out="$(DISPATCH_PROVIDER_RESOLVER="$_resolver_codex" run '/tmp/doc.md' --handoff)"
check_eq 'handoff config codex status'   "$(field "$out" status)"   'ok'
check_eq 'handoff config codex provider' "$(field "$out" provider)" 'codex'
rm -f "$_resolver_codex"

_resolver_unsupported="$(mktemp)"
printf '#!/usr/bin/env bash\necho agy\n' > "$_resolver_unsupported"
chmod +x "$_resolver_unsupported"
out="$(DISPATCH_PROVIDER_RESOLVER="$_resolver_unsupported" run '/tmp/doc.md' --handoff)"
check_eq 'handoff unsupported config status'   "$(field "$out" status)"   'ok'
check_eq 'handoff unsupported config fallback' "$(field "$out" provider)" 'claude'
rm -f "$_resolver_unsupported"

# --- --discuss / --ask are retired flags -> unknown-argument error (x-cbb0) ---
# discuss is subsumed by a verbatim seed; ask by the headless substrate. Both
# flags now fail loud rather than silently no-op.
out="$(run 'what is the architecture of the loop' --discuss)"
check_eq 'retired --discuss is error' "$(field "$out" status)" 'error'
out="$(run 'quick question' --ask)"
check_eq 'retired --ask is error'     "$(field "$out" status)" 'error'

# ===========================================================================
# Cross-project cwd resolution (-P/--project)  (cross-project-spawn)
# ===========================================================================
# Hermetic injectable resolver (mirrors DISPATCH_PROVIDER_RESOLVER): speaks the
# `ok\t<canon>\t<path>` | `notfound\t<csv>` | `error\t<msg>` protocol so these
# tests never touch fno or settings.yaml. `etl` maps to $HERE (a real dir on
# disk); `ghost` maps to a path that does not exist; anything else is unknown.
_proj_res="$(mktemp)"
{
  printf '#!/usr/bin/env bash\n'
  printf 'case "$1" in\n'
  printf '  etl)   printf '\''ok\\tetl\\t%%s\\n'\'' "%s" ;;\n' "$HERE"
  printf '  ghost) printf '\''ok\\tghost\\t/no/such/dir/zzz\\n'\'' ;;\n'
  printf '  *)     printf '\''notfound\\tchingu,etl,fno,loci,web\\n'\'' ;;\n'
  printf 'esac\n'
} > "$_proj_res"
chmod +x "$_proj_res"

# --- free-text + -P resolves: project + resolved_cwd emitted; free text seeds --
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'backend work' -P etl)"
check_eq   'proj resolve status'        "$(field "$out" status)"       'ok'
check_eq   'proj resolve project'       "$(field "$out" project)"      'etl'
check_eq   'proj resolve cwd'           "$(field "$out" resolved_cwd)" "$HERE"
check_eq   'proj resolve node empty'    "$(field "$out" node)"         ''
check_eq   'proj resolve free text seeds' "$(field "$out" payload_mode)" 'seed'
check_eq   'proj resolve seed verbatim'   "$(field "$out" message)"      'backend work'

# --- --project long form is identical to -P ----------------------------------
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'backend work' --project etl)"
check_eq   'proj long-form cwd' "$(field "$out" resolved_cwd)" "$HERE"

# --- no -P: project + resolved_cwd are empty (default caller-cwd launch) -------
out="$(run 'backend work')"
check_eq   'no-proj project empty'      "$(field "$out" project)"      ''
check_eq   'no-proj resolved_cwd empty' "$(field "$out" resolved_cwd)" ''

# --- unknown project -> error with the known-names list -----------------------
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'backend work' -P bad)"
check_eq       'unknown proj status' "$(field "$out" status)" 'error'
check_contains 'unknown proj lists known' "$(field "$out" error)" 'etl, fno'

# --- project resolves but path is missing on disk -> error (early stat) --------
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'backend work' -P ghost)"
check_eq       'missing-path status'   "$(field "$out" status)" 'error'
check_contains 'missing-path names path' "$(field "$out" error)" '/no/such/dir/zzz'

# --- node + -P conflicts (a node carries its own project) ---------------------
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'ab-12345678' -P etl)"
check_eq       'node+proj conflict status' "$(field "$out" status)" 'error'
check_contains 'node+proj conflict hints force' "$(field "$out" error)" '--force'

# --- node + -P + -f forces the override (flag wins) ---------------------------
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'ab-12345678' -P etl -f)"
check_eq   'forced override status'  "$(field "$out" status)"       'ok'
check_eq   'forced override node'    "$(field "$out" node)"         'ab-12345678'
check_eq   'forced override project' "$(field "$out" project)"      'etl'
check_eq   'forced override cwd'     "$(field "$out" resolved_cwd)" "$HERE"

# --- a slug candidate is an unresolved node ref -> also conflicts with -P ------
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'some-slug' -P etl)"
check_eq   'slug+proj conflict status' "$(field "$out" status)" 'error'
# ...and --force overrides that too
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'some-slug' -P etl -f)"
check_eq   'slug+proj force status' "$(field "$out" status)" 'ok'

# --- a --project token in free-text PROSE is verbatim seed content (x-cbb0): a
#     seed is exempt from the flag scan, so it does NOT error. A glued flag in a
#     DISPATCHED command (node build / passthrough) still fails loud (asserted
#     above via `ab-99999999 --model opus` etc.).
out="$(run 'add a --project switch to the cli')"
check_eq   'stray --project in a seed is verbatim, not an error' "$(field "$out" status)" 'ok'
check_eq   'stray --project seed mode'  "$(field "$out" payload_mode)" 'seed'

# --- -P with an empty value fails loud (codex P2): never a silent caller-cwd hop -
out="$(run 'backend work' -P '')"
check_eq       'empty -P value status' "$(field "$out" status)" 'error'
check_contains 'empty -P value message' "$(field "$out" error)" 'requires a project name'
# a bare trailing -P (no value token at all) is the same loud refusal
out="$(bash "$NORM" --input 'backend work' -P)"
check_eq       'bare trailing -P status' "$(field "$out" status)" 'error'

rm -f "$_proj_res"

# --- the node-id shape is config-agnostic, not hardcoded to ab- ---------------
# run_nofno pins the no-merge posture to the builtin default so the message
# assertions do not inherit the host repo's dispatch.auto_merge.
out="$(run_nofno 'x-2aad bg')"
check_eq 'configured-prefix node classifies'      "$(field "$out" node)" 'x-2aad'
check_eq 'configured-prefix build mode'           "$(field "$out" payload_mode)" 'build'
check_eq 'configured-prefix substrate survives'   "$(field "$out" substrate)" 'bg'
check_eq 'configured-prefix message'              "$(field "$out" message)" '/target x-2aad no-merge'

out="$(run_nofno 'ab-4040eee8')"
check_eq 'ab- id still classifies (regression)'   "$(field "$out" node)" 'ab-4040eee8'
check_eq 'ab- id still builds (regression)'       "$(field "$out" payload_mode)" 'build'

out="$(run_nofno 'x-0123456789ab')"
check_eq 'over-long hex is not a node'            "$(field "$out" node)" ''
check_eq 'over-long hex becomes a query'          "$(field "$out" node_query)" 'x-0123456789ab'
check_eq 'over-long hex seeds'                    "$(field "$out" payload_mode)" 'seed'

out="$(run_nofno '1234abcd')"
check_eq 'bare hex is a query, not a guessed id'  "$(field "$out" node)" ''
check_eq 'bare hex query value'                   "$(field "$out" node_query)" '1234abcd'
# The id the resolver hands back must classify as tier 1, or the SKILL's
# re-normalize would reclassify it as a slug candidate forever.
out="$(run_nofno 'x-1a2b')"
check_eq 'resolved id terminates as a build'      "$(field "$out" payload_mode)" 'build'

out="$(run_nofno '/target internal/fno/plans/20260711-dark-mode-x-8af8.md')"
check_eq 'passthrough extracts configured id'     "$(field "$out" node)" 'x-8af8'
check_eq 'passthrough mode'                       "$(field "$out" payload_mode)" 'passthrough'

out="$(run_nofno 'x-2aad --yolo')"
check_eq 'flag-scan fires on configured prefix'   "$(field "$out" status)" 'error'

# --- node_bare: did the user TYPE an id, or did prose merely look like one? ---
# a-f are letters, so the shape matches hyphen-joined English. VALIDATE refuses
# loud only on node_bare=1; an inferred id degrades to a verbatim seed.
out="$(run_nofno 'x-2aad')"
check_eq 'bare id is deliberate'                  "$(field "$out" node_bare)" '1'
out="$(run_nofno 'x-2aad bg')"
check_eq 'posture words do not spoil bareness'    "$(field "$out" node_bare)" '1'
out="$(run_nofno 'dead-beef cleanup')"
check_eq 'hex-shaped prose word is inferred'      "$(field "$out" node_bare)" '0'
out="$(run_nofno 're-added the auth check')"
check_eq 're-added is inferred, not deliberate'   "$(field "$out" node_bare)" '0'
out="$(run_nofno 'ab-4040eee8 fix the login')"
check_eq 'id plus trailing prose is inferred'     "$(field "$out" node_bare)" '0'
out="$(run_nofno '/target internal/fno/plans/20260711-dark-mode-x-8af8.md')"
check_eq 'passthrough id is never bare'           "$(field "$out" node_bare)" '0'
# A payload with no id-shaped first token is untouched by any of this.
out="$(run_nofno 'dead code cleanup')"
check_eq 'unhyphenated prose stays a seed'        "$(field "$out" payload_mode)" 'seed'
check_eq 'unhyphenated prose message verbatim'    "$(field "$out" message)" 'dead code cleanup'

# field() runs the harness's own sed over a message= line carrying the raw byte,
# so it needs the C locale too or it prints the very error under test.
out="$(LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 run_nofno "$(printf 'task \x80 text')" 2>/dev/null)"
check_eq       'invalid utf8 byte does not abort' "$(LC_ALL=C field "$out" status)" 'ok'
check_contains 'invalid utf8 keeps the full name' "$(LC_ALL=C field "$out" name)" 'text'

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
