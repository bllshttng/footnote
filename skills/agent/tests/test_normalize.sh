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
out="$(run 'spawn the node that will merge two branches')"
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

# trailing `merge` after a non-posture token still binds (right-anchored run)
out="$(run 'ab-22222222 merge')"
check_eq   'trailing merge binds' "$(field "$out" allow_merge)" '1'
check_eq   'trailing merge node'  "$(field "$out" node)" 'ab-22222222'

# --- provider bareword alone --------------------------------------------------
out="$(run 'add a login form gemini')"
check_eq   'provider bareword gemini' "$(field "$out" provider)" 'gemini'

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
out="$(run 'ab-99999999 as merge')"
check_eq   'as merge -> name=merge'           "$(field "$out" name)" 'merge'
check_eq   'as merge -> allow_merge stays 0'  "$(field "$out" allow_merge)" '0'
# a posture word BEFORE the `as <name>` pair still binds normally
out="$(run 'ab-99999999 codex as worker')"
check_eq   'codex as worker -> provider codex' "$(field "$out" provider)" 'codex'
check_eq   'codex as worker -> name worker'    "$(field "$out" name)" 'worker'

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
out="$(run "$ml2")"
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

# --- tier 3 (AC4-HP): bare 8-hex re-prefixes to ab- ---------------------------
out="$(run '1234abcd')"
check_eq   'tier3 bare-hex status'   "$(field "$out" status)" 'ok'
check_eq   'tier3 bare-hex node'     "$(field "$out" node)"   'ab-1234abcd'
check_contains 'tier3 bare-hex message carries canonical id' "$(field "$out" message)" '/target ab-1234abcd'

# --- tier 3 (AC4-ERR): NOT exactly 8 hex is NOT a bare-hex id ------------------
# 10 hex chars: falls through to the slug-candidate tier as free text, never a
# malformed id. node stays empty (no malformed id reaches VALIDATE).
out="$(run '1234abcdef')"
check_eq   'tier3 10-hex not a node'        "$(field "$out" node)" ''
check_eq   'tier3 10-hex is a slug-candidate' "$(field "$out" node_query)" '1234abcdef'

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
# Dispatch intents: shape_hint + handoff/discuss verbs (2026-06-11)
# ===========================================================================

# --- shape_hint emitted on every run; feature is the default ------------------
out="$(run 'add a dark-mode toggle to settings')"
check_eq       'shape feature default'      "$(field "$out" shape_hint)"   'feature'
check_eq       'shape feature still builds'  "$(field "$out" payload_mode)" 'build'
check_contains 'shape feature /target'       "$(msg_block "$out")"          '/target add a dark-mode toggle'

# --- shape path: an absolute doc path -----------------------------------------
out="$(run '/Users/me/notes/handoff.md')"
check_eq 'shape path absolute' "$(field "$out" shape_hint)" 'path'
out="$(run '~/notes.md')"
check_eq 'shape path tilde'    "$(field "$out" shape_hint)" 'path'
# relative path with an interior slash, no ~/./../ prefix (gemini HIGH, codex P2 #501)
out="$(run 'docs/handoff.md')"
check_eq 'shape path relative interior-slash' "$(field "$out" shape_hint)" 'path'
out="$(run 'subfolder/file.txt')"
check_eq 'shape path relative txt'            "$(field "$out" shape_hint)" 'path'

# --- a slash COMMAND is not a path (no second slash, no extension) ------------
out="$(run '/target add a thing')"
check_eq 'slash command not path' "$(field "$out" shape_hint)" 'feature'
out="$(run '/pr check 42')"
check_eq 'pr command not path'    "$(field "$out" shape_hint)" 'feature'

# --- shape question -----------------------------------------------------------
out="$(run 'should we cache the provider lookup?')"
check_eq 'shape question by ?'         "$(field "$out" shape_hint)" 'question'
out="$(run 'what does normalize emit')"
check_eq 'shape question by lead word' "$(field "$out" shape_hint)" 'question'

# --- shape continue -----------------------------------------------------------
out="$(run 'continue the quarterly outreach work')"
check_eq 'shape continue lead'   "$(field "$out" shape_hint)" 'continue'
out="$(run 'pick up where the last session left off')"
check_eq 'shape continue pickup' "$(field "$out" shape_hint)" 'continue'

# --- handoff mode: doc path -> continuation seed, no /target ------------------
out="$(run '/Users/me/handoff-2026.md' --handoff)"
check_eq           'handoff status'         "$(field "$out" status)"       'ok'
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

# --- handoff is claude-only: an EXPLICIT non-claude provider is an error -----
out="$(run '/x/doc.md' --handoff --provider codex)"
check_eq 'handoff explicit non-claude error' "$(field "$out" status)" 'error'

# --- but config routing the derived tgt-* name to non-claude must NOT error ---
# (codex P2 #501): a configured user who did not choose a provider can still use
# the claude-only verbs; we force claude rather than consulting config routing.
_resolver_codex="$(mktemp)"
printf '#!/usr/bin/env bash\necho codex\n' > "$_resolver_codex"
chmod +x "$_resolver_codex"
out="$(DISPATCH_PROVIDER_RESOLVER="$_resolver_codex" run '/tmp/doc.md' --handoff)"
check_eq 'handoff ignores config non-claude (status)'  "$(field "$out" status)"   'ok'
check_eq 'handoff forces claude despite config routing' "$(field "$out" provider)" 'claude'
out="$(DISPATCH_PROVIDER_RESOLVER="$_resolver_codex" run 'lets talk about the loop' --discuss)"
check_eq 'discuss ignores config non-claude (status)'  "$(field "$out" status)"   'ok'
check_eq 'discuss forces claude despite config routing' "$(field "$out" provider)" 'claude'
rm -f "$_resolver_codex"

# --- discuss mode: verbatim seed, no /target ---------------------------------
out="$(run 'what is the architecture of the loop' --discuss)"
check_eq           'discuss status'           "$(field "$out" status)"       'ok'
check_eq           'discuss payload_mode'      "$(field "$out" payload_mode)" 'discuss'
check_eq           'discuss message verbatim'  "$(field "$out" message)"      'what is the architecture of the loop'
check_not_contains 'discuss no /target'        "$(field "$out" message)"      '/target'

# --- discuss seed starting with / stays verbatim (discuss beats passthrough) --
out="$(run '/target what does it do' --discuss)"
check_eq 'discuss beats passthrough'   "$(field "$out" payload_mode)" 'discuss'
check_eq 'discuss slash seed verbatim' "$(field "$out" message)"      '/target what does it do'

# --- discuss does not resolve a node-shaped seed -----------------------------
out="$(run 'ab-12345678 looks broken' --discuss)"
check_eq 'discuss node empty'       "$(field "$out" node)"       ''
check_eq 'discuss node_query empty' "$(field "$out" node_query)" ''

# --- discuss empty seed -> error (v1: discuss requires an opening message) ----
out="$(run '' --discuss)"
check_eq 'discuss empty seed error' "$(field "$out" status)" 'error'

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

# --- free-text + -P resolves: project + resolved_cwd emitted, build unchanged --
out="$(PROJECT_ROOT_RESOLVER="$_proj_res" run 'backend work' -P etl)"
check_eq   'proj resolve status'        "$(field "$out" status)"       'ok'
check_eq   'proj resolve project'       "$(field "$out" project)"      'etl'
check_eq   'proj resolve cwd'           "$(field "$out" resolved_cwd)" "$HERE"
check_eq   'proj resolve node empty'    "$(field "$out" node)"         ''
check_contains 'proj resolve still builds' "$(msg_block "$out")"       '/target backend work'

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

# --- a stray --project token in task prose fails loud (defensive flag scan) ----
out="$(run 'add a --project switch to the cli')"
check_eq   'stray --project token errors' "$(field "$out" status)" 'error'

# --- -P with an empty value fails loud (codex P2): never a silent caller-cwd hop -
out="$(run 'backend work' -P '')"
check_eq       'empty -P value status' "$(field "$out" status)" 'error'
check_contains 'empty -P value message' "$(field "$out" error)" 'requires a project name'
# a bare trailing -P (no value token at all) is the same loud refusal
out="$(bash "$NORM" --input 'backend work' -P)"
check_eq       'bare trailing -P status' "$(field "$out" status)" 'error'

rm -f "$_proj_res"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
