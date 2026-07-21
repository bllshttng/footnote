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
#   normalize.sh --input "<raw payload>" [--name <n>] [--provider <p>] [--allow-merge|--no-merge]
#
# --allow-merge / --no-merge: per-run merge posture (x-4391). Neither => posture
#   from config.dispatch.auto_merge (default false = no-merge; fno absent => false).
#
# Emits key=value lines on stdout (one per line; values are NOT shell-quoted -
# read them line by line, never `eval`):
#   status=ok | status=error
#   error=<message>            (only when status=error)
#   node=<id>                  (empty when the payload is a free-form feature)
#   node_bare=0|1              (1 = the payload was nothing but the id)
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

# Every regex and tr range here is ASCII, and the smart-quote/em-dash handling
# uses literal octal bytes - byte-wise matching is what those want. Without this,
# BSD sed/tr abort with "RE error: illegal byte sequence" on a payload carrying
# bytes invalid in UTF-8.
export LC_ALL=C

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
EFFORT=""          # reasoning effort forwarded to `fno agents spawn --effort`.
                   # Dashless: `effort <value>`; the CLI validates per provider.
EFFORT_SET=0       # 1 = explicit --effort was passed, including an empty value.
# x-4391 tri-state: "" = unset (resolve from config.dispatch.auto_merge after
# arg parse); 1 = allow (--allow-merge / dashless `merge`); 0 = no-merge
# (--no-merge). Resolved to 0/1 before any read, so `allow_merge=` never emits "".
ALLOW_MERGE=""
YES=0              # 1 = -y/--yes: skip the confirm (consumed by the SKILL policy)
MODE="exec"        # exec | interactive  (-i routes codex/gemini -> host)
SUBSTRATE=""       # x-2c27: ""|bg|headless trailing-posture word (the spawn
                   # substrate axis). Empty = the default `pane` (owned-PTY).
YOLO=0             # 1 = full-auto (codex/gemini bypass); sandboxed default
HANDOFF_MODE=0     # 1 = `handoff` verb: payload is a doc path -> continuation seed
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
ADD_DIR=""         # x-b6e2: extra writable dir; forwarded to spawn.sh --add-dir
AGENT=""           # x-b6e2: sub-agent name; forwarded to spawn.sh --agent
TOOLS=""           # x-b6e2: allowed-tools list; forwarded to spawn.sh --tools
DENY_TOOLS=""      # x-b6e2: disallowed-tools list; forwarded to spawn.sh --deny-tools

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
    --effort)         EFFORT="${2:-}"; EFFORT_SET=1; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --permission-mode) PERMISSION_MODE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -r|--role)        ROLE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -t|--timeout)     TIMEOUT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --add-dir)        ADD_DIR="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --agent)          AGENT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --tools)          TOOLS="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --deny-tools)     DENY_TOOLS="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --fresh)          FRESH=1; shift ;;
    --here|--in-place) HERE=1; shift ;;
    -m|--allow-merge) ALLOW_MERGE=1; shift ;;
    --no-merge)       ALLOW_MERGE=0; shift ;;
    -y|--yes)         YES=1; shift ;;
    -i|--interactive) MODE="interactive"; shift ;;
    -Y|--yolo)        YOLO=1; shift ;;
    --handoff)        HANDOFF_MODE=1; shift ;;
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
# invariant). handoff (a doc path) is a verbatim continuation seed, so it skips
# this scan entirely - a node-shaped or posture-shaped token in the doc path
# must never be consumed as posture. A free-text seed still runs the scan: the
# posture words (provider/substrate/name/...) are session-launch axes orthogonal
# to whether the payload builds or seeds, so `spawn "talk it over" codex` still
# routes the seed to codex.
if [[ "$HANDOFF_MODE" -eq 0 ]]; then
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
    if (( _i >= 1 )) && [[ "$(printf '%s' "${_toks[$((_i-1))]}" | tr '[:upper:]' '[:lower:]')" == "effort" ]]; then
      [[ -z "$EFFORT" ]] && EFFORT="$_t"
      _end=$(( _i - 1 ))
      continue
    fi
    _lt=$(printf '%s' "$_t" | tr '[:upper:]' '[:lower:]')
    # -Y matches the RAW token: lowercased it would collide with -y (--yes),
    # which must keep refusing loud via the flag-lookalike guard below.
    if [[ "$_t" == "-Y" ]]; then YOLO=1; _end=$_i; continue; fi
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
      effort)              emit_error "'effort' is a keyword with no value after it; write 'effort <value>' or drop it" ;;
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
  # then starts with the bareword (not '/'), so the /command it fronts is buried
  # inside a verbatim seed instead of dispatched. Refuse with the corrective
  # trailing form instead.
  #   - Scoped to a /-led remainder: genuine feature prose that merely begins with
  #     the word (`headless browser screenshots`, `bg worker cleanup`) still seeds
  #     normally - only a clear mis-ordered dispatch is refused (PR #106 codex P2).
  #   - Exact-token, case-insensitive (matches the trailing parser's tr at l.150,
  #     so a mobile-auto-capitalized `BG` is caught; `bgcolor`/`background` are not).
  #   - Inside the HANDOFF==0 block, so a verbatim handoff doc path beginning with
  #     the literal word "bg" is exempt (its posture words are never parsed).
  _first_lc="$(printf '%s' "${msg%%[[:space:]]*}" | tr '[:upper:]' '[:lower:]')"
  case "$_first_lc" in
    bg|headless)
      _rest="${msg#"${msg%%[[:space:]]*}"}"; _rest="${_rest#"${_rest%%[![:space:]]*}"}"  # trim
      if [[ "$_rest" == /* ]]; then
        emit_error "posture words are trailing, not leading: write the dispatch first then the substrate, e.g. 'spawn ${_rest} ${_first_lc}'. (A leading '${_first_lc}' would otherwise bury the '${_rest%%[[:space:]]*}' command inside a verbatim seed instead of dispatching it.)"
      fi
      ;;
  esac
fi

# ---- 1b. defensive flag-vocabulary scan (phone-mangled flag in prose) --------
# If a known flag token survived in the task text (the LLM rescue layer missed
# it, or the operator typed it inline), fail loud rather than let it fold
# silently into the payload. Whitespace-delimited, token-initial dash forms
# only: feature prose with unrelated dash tokens ("support -v verbose") and
# glued tokens ("tooltip--yes") pass untouched. The scan canonicalizes a COPY of
# each token's leading em/en-dash to `--`; the message keeps the original text
# verbatim. `set -f` disables globbing so an unquoted `*` in the task text cannot
# expand to filenames during the word-split. (ab-27541df5 US1, Locked Decision 5)
# The scan applies ONLY to a dispatched command - a passthrough (leading `/`) or
# a node-id build - where a flag glued into the payload would corrupt the
# /target-family command that runs. A free-text SEED (x-cbb0) is sent VERBATIM as
# the session's opening turn, so a flag-shaped token in it ("what does grep -i
# do") is conversational content, not a mangled dispatch flag - exempt, exactly as
# the retired `ask` verb and `handoff` are. The node-id check mirrors the tier-1
# detection below (inline here because node detection runs after this); a tier-2
# candidate is scanned on the SKILL's re-normalize pass, once it matches tier 1.
_scan_ft="${msg%%[[:space:]]*}"
if [[ "$HANDOFF_MODE" -eq 0 ]] && { [[ "$msg" == /* ]] || printf '%s' "$_scan_ft" | grep -qE '^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$'; }; then
  set -f
  for scan_tok in $msg; do
    scan_cano="$scan_tok"
    case "$scan_cano" in
      "$EMDASH"*) scan_cano="--${scan_cano#"$EMDASH"}" ;;
      "$ENDASH"*) scan_cano="--${scan_cano#"$ENDASH"}" ;;
    esac
    case "$scan_cano" in
      -y|--yes|-m|--allow-merge|--no-merge|-n|--name|-i|--interactive|-Y|--yolo|--provider|--model|--effort|-P|--project|-f|--force|--permission-mode|-r|--role|-t|--timeout|--fresh|--here|--in-place|--add-dir|--agent|--tools|--deny-tools)
        emit_error "the task text contains a token that looks like a dispatch flag ('$scan_tok') - refusing so it cannot fold silently into the payload. Pass it as a real flag (-y / -m / -n N) separated from the task text (on a phone use the single-dash short form: iOS turns a typed -- into a long dash), or quote/rephrase it if it is genuinely part of the task text."
        ;;
    esac
  done
  set +f
fi
set +f

# x-4391: merge posture is resolved AFTER cross-project cwd resolution (section
# 2c below) so a `-P/--project` spawn reads the TARGET project's config, not the
# caller's (codex P2). See the resolution block just after RESOLVED_CWD.

# ---- 2. node detection + resolution-tier classification ----------------------
# A backlog node id matches `^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$` (tier 1, exact) -
# the shape parse-claims-arg.sh and graph-resolve.sh already use, so any
# configured id_prefix/id_hex_width classifies, not just `ab-`. Two id-free entry
# modes layer on top (ab-f82e8083):
#   tier 2 - a single slug-shaped token is a slug CANDIDATE the SKILL resolves
#            via `fno backlog get`, falling through to describe-it on a miss.
#            Bare hex rides this lane too: the resolver is format-agnostic and
#            accepts it, which beats guessing a prefix here.
#   tier 5 - `next` / `next all` asks for the top ready node; the SKILL resolves
#            it via `fno backlog next` (this-project default; `all` widens).
# Slug + next resolution need the graph, so normalize only CLASSIFIES them here
# (deterministic + unit-testable); the SKILL does the lookup and re-normalizes
# with the resolved id. Termination invariant: a canonical id from the resolver
# MUST match tier 1, or the re-normalize would reclassify as tier 2 forever. The
# describe-it fuzzy tier (tier 4) is whatever is left - free prose - and lives
# entirely in the SKILL body behind a confirm.
NODE=""
NODE_BARE=0
NODE_QUERY=""
SPAWN_NEXT=0
NEXT_SCOPE=""
first_tok="${msg%%[[:space:]]*}"
msg_lc="$(printf '%s' "$msg" | tr '[:upper:]' '[:lower:]')"
if [[ "$HANDOFF_MODE" -eq 1 ]]; then
  :   # handoff (a doc path) carries no node id; a node-shaped token in the doc
      # path must NOT be resolved as a node.
elif [[ "$msg_lc" == "next" || "$msg_lc" == "next all" ]]; then
  SPAWN_NEXT=1
  [[ "$msg_lc" == "next all" ]] && NEXT_SCOPE="all" || NEXT_SCOPE="project"
elif printf '%s' "$first_tok" | grep -qE '^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$'; then
  NODE="$first_tok"                                   # tier 1: exact node id
  # a-f are letters, so this shape also matches hyphen-joined English
  # ("re-added", "dead-beef"). Whether the payload is NOTHING BUT the id is the
  # only deliberate-naming signal available without a graph read; VALIDATE uses
  # it to decide refuse-loud vs degrade-to-seed on a resolution miss.
  [[ "$msg" == "$first_tok" ]] && NODE_BARE=1
elif printf '%s' "$msg" | grep -qE '^/target([[:space:]]|$)'; then
  # Unanchored, so a hex-shaped prose word ("re-added") can match; the SKILL's
  # VALIDATE arm degrades a passthrough miss to node="" rather than refusing.
  NODE="$(printf '%s' "$msg" | grep -oE '[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}' | head -1)"
elif printf '%s' "$msg" | grep -iqE '^[a-z0-9][a-z0-9-]*$'; then
  # tier 2: slug candidate. Case-insensitive (`-i`) so a mobile-auto-capitalized
  # slug (`Dashless-spawn`) is still classified as a candidate; the resolver
  # (`fno backlog get` -> resolve_node) matches slugs case-insensitively.
  NODE_QUERY="$msg"
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
if [[ "$EFFORT_SET" -eq 1 && -z "$EFFORT" ]]; then
  emit_error "--effort requires a value (got an empty value)"
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

# x-4391: resolve merge posture when no explicit flag/word set it. Rung 2 =
# config.dispatch.auto_merge; rung 3 = builtin no-merge. Read from the TARGET
# project's cwd when a -P/--project cross-project spawn resolved one (RESOLVED_CWD),
# so a caller repo's opt-in never leaks to a project that opted out, and vice
# versa (codex P2). `fno config get` prints a Python bool (`True`/`False`) and has
# no cwd flag, so cd in a subshell then lowercase before the exact-`true` compare.
# fno absent, a stale binary rejecting the key, or any error degrades to no-merge
# (matches this file's provider-fallback degrade; never grant merge on a failed read).
if [[ -z "$ALLOW_MERGE" ]]; then
  ALLOW_MERGE=0
  if command -v fno >/dev/null 2>&1; then
    _am="$( ( [[ -n "$RESOLVED_CWD" ]] && cd "$RESOLVED_CWD" 2>/dev/null; fno config get dispatch.auto_merge 2>/dev/null ) | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]' || true)"
    [[ "$_am" == "true" ]] && ALLOW_MERGE=1
  fi
fi

# ---- 3. agent-name derivation ------------------------------------------------
# Explicit --name wins (sanitized). Otherwise the name is provenance-carrying so
# the thread title reads at a glance: <verb>-<full-node-id>-<slug> for a node
# (e.g. spawn-ab-4040eee8-cargo-bootstrapper), <verb>-<slug> for a free-form
# feature, falling back to a deterministic CRC short-id when the slug is empty.
# The leading <verb> is the launching verb (spawn | handoff), never a fixed
# string, so a self-spawned worker is distinguishable from a handoff thread.
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
[[ "$HANDOFF_MODE" -eq 1 ]] && verb="handoff"

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

if [[ "$HANDOFF_MODE" -eq 1 ]]; then
  # A prose handoff runs on the verified first-class CLIs. Honor explicit ->
  # config -> claude routing within that allowlist. A user's explicit unsupported
  # choice is an error; unrelated configured providers fall back.
  if [[ -n "$PROVIDER" ]]; then
    if ! is_valid_provider "$PROVIDER"; then
      emit_error "invalid provider '$PROVIDER'; valid: ${VALID_PROVIDERS// /, }"
    fi
    case "$PROVIDER" in
      claude|codex|gemini) provider="$PROVIDER" ;;
      *) emit_error "handoff supports providers claude, codex, gemini; you passed --provider $PROVIDER" ;;
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

# --yolo is the full-auto / no-gates bypass. codex/gemini have a literal --yolo
# (codex --dangerously-bypass-approvals-and-sandbox / gemini --yolo inside `fno
# agents spawn`/`host`), so YOLO stays 1 for them. claude has NO --yolo flag; its
# "full auto, no gates" equivalent is --permission-mode bypassPermissions (x-dfa4:
# default|acceptEdits|plan|bypassPermissions). Map it there rather than dropping
# it, so a yolo'd claude bg worker actually runs gate-free instead of stalling on
# a permission prompt. An explicit --permission-mode the user passed WINS (never
# clobber it); then clear YOLO so the claude spawn is never handed an unknown
# --yolo flag. Forwarded via permission_mode= (the x-019d plumbing).
if [[ "$YOLO" -eq 1 && "$provider" == "claude" ]]; then
  [[ -z "$PERMISSION_MODE" ]] && PERMISSION_MODE="bypassPermissions"
  YOLO=0
fi

# ---- 5. payload mode + provider-aware message assembly -----------------------
# `spawn` means start a session with what you pass, nothing more (x-cbb0). Four
# modes, no shape inference:
#   passthrough: payload is an explicit slash command (leading `/`). Rendered
#                per-harness: claude/agy verbatim, opencode `/fno:verb`, codex
#                `$fno:verb`; a deprecated (refused) provider has no dispatch
#                lane, so a passthrough to it is REFUSED naming agy. (AC4-ERR)
#   build:       a resolved backlog node id. The ONE surviving implicit `/target`,
#                config-driven not shape-inferred: dispatch_verb resolution
#                (node > config > builtin `/target`) - normalize uses the builtin
#                rung, wrapping `/target <id>` PROVIDER-AWARE (opencode
#                `/fno:target`, codex `$fno:target`) + no-merge.
#   seed:        default for free text. Sent VERBATIM as the session opening seed
#                on the default pane - no `/target` wrap, no no-merge, no build
#                framing (what `--discuss` produced before the wrap was cut).
#   handoff:     `--handoff`, a doc path -> continuation seed + guardrail.
# passthrough (leading `/`) wins over the node-id build so an explicit `/target
# ab-xxxx` renders as a passthrough, not a re-wrapped build.
if [[ "$HANDOFF_MODE" -eq 1 ]]; then
  payload_mode="handoff"
else
  case "$msg" in
    /*) payload_mode="passthrough" ;;
    *)
      if [[ -n "$NODE" ]]; then
        payload_mode="build"     # node-id dispatch (implicit /target, config rung)
      else
        payload_mode="seed"      # free text -> verbatim session seed
      fi
      ;;
  esac
fi

# Command surface (slash|codex-skill|prose) from the harness-map normalizer
# (fno.agents.harness_map), the single source both dispatch surfaces route
# through - so /agent spawn never re-encodes the per-harness spelling and can't
# drift from `/target bg`. `fno dispatch resolve` is authoritative; a static
# fallback keeps a spawn working if fno is unreachable (mirrors resolve_project).
resolve_command_surface() {
  local _prov="$1" _line
  _line="$(fno dispatch resolve --harness "$_prov" 2>/dev/null | sed -n 's/^command_surface=//p' | head -1)"
  if [[ -n "$_line" ]]; then printf '%s' "$_line"; return 0; fi
  # Static mirror of fno.agents.harness_map (a python test asserts parity):
  # opencode's fno plugin exposes `/fno:verb` (palette + `run --command`), so it
  # is a slash surface; gemini is deprecated -> refused. Unknown -> refused too,
  # never a silent prose no-op.
  case "$_prov" in
    claude|agy|opencode) printf 'slash' ;;
    codex)               printf 'codex-skill' ;;
    gemini)              printf 'refused' ;;
    *)                   printf 'refused' ;;
  esac
}

# The plugin-namespace prefix a slash-surface provider prepends to `/verb`
# (mirrors harness_map's slash_prefix): opencode -> `fno:` (`/fno:target`);
# claude/agy inject natively, so it is empty. Keep in sync with harness_map.
slash_prefix() {
  case "$1" in
    opencode) printf 'fno:' ;;
    *)        printf '' ;;
  esac
}

case "$payload_mode" in
  passthrough)
    # A leading-`/` footnote command. claude/agy run it verbatim; opencode
    # namespaces it (`/verb` -> `/fno:verb`, plugin palette); codex swaps
    # `/verb` -> `$fno:verb` (plugin skill); a refused (deprecated) provider has
    # no dispatch lane, so refuse loudly naming its successor (agy).
    surface="$(resolve_command_surface "$provider")"
    case "$surface" in
      slash)
        # Prefix-swap, idempotent: a command already in the native namespaced
        # form (`/fno:target`, e.g. copied from opencode's palette) must NOT be
        # double-prefixed to `/fno:fno:target` (mirrors normalize_command).
        _prefix="$(slash_prefix "$provider")"
        if [[ -n "$_prefix" && "$msg" == "/$_prefix"* ]]; then
          message="$msg"
        else
          message="/${_prefix}${msg#/}"
        fi
        ;;
      codex-skill) message="\$fno:${msg#/}" ;;
      *)           emit_error "$provider is deprecated (successor: agy) and has no dispatch lane; '$msg' cannot be dispatched there (route to a claude/codex/opencode/agy harness)" ;;
    esac
    ;;
  seed)
    # Free text sent VERBATIM as the session opening seed (default pane). No
    # /target wrap, no build framing (what --discuss produced before x-cbb0). A
    # seed beginning with `/` never reaches here - a leading `/` is passthrough.
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
  build)
    # A resolved node id ($msg is the ab-id). PROVIDER-AWARE via the normalizer
    # surface: claude/agy invoke the native `/target` skill; opencode invokes
    # `/fno:target` (plugin palette); codex invokes `$fno:target` - all run the
    # REAL pipeline on the node. A refused (deprecated) provider has no skill
    # surface, so refuse loudly naming agy.
    surface="$(resolve_command_surface "$provider")"
    case "$surface" in
      slash)       message="/$(slash_prefix "$provider")target $msg" ;;
      codex-skill) message="\$fno:target $msg" ;;
      *)           emit_error "$provider is deprecated (successor: agy) and has no build lane; route this build to a claude/codex/opencode/agy harness (no prose brief is generated)" ;;
    esac
    ;;
esac

# no-merge default for any native `/target`-family message (node-id build OR an
# explicit /target passthrough): a fire-and-forget worker should land a PR for
# review, not auto-merge to main from a fat-fingered tap. --allow-merge or an
# already-present no-merge opts out. Covers claude `/target`, opencode
# `/fno:target`, and codex `$fno:target`; a refused provider never reaches here.
# ONLY build/passthrough get this: a verbatim seed or a handoff continuation seed
# that happens to contain `/target` is NOT a build command (sigma-review finding
# 4), so it must never be no-merge'd.
case "$payload_mode" in
  build|passthrough)
    # A native /target-family invocation (claude/agy `/target`, opencode
    # `/fno:target`, codex `$fno:target`) is no-merge'd. Single-quote the `$fno:`
    # literal so it is not read as a variable.
    case "$message" in
      /target\ *|/target|/fno:target\ *|/fno:target|'$fno:target '*|'$fno:target')
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
# 1 only when the payload was NOTHING but the id, so VALIDATE can tell a
# deliberately-named node from one inferred out of prose.
printf 'node_bare=%s\n' "$NODE_BARE"
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
printf 'name=%s\n' "$agent_name"
printf 'provider=%s\n' "$provider"
printf 'model=%s\n' "$MODEL"
# Spawn flags the SKILL forwards to spawn.sh (permission-mode/role/timeout are
# value flags, empty when unset; fresh/here are 0|1). Value validation lives in
# `fno agents spawn` (fail-closed), not here - these pass through opaquely.
printf 'permission_mode=%s\n' "$PERMISSION_MODE"
printf 'role=%s\n' "$ROLE"
printf 'timeout=%s\n' "$TIMEOUT"
# x-b6e2: Tier-3 harness-native passthrough (empty when unset). spawn.sh forwards
# a non-empty value; `fno agents spawn` maps or fails closed per provider.
printf 'add_dir=%s\n' "$ADD_DIR"
printf 'agent=%s\n' "$AGENT"
printf 'tools=%s\n' "$TOOLS"
printf 'deny_tools=%s\n' "$DENY_TOOLS"
printf 'fresh=%s\n' "$FRESH"
printf 'here=%s\n' "$HERE"
printf 'effort=%s\n' "$EFFORT"
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
