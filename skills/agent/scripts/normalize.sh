#!/usr/bin/env bash
# normalize.sh - deterministic input normalization for /fno:agent (spawn verb).
#
# Remote and runner-less surfaces can curl quotes or lack a local command runner,
# so a typed `fno agents spawn ...` may split on bad quotes or never execute.
# This helper takes the natural-language payload, normalizes it,
# and emits the exact fields the agents SKILL.md needs to build a genuine
# `fno agents spawn|host <name> "<message>" --provider <p>` launch. It does NOT spawn
# anything (that is spawn.sh, after the SKILL.md confirm gate) and has no
# side effects beyond a read-only provider lookup.
#
# Self-contained skill script. External deps: bash + `fno` (only for the
# config-driven provider fallback, which degrades to `claude` if fno is absent).
#
# Usage:
#   normalize.sh --input "<raw payload>" [--name <n>] [--provider <p>] [--allow-merge]
#
# Emits key=value lines on stdout (one per line; values are NOT shell-quoted -
# read them line by line, never `eval`):
#   status=ok | status=error
#   error=<message>            (only when status=error)
#   node=<ab-XXXXXXXX>         (empty when the payload is a free-form feature)
#   name=<agent name>
#   provider=<claude|codex|gemini>
#   model=<exact model name>   (empty unless `model <name>` / --model given)
#   message=<final message to pass verbatim to the spawn/host verb>
#
# Locked decisions honored:
#   4. provider = explicit -> config.providers (resolve_dispatch_target) -> claude
#   8. the message is one unit; the SKILL builds `fno agents spawn <name> "<msg>"`
#      with name POSITIONAL.

set -uo pipefail

# Mirrors the Rust KNOWN_PROVIDERS source of truth (crates/fno-agents provider.rs):
# the set `fno agents spawn --provider` accepts. Widen both together. (hermes /
# openclaw are megawalk drivers, a different axis, not spawn providers.)
VALID_PROVIDERS="claude codex gemini agy opencode"

INPUT=""
NAME=""
NAME_SET=0         # 1 = -n/--name was passed (even if empty -> empty-name error)
PROVIDER=""
MODEL=""           # exact model name forwarded to `fno agents spawn --model`
                   # (each provider's own --model). Dashless: `model <name>`.
                   # No short flag: `-m` is taken by --allow-merge.
ALLOW_MERGE=0
YES=0              # 1 = -y/--yes: skip the confirm (consumed by the SKILL policy)
MODE="exec"        # exec | interactive  (-i routes codex/gemini -> host)
SUBSTRATE=""       # x-2c27: ""|bg|headless trailing-posture word (the spawn
                   # substrate axis). Empty = the default `pane` (owned-PTY).
YOLO=0             # 1 = full-auto (codex/gemini bypass); sandboxed default
ASK_MODE=0         # 1 = `ask`/`bare` verb: send the prompt VERBATIM (no /target)
HANDOFF_MODE=0     # 1 = `handoff` verb: payload is a doc path -> continuation seed
DISCUSS_MODE=0     # 1 = `discuss` verb: payload is a verbatim conversational seed
PROJECT=""         # cross-project target: a registry project name/short_name to
                   # resolve into a launch cwd (work.workspaces.*.projects)
PROJECT_SET=0      # 1 = -P/--project was passed (empty value -> loud error, never
                   # a silent caller-cwd launch when a cross-project hop was asked)
FORCE=0            # 1 = -f/--force: let --project win over a node's own cwd
PERMISSION_MODE="" # x-dfa4: forwarded to spawn.sh --permission-mode (provider-native, CLI fails closed)
ROLE=""            # x-d2fe: per-spawn model-routing role; forwarded to spawn.sh --role
TIMEOUT=""         # per-spawn timeout seconds; forwarded to spawn.sh --timeout
FRESH=0            # 1 = --fresh: resolve worker cwd to canonical main root
HERE=0             # 1 = --here/--in-place: keep caller cwd (opt out of --fresh)

emit_error() { printf 'status=error\nerror=%s\n' "$1"; exit 0; }

# iOS smart punctuation rewrites a typed `--` into an em-dash (U+2014) or, on
# some keyboards, an en-dash (U+2013) - the same failure class as the smart
# quotes below. Canonicalize a token-initial em/en-dash back to `--` before
# flag matching so a phone-mangled `--yes` parses as the flag, not an unknown
# argument. bash 3.2 safe: printf octal, no \u. (ab-27541df5 US1)
EMDASH=$(printf '\342\200\224')   # U+2014 em dash
ENDASH=$(printf '\342\200\223')   # U+2013 en dash

while [[ $# -gt 0 ]]; do
  tok="$1"
  case "$tok" in
    "$EMDASH"*) tok="--${tok#"$EMDASH"}" ;;
    "$ENDASH"*) tok="--${tok#"$ENDASH"}" ;;
  esac
  # Value-flags guard $# so a bare trailing flag (a fat-fingered phone tap)
  # cannot wedge `shift 2` into an out-of-range no-op infinite loop.
  case "$tok" in
    --input)          INPUT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -n|--name)        NAME="${2:-}"; NAME_SET=1; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --provider)       PROVIDER="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --model)          MODEL="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --permission-mode) PERMISSION_MODE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -r|--role)        ROLE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -t|--timeout)     TIMEOUT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --fresh)          FRESH=1; shift ;;
    --here|--in-place) HERE=1; shift ;;
    -m|--allow-merge) ALLOW_MERGE=1; shift ;;
    -y|--yes)         YES=1; shift ;;
    -i|--interactive) MODE="interactive"; shift ;;
    --yolo)           YOLO=1; shift ;;
    --ask)            ASK_MODE=1; shift ;;
    --handoff)        HANDOFF_MODE=1; shift ;;
    --discuss)        DISCUSS_MODE=1; shift ;;
    -P|--project)     PROJECT="${2:-}"; PROJECT_SET=1; [[ $# -ge 2 ]] && shift 2 || shift ;;
    # -f here = override the node's own cwd with --project. NOT `fno agents spawn
    # -F/--force` (spawn-gate bypass: max_live + RAM floor). Different semantics;
    # do not plumb the CLI gate-bypass through as -f.
    -f|--force)       FORCE=1; shift ;;
    *) emit_error "unknown argument: $tok" ;;
  esac
done

# ---- 1. smart-quote normalization (bash 3.2 safe: printf octal, no \u) -------
# U+201C/201D -> "  ; U+2018/2019 -> '  (the #1 phone failure mode).
ldq=$(printf '\342\200\234')   # U+201C left double quote
rdq=$(printf '\342\200\235')   # U+201D right double quote
lsq=$(printf '\342\200\230')   # U+2018 left single quote
rsq=$(printf '\342\200\231')   # U+2019 right single quote
sq="'"                          # a literal apostrophe cannot sit inside ${//}
msg="$INPUT"
msg="${msg//$ldq/\"}"
msg="${msg//$rdq/\"}"
msg="${msg//$lsq/$sq}"
msg="${msg//$rsq/$sq}"

# Trim leading/trailing whitespace (incl. tabs/newlines a paste may carry).
msg="${msg#"${msg%%[![:space:]]*}"}"
msg="${msg%"${msg##*[![:space:]]}"}"

# Boundary: an empty / whitespace-only payload is refused with no spawn.
[[ -z "$msg" ]] && emit_error "empty task: nothing to dispatch"

# ---- 1a. dashless trailing-run posture parse (ab-994222ee) -------------------
# The documented grammar is dashless: `<task> [codex|gemini] [interactive|drive]
# [yolo|auto] [as <name>] [merge]`. normalize.sh is the deterministic backstop:
# it recognizes the closed posture vocabulary as a CONTIGUOUS TRAILING run,
# right-anchored. Scan tokens from the RIGHT, consuming recognized posture
# barewords (and an `as <name>` pair) until the first token NOT in the
# vocabulary; everything to its left is the task text, verbatim. A mid-task
# occurrence ("spawn the node that will merge two branches") stays task text
# because a non-posture token follows it (AC1-EDGE). `as` with nothing after it
# in the run is a dangling name keyword -> error (Boundaries). Barewords only
# FILL a field the model did not already set via an explicit dash-flag, and are
# idempotent with it (AC4-FR), so the NL parse and this parse agree (Concurrency
# invariant). ask/bare is a LEADING verb the SKILL handles (passed as --ask), so
# it is NOT consumed here - that avoids a trailing "the user's ask" false match;
# a verbatim ask payload skips this scan entirely. handoff (a doc path) and
# discuss (a verbatim chat seed) are verbatim too, so they skip it the same way.
if [[ "$ASK_MODE" -eq 0 && "$HANDOFF_MODE" -eq 0 && "$DISCUSS_MODE" -eq 0 ]]; then
  set -f
  # Read the FULL message (every line) into the token array. A bare
  # `read -r -a <<<` stops at the first newline, so a multi-line task whose
  # first line ends in a posture word would silently drop lines 2+ when the run
  # is rebuilt (gemini CRITICAL). `-d ''` reads to the NUL the here-string never
  # contains, consuming the whole payload; it returns non-zero at EOF, so `|| :`
  # keeps the pipeline happy. IFS (space/tab/newline) does the word-split.
  IFS=$' \t\n' read -r -d '' -a _toks <<< "$msg" || :
  set +f
  _n=${#_toks[@]}
  _end=$_n
  while (( _end > 0 )); do
    _i=$(( _end - 1 ))
    _t="${_toks[$_i]}"
    # `as <name>` binds the token AFTER `as` as the name, verbatim - even when
    # that token is itself a posture word (`as gemini` names the worker
    # "gemini", it does NOT set provider=gemini). So the left-peek for `as` runs
    # BEFORE the posture case-match, consuming the `as <name>` pair as a unit.
    if (( _i >= 1 )) && [[ "$(printf '%s' "${_toks[$((_i-1))]}" | tr '[:upper:]' '[:lower:]')" == "as" ]]; then
      [[ "$NAME_SET" -eq 0 ]] && { NAME="$_t"; NAME_SET=1; }
      _end=$(( _i - 1 ))
      continue
    fi
    # `model <name>` binds the token AFTER `model` as the exact model name,
    # verbatim - same left-peek shape as `as <name>` so a model name that is
    # not a posture word (opus/sonnet/gpt-5) is consumed as the value, not as
    # task text. A dash-flag `--model` already wins (idempotent fill).
    if (( _i >= 1 )) && [[ "$(printf '%s' "${_toks[$((_i-1))]}" | tr '[:upper:]' '[:lower:]')" == "model" ]]; then
      [[ -z "$MODEL" ]] && MODEL="$_t"
      _end=$(( _i - 1 ))
      continue
    fi
    _lt=$(printf '%s' "$_t" | tr '[:upper:]' '[:lower:]')
    # A provider bareword is any non-claude token in VALID_PROVIDERS (which
    # mirrors the Rust KNOWN_PROVIDERS source of truth), so a newly-supported
    # harness needs no parser edit - just widen VALID_PROVIDERS. claude is the
    # default and stays task text (matching the historical codex|gemini arm).
    # Inline string-membership, not is_valid_provider(): that fn is defined
    # later in the file and is not yet in scope during this top-level parse.
    if [[ "$_lt" != claude && " $VALID_PROVIDERS " == *" $_lt "* ]]; then
      [[ -z "$PROVIDER" ]] && PROVIDER="$_lt"; _end=$_i; continue
    fi
    case "$_lt" in
      yolo|auto)           YOLO=1; _end=$_i ;;
      merge)               ALLOW_MERGE=1; _end=$_i ;;
      interactive|drive)   [[ "$MODE" == "exec" ]] && MODE="interactive"; _end=$_i ;;
      bg|headless)         [[ -z "$SUBSTRATE" ]] && SUBSTRATE="$_lt"; _end=$_i ;;
      as)                  emit_error "'as' is a name keyword with no name after it; write 'as <name>' or drop it" ;;
      model)               emit_error "'model' is a keyword with no name after it; write 'model <name>' or drop it" ;;
      *)                   break ;;  # task-text boundary (the run ends here)
    esac
  done
  # Strip the consumed trailing tokens from the ORIGINAL msg (right to left),
  # preserving the leading task text's whitespace/newlines verbatim. Re-joining
  # "${_toks[*]:0:$_end}" would collapse every newline to a single space, so a
  # multi-line task would be reformatted even when no data is lost.
  if (( _end < _n )); then
    for (( _k=_n-1; _k>=_end; _k-- )); do
      msg="${msg%"${msg##*[![:space:]]}"}"   # trim trailing whitespace
      msg="${msg%"${_toks[$_k]}"}"           # peel this token off the end
    done
    msg="${msg%"${msg##*[![:space:]]}"}"     # trim trailing whitespace
    msg="${msg#"${msg%%[![:space:]]*}"}"     # trim leading whitespace
  fi
  # The trailing run may have consumed the whole payload (`codex yolo merge`
  # with no task) -> refuse with no spawn (Boundaries: empty task fails loud).
  [[ -z "$msg" ]] && emit_error "empty task: only posture modifiers, nothing to dispatch"
  # Leading posture-word guard (x-ffc3): the posture vocabulary (bg|headless) is
  # TRAILING only (consumed right-anchored above). A LEADING posture word whose
  # remainder is a /command passthrough (e.g. `bg /goal ...`) is the user meaning
  # the substrate but mis-ordering it; left alone it is not consumed, the payload
  # then starts with the bareword (not '/'), and the feature default silently
  # wraps it as a /target BUILD of the literal text. Refuse with the corrective
  # trailing form instead.
  #   - Scoped to a /-led remainder: genuine feature prose that merely begins with
  #     the word (`headless browser screenshots`, `bg worker cleanup`) still flows
  #     to the normal build path - only a clear mis-ordered dispatch is refused
  #     (PR #106 codex P2). This is the plan's preferred narrow trigger.
  #   - Exact-token, case-insensitive (matches the trailing parser's tr at l.150,
  #     so a mobile-auto-capitalized `BG` is caught; `bgcolor`/`background` are not).
  #   - Inside the ask/handoff/discuss==0 block, so a verbatim payload beginning
  #     with the literal word "bg" is exempt (its posture words are never parsed).
  _first_lc="$(printf '%s' "${msg%%[[:space:]]*}" | tr '[:upper:]' '[:lower:]')"
  case "$_first_lc" in
    bg|headless)
      _rest="${msg#"${msg%%[[:space:]]*}"}"; _rest="${_rest#"${_rest%%[![:space:]]*}"}"  # trim
      if [[ "$_rest" == /* ]]; then
        emit_error "posture words are trailing, not leading: write the dispatch first then the substrate, e.g. 'spawn ${_rest} ${_first_lc}'. (A leading '${_first_lc}' would otherwise be wrapped into a /target build of the literal text.)"
      fi
      ;;
  esac
fi

# ---- 1b. defensive flag-vocabulary scan (phone-mangled flag in prose) --------
# If a known flag token survived in the BUILD task text (the LLM rescue layer
# missed it, or the operator typed it inline), fail loud rather than let it fold
# silently into the build brief. Whitespace-delimited, token-initial dash forms
# only: feature prose with unrelated dash tokens ("support -v verbose") and
# glued tokens ("tooltip--yes") pass untouched. The scan canonicalizes a COPY of
# each token's leading em/en-dash to `--`; the message keeps the original text
# verbatim. `set -f` disables globbing so an unquoted `*` in the task text cannot
# expand to filenames during the word-split. ASK payloads are exempt: a one-shot
# question is sent verbatim - it has no build brief a flag could fold into, and a
# flag token there is part of the question ("what does grep -i do").
# (ab-27541df5 US1, Locked Decision 5)
# handoff/discuss are verbatim like ask (a doc path / chat seed has no build
# brief a flag could fold into), so they are exempt the same way.
if [[ "$ASK_MODE" -eq 0 && "$HANDOFF_MODE" -eq 0 && "$DISCUSS_MODE" -eq 0 ]]; then
  set -f
  for scan_tok in $msg; do
    scan_cano="$scan_tok"
    case "$scan_cano" in
      "$EMDASH"*) scan_cano="--${scan_cano#"$EMDASH"}" ;;
      "$ENDASH"*) scan_cano="--${scan_cano#"$ENDASH"}" ;;
    esac
    case "$scan_cano" in
      -y|--yes|-m|--allow-merge|-n|--name|-i|--interactive|--yolo|--provider|--model|--ask|-P|--project|-f|--force|--permission-mode|-r|--role|-t|--timeout|--fresh|--here|--in-place)
        emit_error "the task text contains a token that looks like a dispatch flag ('$scan_tok') - refusing so it cannot fold silently into the build brief. Pass it as a real flag (-y / -m / -n N) separated from the task text (on a phone use the single-dash short form: iOS turns a typed -- into a long dash), or quote/rephrase it if it is genuinely part of the feature text."
        ;;
    esac
  done
  set +f
fi
set +f

# ---- 2. node detection + resolution-tier classification ----------------------
# A backlog node id is exactly `ab-` + 8 lowercase hex (tier 1, exact). Three
# id-free entry modes layer on top (ab-f82e8083):
#   tier 3 - a bare 8-hex first token (no `ab-`, no hyphen: autocorrect-neutral)
#            re-prefixes to `ab-`. EXACTLY 8 hex, so a 10-char hex string is NOT
#            a node id (AC4-ERR) and falls through to the describe-it tier.
#   tier 2 - a single slug-shaped token is a slug CANDIDATE the SKILL resolves
#            via `fno backlog get` (exact slug -> ab-id), falling through to
#            describe-it on a miss.
#   tier 5 - `next` / `next all` asks for the top ready node; the SKILL resolves
#            it via `fno backlog next` (this-project default; `all` widens).
# Slug + next resolution need the graph, so normalize only CLASSIFIES them here
# (deterministic + unit-testable); the SKILL does the lookup and re-normalizes
# with the resolved ab-id. The describe-it fuzzy tier (tier 4) is whatever is
# left - free prose - and lives entirely in the SKILL body behind a confirm.
NODE=""
NODE_QUERY=""
SPAWN_NEXT=0
NEXT_SCOPE=""
first_tok="${msg%%[[:space:]]*}"
msg_lc="$(printf '%s' "$msg" | tr '[:upper:]' '[:lower:]')"
if [[ "$HANDOFF_MODE" -eq 1 || "$DISCUSS_MODE" -eq 1 ]]; then
  :   # handoff (a doc path) / discuss (a chat seed) carry no node id; a
      # node-shaped token in the seed must NOT be resolved as a node.
elif [[ "$msg_lc" == "next" || "$msg_lc" == "next all" ]]; then
  SPAWN_NEXT=1
  [[ "$msg_lc" == "next all" ]] && NEXT_SCOPE="all" || NEXT_SCOPE="project"
elif printf '%s' "$first_tok" | grep -qE '^ab-[0-9a-f]{8}$'; then
  NODE="$first_tok"                                   # tier 1: exact ab-id
elif printf '%s' "$first_tok" | grep -qE '^[0-9a-f]{8}$'; then
  NODE="ab-$first_tok"                                # tier 3: bare 8-hex
  # Rewrite the leading bare-hex token so the /target message the worker runs
  # carries the canonical id, not the bare hex.
  msg="ab-${first_tok}${msg#"$first_tok"}"
elif printf '%s' "$msg" | grep -qE '^/target([[:space:]]|$)'; then
  NODE="$(printf '%s' "$msg" | grep -oE 'ab-[0-9a-f]{8}' | head -1)"
elif printf '%s' "$msg" | grep -iqE '^[a-z0-9][a-z0-9-]*$'; then
  # tier 2: slug candidate. Case-insensitive (`-i`) so a mobile-auto-capitalized
  # slug (`Dashless-spawn`) is still classified as a candidate; the resolver
  # (`fno backlog get` -> resolve_node) matches slugs case-insensitively.
  NODE_QUERY="$msg"
fi

# ---- 2b. shape classification (for the SKILL's bare-input announce) ----------
# A deterministic hint at what KIND of payload this is, emitted on every run.
# The SKILL uses it ONLY on the no-explicit-verb path: when bare input is not
# feature-shaped (a path, a question, a continue/handoff phrasing) it announces
# the /target build wrap and offers `handoff`/`discuss` instead of silently
# wrapping. Order: path > continue > question > feature (the default). grep -E is
# used (not bash `=~`) to match the file's bash-3.2-safe style.
shape_hint="feature"
if printf '%s' "$first_tok" | grep -qE '^(~|\./|\.\./)'; then
  shape_hint="path"                          # ~, ./ or ../ prefix
elif printf '%s' "$first_tok" | grep -qE '^/.*/'; then
  shape_hint="path"                          # leading / with an interior slash
elif printf '%s' "$first_tok" | grep -qE '^/.+\.[A-Za-z0-9]+$'; then
  shape_hint="path"                          # leading / single segment with ext
elif printf '%s' "$first_tok" | grep -qE '^[^/]+/.+'; then
  shape_hint="path"                          # relative path with an interior slash
fi
# A bare slash COMMAND (/target, /pr) has neither an interior slash nor an
# extension, so it stays `feature` and never trips the path announce.
_first_word="${msg_lc%%[[:space:]]*}"
if [[ "$shape_hint" == "feature" ]]; then
  case "$_first_word" in
    continue|resume|pickup) shape_hint="continue" ;;
  esac
  case "$msg_lc" in
    "pick up"*|"hand off"*|handoff*) shape_hint="continue" ;;
  esac
fi
if [[ "$shape_hint" == "feature" ]]; then
  case "$msg" in
    *\?) shape_hint="question" ;;
  esac
  if [[ "$shape_hint" == "feature" ]]; then
    case "$_first_word" in
      what|why|how|should|can|does|is|are|when|who) shape_hint="question" ;;
    esac
  fi
fi

# ---- 2c. cross-project cwd resolution (-P/--project) -------------------------
# A free-text spawn launches in the caller's cwd by default. `-P/--project <name>`
# retargets it: resolve the registry name/short_name to its work-map root and emit
# resolved_cwd, which the SKILL passes verbatim to `spawn.sh --cwd`. Resolution is
# a PURE config lookup (work.workspaces.*.projects in settings.yaml) - no graph, no
# lock, no LLM judgment - so it lives here in the deterministic layer alongside the
# provider lookup, not in the SKILL (which owns the graph-needing slug/next tiers).
# The `in <project>` / `as <project>` natural-language ergonomic is the SKILL's
# model-judged job: it disambiguates a directive from task prose ("fix the bug in
# etl") and calls this script with -P. normalize only ever sees the unambiguous flag.
#
# Node conflict: a backlog node carries its OWN project (its _resolved_cwd), so a
# node + --project is contradictory. Refuse loud by default; -f/--force flips it to
# a flag-win override (run the node's work in the forced repo). A slug candidate or
# `next` pointer is an as-yet-unresolved node reference, so it conflicts too - the
# SKILL re-runs normalize with the resolved ab-id (carrying -P/-f), and the conflict
# fires deterministically there.
#
# Resolver is test-injectable via PROJECT_ROOT_RESOLVER (mirrors the provider and
# slug resolvers): a command taking the project name and printing ONE line in the
# protocol `ok\t<canonical>\t<abspath>` | `notfound\t<csv of known names>` |
# `error\t<message>`. Default = the shipped fno python (resolve_project_name folds
# short_name -> canonical; project_root_from_settings maps canonical -> abs path).
PROJECT_CANON=""
RESOLVED_CWD=""

resolve_project() {
  local _proj="$1"
  if [[ -n "${PROJECT_ROOT_RESOLVER:-}" ]]; then
    "$PROJECT_ROOT_RESOLVER" "$_proj" 2>/dev/null
    return 0
  fi
  local abi_bin shebang
  abi_bin="$(command -v fno 2>/dev/null)" || { printf 'error\tfno not on PATH (cannot resolve --project %s)\n' "$_proj"; return 0; }
  # Strip a trailing CR: a CRLF-lined fno (WSL / git autocrlf) would leave \r on
  # the shebang and break the interpreter exec.
  shebang="$(head -1 "$abi_bin" 2>/dev/null | sed 's/^#![[:space:]]*//' | tr -d '\r')"
  # Same interpreter guard as resolve_from_config: only a python entrypoint can
  # import the fno package. A shell-wrapper fno means the resolver is unreachable.
  [[ -n "$shebang" && "$shebang" == *python* ]] || { printf 'error\tproject resolver unavailable (fno is not a python entrypoint)\n'; return 0; }
  local py_cmd=()
  read -r -a py_cmd <<< "$shebang"
  { [[ -x "${py_cmd[0]}" ]] || command -v "${py_cmd[0]}" >/dev/null 2>&1; } || { printf 'error\tproject resolver interpreter not executable\n'; return 0; }
  "${py_cmd[@]}" -c 'import sys
proj = sys.argv[1]
try:
    from fno.projects.resolve import resolve_project_name, ProjectNotFound, _get_cache
    from fno.graph._intake import project_root_from_settings
except Exception as e:
    print("error\tproject resolver import failed: %s" % e); sys.exit(0)
try:
    canon = resolve_project_name(proj)
except ProjectNotFound:
    try:
        known = sorted({v for v in _get_cache().values()})
    except Exception:
        known = []
    print("notfound\t" + ",".join(known)); sys.exit(0)
except Exception as e:
    print("error\t%s" % e); sys.exit(0)
path = project_root_from_settings(canon)
if not path:
    print("error\tproject %r resolved to no path in settings" % canon); sys.exit(0)
print("ok\t%s\t%s" % (canon, path))' "$_proj" 2>/dev/null || { printf 'error\tproject resolver crashed\n'; return 0; }
}

# -P/--project was passed but its value is empty (a bare trailing `-P`, or an
# explicit `-P ""`): the user asked for a cross-project hop, so refuse loud rather
# than silently launch in the caller's cwd (mirrors the empty --name guard below).
if [[ "$PROJECT_SET" -eq 1 && -z "$PROJECT" ]]; then
  emit_error "-P/--project requires a project name (got an empty value)"
fi
if [[ -n "$PROJECT" ]]; then
  # A node reference (resolved ab-id, slug candidate, or `next` pointer) carries
  # its own project; --project conflicts unless forced.
  if [[ "$FORCE" -eq 0 ]] && { [[ -n "$NODE" ]] || [[ -n "$NODE_QUERY" ]] || [[ "$SPAWN_NEXT" -eq 1 ]]; }; then
    emit_error "a backlog node carries its own project, so --project '$PROJECT' conflicts with it. Drop --project (the node's cwd is used), or pass -f/--force to override the node's cwd with project '$PROJECT'."
  fi
  _pres="$(resolve_project "$PROJECT")"
  _pkind="${_pres%%$'\t'*}"
  _prest="${_pres#*$'\t'}"
  case "$_pkind" in
    ok)
      PROJECT_CANON="${_prest%%$'\t'*}"
      RESOLVED_CWD="${_prest#*$'\t'}"
      # Refuse a mapped-but-missing repo BEFORE any billed launch (project_root_
      # from_settings is a pure map lookup and does not stat). Names both the
      # project and the path so the fix is obvious.
      [[ -d "$RESOLVED_CWD" ]] || emit_error "project '$PROJECT_CANON' maps to $RESOLVED_CWD, which does not exist on disk; fix its path in settings.yaml or create the checkout"
      ;;
    notfound)
      _known="$_prest"
      [[ "$_known" == "$_pres" ]] && _known=""   # no tab -> no known list
      if [[ -n "$_known" ]]; then
        emit_error "unknown project '$PROJECT'; known projects: ${_known//,/, }"
      else
        emit_error "unknown project '$PROJECT' (no projects found in settings.yaml work.workspaces)"
      fi
      ;;
    *)
      _emsg="$_prest"
      [[ "$_emsg" == "$_pres" ]] && _emsg="project resolution failed"
      emit_error "$_emsg"
      ;;
  esac
fi

# ---- 3. agent-name derivation ------------------------------------------------
# Explicit --name wins (sanitized). Otherwise the name is provenance-carrying so
# the thread title reads at a glance: <verb>-<full-node-id>-<slug> for a node
# (e.g. spawn-ab-4040eee8-cargo-bootstrapper), <verb>-<slug> for a free-form
# feature, falling back to a deterministic CRC short-id when the slug is empty.
# The leading <verb> is the launching verb (spawn | handoff | discuss | ask),
# never a fixed string, so a self-spawned worker is distinguishable from a
# handoff/discuss thread.
sanitize_name() {
  # lowercase, keep alnum + dash, collapse repeats, trim dashes, cap length.
  printf '%s' "$1" \
    | tr '[:upper:]' '[:lower:]' \
    | tr -c 'a-z0-9-' '-' \
    | sed -E 's/-+/-/g; s/^-+//; s/-+$//' \
    | cut -c1-32
}

# Verb that launched this thread; used as the agent-name prefix.
verb="spawn"
if [[ "$HANDOFF_MODE" -eq 1 ]]; then verb="handoff"
elif [[ "$DISCUSS_MODE" -eq 1 ]]; then verb="discuss"
elif [[ "$ASK_MODE" -eq 1 ]]; then verb="ask"
fi

# Resolve a node's title-derived slug for the readable name tail. Best-effort: an
# unknown node / read failure yields an empty slug and the name degrades to
# <verb>-<full-node-id>. Overridable via NODE_SLUG_RESOLVER for hermetic tests,
# mirroring DISPATCH_PROVIDER_RESOLVER below.
resolve_node_slug() {
  local _id="$1" _raw
  if [[ -n "${NODE_SLUG_RESOLVER:-}" ]]; then
    _raw="$("$NODE_SLUG_RESOLVER" "$_id" 2>/dev/null)"
  else
    _raw="$(fno backlog get "$_id" 2>/dev/null | jq -r '.slug // .title // empty' 2>/dev/null)"
  fi
  sanitize_name "$_raw" | cut -c1-30 | sed -E 's/-+$//'
}

if [[ "$NAME_SET" -eq 1 ]]; then
  agent_name="$(sanitize_name "$NAME")"
  [[ -z "$agent_name" ]] && emit_error "supplied --name normalized to empty"
elif [[ -n "$NODE" ]]; then
  _node_slug="$(resolve_node_slug "$NODE")"
  if [[ -n "$_node_slug" ]]; then
    agent_name="${verb}-${NODE}-${_node_slug}"
  else
    agent_name="${verb}-${NODE}"
  fi
else
  # Free-form name source. For handoff, slug the doc BASENAME (sans extension),
  # not the full path, so `handoff /a/b/project-handoff.md` names the worker
  # `handoff-project-handoff`, not `handoff-a-b-project-handoff`.
  _name_src="$msg"
  if [[ "$HANDOFF_MODE" -eq 1 ]]; then
    _name_src="${msg##*/}"        # basename
    _name_src="${_name_src%.*}"   # strip a trailing extension
  fi
  slug="$(sanitize_name "$_name_src" | cut -c1-20 | sed -E 's/-+$//')"
  if [[ -n "$slug" ]]; then
    agent_name="${verb}-$slug"
  else
    crc="$(printf '%s' "$msg" | cksum | awk '{printf "%08x", $1}')"
    agent_name="${verb}-$crc"
  fi
fi

# ---- 4. provider resolution (defer to config; never reinvent routing) --------
# explicit --provider (validated) wins; else defer to the shipped
# resolve_dispatch_target via fno's own interpreter; else default claude. A
# combo (rotation list) does NOT map to a single --provider, so we only adopt a
# concrete provider_id and otherwise fall back - combo/per-task routing is a
# documented deferred fast-follow that belongs in the provider layer.
is_valid_provider() {
  case " $VALID_PROVIDERS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

resolve_from_config() {
  # Test-injectable: DISPATCH_PROVIDER_RESOLVER is a command taking the agent
  # name and printing a provider (or empty). Default = the real resolver.
  if [[ -n "${DISPATCH_PROVIDER_RESOLVER:-}" ]]; then
    "$DISPATCH_PROVIDER_RESOLVER" "$1" 2>/dev/null || true
    return 0
  fi
  local abi_bin shebang
  abi_bin="$(command -v fno 2>/dev/null)" || return 0
  shebang="$(head -1 "$abi_bin" 2>/dev/null | sed 's/^#![[:space:]]*//')"
  # Only a python interpreter can import the abilities package. A shell-wrapper
  # fno (e.g. shebang "/bin/bash") or an empty shebang means the resolver is
  # unreachable from here - fall back to claude rather than running the Python
  # under the wrong interpreter.
  [[ -n "$shebang" && "$shebang" == *python* ]] || return 0
  # Split the shebang into an argv array so "/usr/bin/env python3" (interpreter
  # + arg) runs correctly; a bare quoted "$shebang" would be treated as one path
  # and both -x and exec would fail. read -r -a is bash 3.2 safe.
  local py_cmd=()
  read -r -a py_cmd <<< "$shebang"
  { [[ -x "${py_cmd[0]}" ]] || command -v "${py_cmd[0]}" >/dev/null 2>&1; } || return 0
  # resolve_dispatch_target returns a provider RECORD id (e.g. "claude-anthropic"),
  # not a CLI name; the CLI name lives at ProviderRecord.cli. Map id -> .cli so a
  # configured non-claude provider resolves correctly instead of silently
  # defaulting to claude. A combo / unresolved target prints nothing (-> claude).
  "${py_cmd[@]}" -c 'import sys
try:
    from fno.sigma_dispatch import resolve_dispatch_target as r
    from fno.adapters.providers.loader import load_providers
    pid = getattr(r(sys.argv[1]), "provider_id", None)
    if pid:
        rec = load_providers().by_id.get(pid)
        print(getattr(rec, "cli", "") or "")
except Exception:
    pass' "$1" 2>/dev/null || true
}

if [[ "$HANDOFF_MODE" -eq 1 || "$DISCUSS_MODE" -eq 1 ]]; then
  # Prose handoffs and discussions run on the verified first-class CLIs. Honor
  # explicit -> config -> claude routing within that allowlist. A user's explicit
  # unsupported choice is an error; unrelated configured providers fall back.
  prose_mode="handoff"; [[ "$DISCUSS_MODE" -eq 1 ]] && prose_mode="discuss"
  if [[ -n "$PROVIDER" ]]; then
    if ! is_valid_provider "$PROVIDER"; then
      emit_error "invalid provider '$PROVIDER'; valid: ${VALID_PROVIDERS// /, }"
    fi
    case "$PROVIDER" in
      claude|codex|gemini) provider="$PROVIDER" ;;
      *) emit_error "$prose_mode supports providers claude, codex, gemini; you passed --provider $PROVIDER" ;;
    esac
  else
    provider="$(resolve_from_config "$agent_name" | head -1 | tr -d '[:space:]' || true)"
    case "$provider" in
      claude|codex|gemini) : ;;
      *) provider="claude" ;;
    esac
  fi
elif [[ -n "$PROVIDER" ]]; then
  if ! is_valid_provider "$PROVIDER"; then
    emit_error "invalid provider '$PROVIDER'; valid: ${VALID_PROVIDERS// /, }"
  fi
  provider="$PROVIDER"
else
  provider="$(resolve_from_config "$agent_name" | head -1 | tr -d '[:space:]')"
  is_valid_provider "$provider" || provider="claude"
fi

# --yolo is the codex/gemini full-auto bypass (maps to codex
# --dangerously-bypass-approvals-and-sandbox / gemini --yolo inside `fno agents
# spawn`/`host`). claude has no such flag, so do NOT forward an unknown flag to a
# claude `ask`: drop it and warn on stderr (stderr keeps the key=value stdout
# clean). Sandboxed is the default - --yolo is honored only when explicitly passed.
if [[ "$YOLO" -eq 1 && "$provider" == "claude" ]]; then
  printf 'warning: --yolo is not supported for claude; ignoring it\n' >&2
  YOLO=0
fi

# ---- 5. payload mode + provider-aware message assembly -----------------------
# Three modes (Locked Decision 8) replace the old slash-or-`/target` binary:
#   passthrough: payload is an explicit slash command (leading `/`). Forwarded
#                verbatim. Meaningful for claude ONLY - codex/gemini have no slash
#                commands, so a passthrough to them is REFUSED (never an inert
#                string). (AC4-ERR)
#   ask:         the skill parsed an `ask`/`bare` verb (--ask). The prompt is sent
#                VERBATIM - no `/target` wrap, no no-merge, no build brief. (AC4-HP)
#   build:       default. PROVIDER-AWARE: claude gets `/target <text>` (+ no-merge);
#                codex/gemini get a build BRIEF in prose - never a literal `/target`,
#                which they cannot interpret (Locked Decision 9).
# An explicit `ask`/`bare` verb WINS over leading-`/` passthrough detection: a
# question that happens to start with `/` (e.g. `ask "/target what does it do"`)
# is still a verbatim prompt, not a slash command to forward.
if [[ "$HANDOFF_MODE" -eq 1 ]]; then
  payload_mode="handoff"
elif [[ "$DISCUSS_MODE" -eq 1 ]]; then
  payload_mode="discuss"
elif [[ "$ASK_MODE" -eq 1 ]]; then
  payload_mode="ask"
else
  case "$msg" in
    /*) payload_mode="passthrough" ;;
    *)  payload_mode="build" ;;
  esac
fi

case "$payload_mode" in
  passthrough)
    if [[ "$provider" != "claude" ]]; then
      emit_error "$provider has no slash commands; '$msg' cannot be dispatched as a passthrough (use 'ask <prompt>' for a one-shot question, or a bare feature description for a build)"
    fi
    message="$msg"
    ;;
  ask)
    message="$msg"
    ;;
  handoff)
    # Provider-neutral continuation seed: the doc IS the plan; do not re-derive it.
    # The standing GUARDRAIL keeps a fire-from-phone continuation from
    # autonomously taking outward/irreversible actions (prompt-level in v1; the
    # harness-level gate is a deferred follow-up). NO /target, NO no-merge token.
    message="You are continuing work handed off from another session. Read the handoff document at ${msg} in full - it is your plan, state, and context. Continue the work it describes from where it left off. Do NOT re-derive a plan or re-run discovery; the document already contains the plan.

GUARDRAIL: Do not autonomously perform outward-facing or irreversible actions (sending emails or messages, deploying, merging, publishing, deleting external resources, contacting third parties). When the work calls for one, STOP, do not perform it, and surface it with <help reason=\"outward-action\" evidence=\"...\"> for explicit human confirmation; resume only when a human sends approval. Internal/local work (reading and editing files, running tests, committing to a branch, opening a pull request for review) proceeds normally.

If the work includes code changes, land them as a pull request for review; do not merge."
    ;;
  discuss)
    # A provider-native interactive pane. The seed is the opening turn, sent
    # VERBATIM (no /target or build framing).
    message="$msg"
    ;;
  build)
    if [[ "$provider" == "claude" ]]; then
      message="/target $msg"
    else
      # codex/gemini build BRIEF - prose, NEVER a literal `/target`. The PR-review
      # (no-merge) intent becomes an instruction; --allow-merge drops it. The brief
      # is the codex/gemini analogue of claude's `/target ... no-merge`.
      if [[ "$ALLOW_MERGE" -eq 0 ]]; then
        pr_clause="open a pull request for review; do not merge it"
      else
        pr_clause="open a pull request"
      fi
      if [[ -n "$NODE" ]]; then
        message="Implement backlog node $NODE following the conventions in AGENTS.md (run \`fno backlog get $NODE\` for the spec). Commit your work and $pr_clause."
      else
        message="Implement the following, per the conventions in AGENTS.md, then commit and $pr_clause: $msg"
      fi
    fi
    ;;
esac

# no-merge default for any claude `/target ...` message (build-wrapped OR an
# explicit /target passthrough): a fire-and-forget worker should land a PR for
# review, not auto-merge to main from a fat-fingered tap. --allow-merge or an
# already-present no-merge opts out. codex/gemini briefs carry the equivalent
# "do not merge" prose above instead, so this only ever touches the claude path.
# ONLY build/passthrough get this: a verbatim ask/discuss prompt or a handoff
# continuation seed that happens to contain `/target` is NOT a build command
# (sigma-review finding 4), so it must never be no-merge'd.
case "$payload_mode" in
  build|passthrough)
    case "$message" in
      /target\ *|/target)
        if [[ "$ALLOW_MERGE" -eq 0 && " $message " != *" no-merge "* ]]; then
          message="$message no-merge"
        fi
        ;;
    esac
    ;;
esac

# ---- emit --------------------------------------------------------------------
printf 'status=ok\n'
printf 'node=%s\n' "$NODE"
# Resolution-tier classification (ab-f82e8083). The SKILL reads these to resolve
# the id-free entry modes: node_query is a slug candidate (resolve via `fno
# backlog get`, fall through to describe-it on miss); spawn_next asks for the
# top ready node via `fno backlog next`; next_scope is project (default) or all.
printf 'node_query=%s\n' "$NODE_QUERY"
printf 'spawn_next=%s\n' "$SPAWN_NEXT"
printf 'next_scope=%s\n' "$NEXT_SCOPE"
# Cross-project target (-P/--project). Both empty when no --project was passed.
# project is the canonical registry name; resolved_cwd is its abs work-map root,
# which the SKILL passes to `spawn.sh --cwd` and echoes in the REPORT receipt.
printf 'project=%s\n' "$PROJECT_CANON"
printf 'resolved_cwd=%s\n' "$RESOLVED_CWD"
# shape_hint (path|question|continue|feature): the SKILL announces the /target
# build wrap + offers handoff/discuss when this is not `feature` and no explicit
# verb was typed (attended callers only). Emitted on every run.
printf 'shape_hint=%s\n' "$shape_hint"
printf 'name=%s\n' "$agent_name"
printf 'provider=%s\n' "$provider"
printf 'model=%s\n' "$MODEL"
# Spawn flags the SKILL forwards to spawn.sh (permission-mode/role/timeout are
# value flags, empty when unset; fresh/here are 0|1). Value validation lives in
# `fno agents spawn` (fail-closed), not here - these pass through opaquely.
printf 'permission_mode=%s\n' "$PERMISSION_MODE"
printf 'role=%s\n' "$ROLE"
printf 'timeout=%s\n' "$TIMEOUT"
printf 'fresh=%s\n' "$FRESH"
printf 'here=%s\n' "$HERE"
printf 'mode=%s\n' "$MODE"
# x-2c27: the spawn substrate (empty=pane default). The SKILL forwards a
# non-empty value to `spawn.sh --substrate`; bg -> claude --bg thread,
# headless -> one-shot (claude -p / codex --exec / agy -p).
printf 'substrate=%s\n' "$SUBSTRATE"
printf 'yolo=%s\n' "$YOLO"
printf 'yes=%s\n' "$YES"
# allow_merge is emitted deterministically (not re-derived from message prose) so
# the confirm-decision merge-grant caveat reads a first-class field, never the
# absence of a "no-merge" / "do not merge" string on the silent-launch path.
printf 'allow_merge=%s\n' "$ALLOW_MERGE"
printf 'payload_mode=%s\n' "$payload_mode"
printf 'message=%s\n' "$message"
