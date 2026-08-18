#!/usr/bin/env bash
# Plan pre-check validator for target
# Usage: validate-plan.sh <plan.md>
# Exit: 0 = valid (may have warnings), 1 = errors found

set -euo pipefail

PLAN_DIR="${1:?Usage: validate-plan.sh <plan.md>}"
ERRORS=0
WARNINGS=0
TMPDIR_BASE_VAL="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE_VAL"' EXIT

# New single-doc plans carry either the canonical Execution Strategy YAML or
# the explicit quick-plan kind. Their executable contract is validated by the
# Python authority; the heading-oriented checks below remain only for legacy
# plans that predate the single-doc shape.
_is_quick_plan() {
    awk '/^---/{c++; if(c==2) exit; next} c==1{print}' "$PLAN_DIR" 2>/dev/null \
        | grep -qE "^[[:space:]]*kind:[[:space:]]*['\"]?quick-plan['\"]?([[:space:]]*(#.*)?)?$"
}

_has_execution_strategy() {
    awk '
        /^##[[:space:]]+Execution Strategy[[:space:]]*$/ { found=1; exit }
        END { exit(found ? 0 : 1) }
    ' "$PLAN_DIR" 2>/dev/null
}

SEMANTIC_SINGLE_DOC=0
if [[ -f "$PLAN_DIR" ]] && { _is_quick_plan || _has_execution_strategy; }; then
    SEMANTIC_SINGLE_DOC=1
fi

error() { echo "  ERROR: $*"; ((ERRORS++)) || true; }
warn()  { echo "  WARN:  $*"; ((WARNINGS++)) || true; }
ok()    { echo "  OK:    $*"; }

# Echo a plan's readiness rung, or `!<reason>` when the verb could not answer.
# Never classifies statuses itself; the single authority is `fno plan rung`
# (fno.graph.ladder).
#
# The reason rides in the return VALUE rather than a global, because every
# caller reads this through `$(...)` - a global set inside that subshell dies
# with it, and the caller silently prints an empty reason.
#
# Silence is reported, never swallowed. An installed `fno` predating the verb
# exits 2 with no `rung=` line, indistinguishable from any other non-answer at
# this layer - and a check that quietly passes on a stale binary is exactly the
# decorative guard this consolidation exists to remove.
#
# SOURCE FIRST, for the same reason `_semantic_validate` resolves that way: an
# installed `fno` older than this checkout does not have verbs this source
# defines. CI is exactly that case - it runs the repo's scripts against whatever
# `fno` is on PATH - so an installed-only lookup degrades to a warning there and
# the check never actually runs. Falls back to the installed binary so the
# script still works from a plugin install with no source tree.
_plan_rung() {
    local _out="" _src=""
    _src="$(_fno_source_python)"
    set +o pipefail
    if [[ -n "$_src" ]]; then
        local _py="${_src%%|*}" _root="${_src##*|}"
        _out="$(PYTHONPATH="$_root/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
            "$_py" -m fno.cli plan rung "$1" 2>/dev/null \
            | sed -n 's/^rung=//p' | head -1)"
    fi
    if [[ -z "$_out" ]] && command -v fno >/dev/null 2>&1; then
        _out="$(fno plan rung "$1" 2>/dev/null | sed -n 's/^rung=//p' | head -1)"
    fi
    set -o pipefail
    if [[ -z "$_out" ]]; then
        printf "!no fno CLI could answer 'plan rung' (installed fno may predate it; run 'fno update' or 'fno doctor --fix')"
        return 0
    fi
    printf '%s' "$_out"
}

# Echo the checkout root that holds `cli/src/fno`, or nothing. Three callers
# asked this the same way in three places, so it is one function.
_fno_source_root() {
    local repo_root="" script_dir="" candidate=""
    repo_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    for candidate in \
        "$repo_root" \
        "$script_dir/.." \
        "$script_dir/../../.."; do
        if [[ -n "$candidate" && -d "$candidate/cli/src/fno" ]]; then
            (cd "$candidate" && pwd)
            return 0
        fi
    done
    return 0
}

# Echo the YYYY-MM-DD a plan's own NAME carries, or nothing. /blueprint names
# every plan it writes `YYYY-MM-DD-slug.md`, and a folder plan takes the date
# from its directory, so this is real evidence of when a plan was written -
# which is what the consolidation gate needs when frontmatter carries none.
_plan_name_date() {
    local file="$1" name=""
    name=$(basename "$file")
    if [[ "$name" == "00-INDEX.md" ]]; then
        name=$(basename "$(dirname "$file")")
    fi
    # Bounded on purpose: a bare 8-digit prefix is not a date, and reading
    # `12345678-notes.md` as 1234-56-78 would sort after any real gate date and
    # refuse a plan for having a number in its name.
    if [[ "$name" =~ ^(20[0-9]{2})-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])[-._] ]] \
        || [[ "$name" =~ ^(20[0-9]{2})(0[1-9]|1[0-2])(0[1-9]|[12][0-9]|3[01])[-._] ]]; then
        printf '%s-%s-%s' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}"
    fi
}

# Echo `<python>|<source_root>` for a checkout that can import the fno CLI, or
# nothing. Shared by _plan_rung, _semantic_validate, and the consolidation gate
# so the "which fno runs?" question has ONE answer here.
_fno_source_python() {
    local source_root="" python_bin=""
    source_root=$(_fno_source_root)
    [[ -z "$source_root" ]] && return 0
    if [[ -n "${FNO_PYTHON:-}" && -x "${FNO_PYTHON}" ]]; then
        python_bin="$FNO_PYTHON"
    elif [[ -x "$source_root/cli/.venv/bin/python" ]]; then
        python_bin="$source_root/cli/.venv/bin/python"
    elif command -v python3 >/dev/null 2>&1; then
        python_bin=$(command -v python3)
    elif command -v python >/dev/null 2>&1; then
        python_bin=$(command -v python)
    fi
    [[ -z "$python_bin" ]] && return 0
    printf '%s|%s' "$python_bin" "$source_root"
}

_semantic_validate() {
    local source_root="" python_bin="" _src=""
    # Same resolver `_plan_rung` uses - one answer to "which fno runs?".
    _src="$(_fno_source_python)"
    if [[ -n "$_src" ]]; then
        python_bin="${_src%%|*}"
        source_root="${_src##*|}"
    else
        # No usable interpreter, but a source tree may still exist; the uv arm
        # below can still run it, and the refusal message names the root.
        source_root=$(_fno_source_root)
    fi
    if [[ -n "$source_root" ]]; then
        # A source checkout whose interpreter lacks the CLI deps (a fresh
        # worktree has no cli/.venv, so this lands on a bare python3) would
        # otherwise surface an ImportError traceback as a plan violation.
        local source_pythonpath="$source_root/cli/src${PYTHONPATH:+:$PYTHONPATH}" probe_error=""
        # Import what the real invocation imports. `import fno.cli` alone proves
        # only that typer is present: the plan sub-app is loaded lazily and pulls
        # in PyYAML, so a narrower probe passes and the real call still crashes.
        if [[ -n "$python_bin" ]]; then
            probe_error=$(PYTHONPATH="$source_pythonpath" \
                "$python_bin" -c 'import fno.cli, fno.plan.cli, fno.plan.schema, fno.plan.execution_validation' 2>&1) && {
                PYTHONPATH="$source_pythonpath" \
                    "$python_bin" -m fno.cli plan validate "$PLAN_DIR" --execution
                return
            }
        fi
        if command -v uv >/dev/null 2>&1; then
            probe_error=$(uv run --project "$source_root/cli" \
                python -c 'import fno.cli, fno.plan.cli, fno.plan.schema, fno.plan.execution_validation' 2>&1) && {
                uv run --project "$source_root/cli" \
                    python -m fno.cli plan validate "$PLAN_DIR" --execution
                return
            }
        fi
        # Refuse rather than delegate: an installed fno older than this checkout
        # advertises --execution while missing the guards this source defines,
        # so falling back would pass a plan this tree rejects.
        echo "source fno CLI at $source_root/cli/src is not runnable: no usable interpreter" >&2
        echo "run 'uv sync --project $source_root/cli' or set FNO_PYTHON to an interpreter with the CLI deps" >&2
        [[ -n "$probe_error" ]] && echo "last probe error: ${probe_error##*$'\n'}" >&2
        [[ -n "${FNO_PYTHON:-}" ]] && echo "note: FNO_PYTHON=$FNO_PYTHON is a strict override and outranks $source_root/cli/.venv" >&2
        return 2
    fi
    if ! command -v fno >/dev/null 2>&1; then
        echo "fno CLI not found; install or update it before validating executable plans" >&2
        return 2
    fi
    if ! fno plan validate --help 2>&1 | grep -q -- '--execution'; then
        echo "installed fno predates semantic plan validation; run 'fno update' or 'fno doctor --fix'" >&2
        return 2
    fi
    fno plan validate "$PLAN_DIR" --execution
}

echo "Validating plan: $PLAN_DIR"
echo ""

# -------------------------------------------------------------------
# Check 1: Structure
# -------------------------------------------------------------------
echo "--- Structure ---"

# The only authored plan shape is a single .md (G1); folder reading is
# removed (G3).
if [[ -f "$PLAN_DIR" ]]; then
    if [[ "$PLAN_DIR" == *.md ]]; then
        ok "single-doc plan: $(basename "$PLAN_DIR")"
        # Frontmatter that does not parse is the loudest failure this script can
        # give it. Everything downstream fails OPEN by design: _read_plan_frontmatter
        # raises, is_design_stage returns False (correct - plans live in a symlinked
        # vault, so a read failure must not quarantine the backlog), the node skips
        # the design rung and derives `ready`, and `/blueprint --finalize` exits 0
        # having validated nothing. The whole chain's only trace is a warning line
        # that scrolls past. An unquoted `title: verb X: qualifier` is how it
        # happens in practice, since node titles here routinely carry a colon.
        _fm_python=""
        if [[ -n "${FNO_PYTHON:-}" && -x "${FNO_PYTHON}" ]]; then
            _fm_python="$FNO_PYTHON"
        elif command -v python3 >/dev/null 2>&1; then
            _fm_python=$(command -v python3)
        fi
        if [[ -n "$_fm_python" ]] && "$_fm_python" -c 'import yaml' 2>/dev/null; then
            _fm_err=$(awk '/^---/{c++; if(c==2) exit; next} c==1{print}' "$PLAN_DIR" \
                | "$_fm_python" -c 'import sys,yaml
try:
    yaml.safe_load(sys.stdin.read())
except yaml.YAMLError as exc:
    sys.stdout.write(" ".join(str(exc).split())[:200])' 2>/dev/null)
            if [[ -n "$_fm_err" ]]; then
                error "frontmatter is not valid YAML: $_fm_err"
                error "  a title containing a colon must be quoted: title: \"verb X: qualifier\""
            else
                ok "frontmatter parses as YAML"
            fi
        else
            # No PyYAML is a tooling gap, not a plan defect. Say so rather than
            # passing silently, which would read as "the frontmatter is fine".
            warn "frontmatter YAML parse not checked (no python3 with PyYAML)"
        fi
        if awk '/^---/{c++; if(c==2) exit; next} c==1{print}' "$PLAN_DIR" \
                | grep -qE '^[[:space:]]*project:'; then
            ok "has 'project:' field"
        else
            warn "missing 'project:' field in frontmatter (intake will fall back to cwd-based inference)"
        fi
    else
        error "not a .md plan file: $PLAN_DIR"
    fi
else
    error "Plan file not found: $PLAN_DIR"
fi

# -------------------------------------------------------------------
# Check 1b: Group-child stub markers + why-digest (x-edf7 US1/US4)
# -------------------------------------------------------------------
# A `blueprint decompose` child is scaffolded with placeholder stub markers and
# an empty-why sentinel; it is born `status: idea` and MUST be inline-filled (or
# designed by a fan-out /think pass) before it can be dispatched. Linking it is
# no longer the thing that arms it - the `idea` rung is undispatchable on every
# surface now - but an unfilled scaffold that reaches a builder is still wasted
# work, so this check refuses to pass one and the link step (skill body) and
# this validator agree on "filled". Keep STUB_MARKERS in sync with
# cli/src/fno/graph/_decompose.py.
echo ""
echo "--- Stub Markers (decompose child) ---"
if [[ -f "$PLAN_DIR" ]]; then
    STUB_MARKERS=(
        "<!-- Seeded from epic waves"
        "<!-- From the epic's File Ownership Map"
        "<!-- The checks that prove"
        "<!-- Why (from epic):"
        "<!-- Consolidation:"
    )
    _found_stub=0
    for _m in "${STUB_MARKERS[@]}"; do
        if grep -Fq "$_m" "$PLAN_DIR"; then
            error "unfilled stub marker '${_m} ...' in $(basename "$PLAN_DIR"); inline-fill the scaffold before linking plan_path"
            _found_stub=1
        fi
    done

    # A born scaffold sits at the `idea` rung (spelled `stub` before x-3571, still
    # read as `idea`). Refuse to pass one: the fill step must flip it to `ready`,
    # or the linked node derives `idea` and no dispatcher will ever pick it up.
    # The rung comes from `fno plan rung` - this script does not parse `status:`.
    _RUNG="$(_plan_rung "$PLAN_DIR")"
    if [[ "$_RUNG" == "idea" ]]; then
        error "$(basename "$PLAN_DIR") is still at the 'idea' rung; set 'status: ready' after filling"
        _found_stub=1
    elif [[ "$_RUNG" == \!* ]]; then
        warn "plan rung not checked: ${_RUNG#!}"
    fi

    # A group-child plan (frontmatter carries `parent_epic:`) must also carry a
    # non-empty `## Why (from epic)` - the transcribed intent grounds its tasks
    # (US4). Only enforced for group children; a normal quick/full plan has no
    # Why section and is not required to grow one.
    # grep redirected to /dev/null (not -q): under `set -o pipefail`, grep -q can
    # exit early and SIGPIPE the upstream awk (exit 141), failing the pipeline and
    # spuriously skipping the check for a real group child.
    if awk '/^---/{c++; if(c==2) exit; next} c==1{print}' "$PLAN_DIR" \
            | grep -E '^[[:space:]]*parent_epic:' >/dev/null; then
        _why_body="$(awk '
            /^##[ \t]+Why \(from epic\)[ \t]*$/{f=1; next}
            f && /^##?[ \t]/{exit}
            f{print}
        ' "$PLAN_DIR" | grep -vE '^[[:space:]]*(<!--|$)' || true)"
        if [[ -z "$_why_body" ]]; then
            error "group-child plan $(basename "$PLAN_DIR") has an empty '## Why (from epic)'; transcribe the epic's intent + binding Locked Decisions"
        else
            ok "## Why (from epic) is non-empty"
        fi
    fi
    [[ "$_found_stub" -eq 0 ]] && ok "no unfilled stub markers"
fi

# -------------------------------------------------------------------
# Check 2: Execution strategy
# -------------------------------------------------------------------
echo ""
echo "--- Execution Strategy ---"

# execution_mode lives in the plan doc's frontmatter. A single-doc quick
# plan legitimately omits it (no waves).
if [[ -f "$PLAN_DIR" ]] && grep -q "execution_mode:" "$PLAN_DIR" 2>/dev/null; then
    ok "execution_mode defined"
else
    ok "no execution_mode (single-task / quick plan)"
fi

# -------------------------------------------------------------------
# Task-block helper: extract the text of a "### Task N.M" section, bounded
# by the next "### Task" heading or the next "## " (2-hash) section heading,
# whichever comes first, or EOF.
# -------------------------------------------------------------------

_task_block() {
    local start_line="$1"
    awk -v start="$start_line" '
        NR == start { capture=1; next }
        capture && (/^### Task/ || /^## /) { exit }
        capture { print }
    ' "$PLAN_DIR"
}

# -------------------------------------------------------------------
# Check 3: Task completeness
# -------------------------------------------------------------------
echo ""
echo "--- Task Completeness ---"

TASK_HEADINGS_RAW=""
if [[ -f "$PLAN_DIR" ]]; then
    TASK_HEADINGS_RAW=$(grep -n '^### Task' "$PLAN_DIR" 2>/dev/null || true)
fi

if [[ "$SEMANTIC_SINGLE_DOC" -eq 1 ]]; then
    semantic_output=$(_semantic_validate 2>&1) && semantic_status=0 || semantic_status=$?
    if [[ $semantic_status -eq 0 ]]; then
        ok "semantic execution contract valid"
    elif [[ $semantic_status -eq 2 ]]; then
        # "The validator could not run" is not "your plan is wrong": callers are
        # told to stop and fix the plan on ERROR, so a broken tool must not
        # arrive wearing that costume.
        echo "  TOOLFAIL: $semantic_output"
        echo ""
        echo "=== Result ==="
        echo "validation could not run -- the plan was NOT judged; fix the tooling above"
        exit 2
    else
        error "$semantic_output"
    fi
elif [[ -z "$TASK_HEADINGS_RAW" ]]; then
    warn "no tasks found (no '### Task' headings)"
else
    while IFS=: read -r lineno heading_rest; do
        [[ -z "$lineno" ]] && continue
        task_name="${heading_rest# }"
        block=$(_task_block "$lineno")

        if ! echo "$block" | grep -q "Acceptance Criteria"; then
            warn "$task_name: missing Acceptance Criteria section"
        else
            ok "$task_name: has Acceptance Criteria"
        fi

        if ! echo "$block" | grep -qE "(Steps:|Step 1:)"; then
            warn "$task_name: missing Steps section"
        else
            ok "$task_name: has Steps"
        fi

        if ! echo "$block" | grep -qiE "^(Files?:|## Files?)"; then
            warn "$task_name: missing Files section"
        else
            ok "$task_name: has Files section"
        fi
    done <<< "$TASK_HEADINGS_RAW"
fi

# -------------------------------------------------------------------
# Check 4: Parallel wave file conflicts
# -------------------------------------------------------------------
echo ""
echo "--- Parallel Conflict Check ---"

if [[ "$SEMANTIC_SINGLE_DOC" -eq 1 ]]; then
    ok "Parallel ownership validated by semantic execution contract"
elif [[ -f "$PLAN_DIR" ]]; then
    # Find parallel waves in the Execution Strategy YAML: lines like
    # "mode: parallel" followed by tasks. Strategy: extract task IDs listed
    # in parallel waves, then check their Files sections for duplicates.

    # Collect all parallel wave task groups
    # We look for blocks: "mode: parallel" then "tasks: [...]"
    PARALLEL_TASKS_RAW=$(awk '
        /mode: parallel/ { in_parallel=1; next }
        in_parallel && /tasks:/ {
            gsub(/tasks:[ \t]*\[/, "")
            gsub(/\]/, "")
            gsub(/,/, " ")
            print
            in_parallel=0
        }
        /mode:/ { in_parallel=0 }
    ' "$PLAN_DIR")

    if [[ -z "$PARALLEL_TASKS_RAW" ]]; then
        ok "No parallel waves detected — skipping conflict check"
    else
        # For each parallel wave group, extract files used by each task
        CONFLICT_FOUND=0
        while IFS= read -r task_group; do
            [[ -z "$task_group" ]] && continue

            all_files_tmp="$TMPDIR_BASE_VAL/all_files_$$.txt"
            : > "$all_files_tmp"

            for task_id in $task_group; do
                task_id=$(echo "$task_id" | tr -d ' ')
                [[ -z "$task_id" ]] && continue

                # Find this task's own "### Task X.Y" heading and scan its
                # block for file paths.
                task_lineno=$(grep -nE "^### Task ${task_id}([^0-9]|$)" "$PLAN_DIR" | head -1 | cut -d: -f1 || true)
                [[ -z "$task_lineno" ]] && continue

                _task_block "$task_lineno" \
                    | awk '
                        /^(Files?:|## Files?)/ { collecting=1; next }
                        collecting && /^(#|---|\*\*|AC|Step|Acceptance)/ { collecting=0 }
                        collecting && /\.ts|\.tsx|\.js|\.py|\.sh|\.md/ { print $0 }
                    ' | sed 's/^[-* ]*//' | tr -d ' ' >> "$all_files_tmp"
            done

            # Check for duplicates
            if [[ -s "$all_files_tmp" ]]; then
                DUPES=$(sort "$all_files_tmp" | uniq -d)
                if [[ -n "$DUPES" ]]; then
                    error "Parallel wave conflict: same file(s) in multiple parallel tasks: $DUPES"
                    CONFLICT_FOUND=1
                fi
            fi
            rm -f "$all_files_tmp"
        done <<< "$PARALLEL_TASKS_RAW"

        [[ $CONFLICT_FOUND -eq 0 ]] && ok "No file conflicts in parallel waves"
    fi
fi

# -------------------------------------------------------------------
# Check 5: Circular dependency detection
# -------------------------------------------------------------------
echo ""
echo "--- Dependency Check ---"

if [[ "$SEMANTIC_SINGLE_DOC" -eq 1 ]]; then
    ok "Dependency topology validated by semantic execution contract"
elif [[ -f "$PLAN_DIR" ]]; then
    # Extract dependency edges: look for "depends_on:" or "Depends on wave"
    # Simple check: ensure wave numbers in depends_on are always lower
    DEP_ERRORS=0
    while IFS= read -r line; do
        wave_num=$(echo "$line" | grep -oE 'wave: [0-9]+' | grep -oE '[0-9]+' || true)
        dep_num=$(echo "$line" | grep -oE 'depends_on: [0-9]+' | grep -oE '[0-9]+' || true)
        if [[ -n "$wave_num" && -n "$dep_num" ]]; then
            if [[ "$dep_num" -ge "$wave_num" ]]; then
                error "Possible circular/forward dependency: wave $wave_num depends on $dep_num"
                DEP_ERRORS=1
            fi
        fi
    done < "$PLAN_DIR"
    [[ $DEP_ERRORS -eq 0 ]] && ok "No circular dependencies detected"
fi

# -------------------------------------------------------------------
# Check 6: Critical Path Trace (semantic)
# -------------------------------------------------------------------
echo ""
echo "--- Critical Path Trace ---"

if [[ "$SEMANTIC_SINGLE_DOC" -eq 1 ]]; then
    ok "Critical-path prose is optional for semantic single-doc plans"
elif [[ -f "$PLAN_DIR" ]]; then
    if grep -q "^## Critical Path Trace" "$PLAN_DIR" 2>/dev/null; then
        ok "Critical Path Trace section found"

        # Check for scope classification
        # Extract scope from the Scope Classification section only (not the whole file)
        # Note: scope value lives INSIDE a YAML code fence by design, so don't filter fences here
        SCOPE=$(awk '/^## Scope Classification/{found=1; next} found && /^## /{exit} found{print}' "$PLAN_DIR" | grep -oE 'scope: (feature|scaffolding|poc)' | head -1 | awk '{print $2}' || true)
        if [[ -z "$SCOPE" ]]; then
            warn "No scope classification found (add 'scope: feature|scaffolding|poc')"
            SCOPE="unknown"
        else
            ok "Scope: $SCOPE"
        fi

        # Check for unresolved stubs in critical path
        # Only scan lines between "## Critical Path Trace" and the next "## " heading
        # Extract trace section, excluding content inside code fences (avoid false positives from template examples)
        # Match both arrow traces and short stub-only trace lines used by scaffolding/POC plans.
        TRACE_SECTION=$(awk '/^## Critical Path Trace/{found=1; next} found && /^## /{exit} found && /^```/{skip=!skip; next} found && !skip{print}' "$PLAN_DIR")
        STUB_LINES=""
        if [[ -n "$TRACE_SECTION" ]]; then
            STUB_LINES=$(echo "$TRACE_SECTION" | awk '(/→/ || /^[[:space:]]*[⚠️❌]/ || /STUB|NOT BUILT/) && /⚠️|❌|STUB|NOT BUILT/')
        fi

        if [[ -n "$STUB_LINES" ]]; then
            # Check if each stub has a task reference
            UNRESOLVED=0
            TOTAL_STUBS=0
            while IFS= read -r line; do
                [[ -z "$line" ]] && continue
                ((TOTAL_STUBS++)) || true
                if ! echo "$line" | grep -qE '\[Task [0-9]+\.[0-9]+\]'; then
                    ((UNRESOLVED++)) || true
                fi
            done <<< "$STUB_LINES"

            if [[ "$UNRESOLVED" -gt 0 ]]; then
                if [[ "$SCOPE" == "feature" ]]; then
                    error "$UNRESOLVED unresolved stub(s) in critical path (scope: feature requires all stubs resolved)"
                else
                    warn "$UNRESOLVED unresolved stub(s) in critical path (acceptable for scope: $SCOPE)"
                fi
            else
                ok "All $TOTAL_STUBS stub(s) have task references"
            fi
        else
            ok "No stubs in critical path"
        fi
    else
        # Is this a new-style plan (has scope) or legacy?
        if grep -qE '^scope: ' "$PLAN_DIR" 2>/dev/null; then
            error "Has scope classification but missing Critical Path Trace section"
        else
            warn "No Critical Path Trace found (legacy plan — consider adding one)"
        fi
    fi
fi

# -------------------------------------------------------------------
# Check 6b: kill_criteria schema (abort conditions)
# -------------------------------------------------------------------
echo ""
echo "--- Kill Criteria ---"

validate_kill_criteria_block() {
    # Emits unit-separator-delimited records "ENTRY|idx|name|predicate|reason"
    # per entry (using ASCII 31 / \037, not the literal pipe, so predicates with
    # pipes won't collide). Entries are bounded by the YAML list-item marker
    # `- `, and any of the three fields may appear first. Unit separator is
    # required because bash `read -r` with tab-only IFS collapses consecutive
    # tabs (whitespace IFS semantics), losing empty fields.
    awk '
        BEGIN { idx=0; in_entry=0; name=""; pred=""; reason=""; US="\037" }
        function flush_entry() {
            if (in_entry) {
                print "ENTRY" US idx US name US pred US reason
                name=""; pred=""; reason=""; in_entry=0
            }
        }
        function strip_quotes(s) {
            gsub(/^["\x27]|["\x27]$/, "", s)
            return s
        }
        # A new list item marker "- " - accept it indented ("  - ", hand-written)
        # or at column 0 ("- ", the PyYAML default block-sequence dump from
        # mutate_doc.py). Both are valid YAML under kill_criteria.
        /^[[:space:]]*-[[:space:]]/ {
            flush_entry()
            idx++
            in_entry=1
            # Strip the leading "- " marker so the remainder looks like a
            # normal "key: value" line and falls through to the key handlers.
            sub(/^[[:space:]]*-[[:space:]]+/, "  ", $0)
        }
        in_entry && /^[[:space:]]+name:[[:space:]]*/ {
            line=$0
            sub(/^[[:space:]]+name:[[:space:]]*/, "", line)
            name=strip_quotes(line)
            next
        }
        in_entry && /^[[:space:]]+predicate:[[:space:]]*/ {
            line=$0
            sub(/^[[:space:]]+predicate:[[:space:]]*/, "", line)
            pred=strip_quotes(line)
            next
        }
        in_entry && /^[[:space:]]+reason:[[:space:]]*/ {
            line=$0
            sub(/^[[:space:]]+reason:[[:space:]]*/, "", line)
            reason=strip_quotes(line)
            next
        }
        END { flush_entry() }
    '
}

# Known predicate vocabulary. Unrecognized predicates are warnings (engine
# will log WARN at runtime and skip them) rather than errors so plans can
# reference new predicates introduced after this validator ships.
KNOWN_PREDICATES_RE='^(iteration[[:space:]]*[><=]+[[:space:]]*[0-9]+|same_test_failing_for[[:space:]]*[><=]+[[:space:]]*[0-9]+|files_outside\(plan_path\)[[:space:]]*[><=]+[[:space:]]*[0-9]+|any_test_file_deleted)[[:space:]]*$'

check_kill_criteria_file() {
    local file="$1"
    local label="$2"     # display name for error messages
    local block=""
    # kill_criteria always lives in the plan frontmatter (the heading form is
    # no longer authored - G1). Extract lines inside the top-level frontmatter
    # (between the first two ---) then the kill_criteria: block up to the next
    # top-level key.
    block=$(awk '
        /^---/ { c++; if (c==2) exit; next }
        c==1 { print }
    ' "$file" | awk '
        /^kill_criteria:/ { in_block=1; next }
        in_block && /^[A-Za-z_][A-Za-z0-9_-]*:/ { in_block=0 }
        in_block { print }
    ')

    if [[ -z "$block" ]]; then
        return 0  # no kill_criteria declared - defaults apply, not an error
    fi

    local entries
    entries=$(printf '%s\n' "$block" | validate_kill_criteria_block)

    if [[ -z "$entries" ]]; then
        error "$label: kill_criteria present but no entries parsed (expected list items like '- name: X')"
        return 1
    fi

    local count=0
    while IFS=$'\037' read -r tag idx entry_name entry_pred entry_reason; do
        [[ "$tag" == "ENTRY" ]] || continue
        ((count++)) || true
        if [[ -z "$entry_name" ]]; then
            error "$label: kill_criteria entry $idx missing required field \`name\`"
        fi
        if [[ -z "$entry_pred" ]]; then
            error "$label: kill_criteria entry ${entry_name:-$idx} missing required field \`predicate\`"
        fi
        if [[ -z "$entry_reason" ]]; then
            error "$label: kill_criteria entry ${entry_name:-$idx} missing required field \`reason\`"
        fi
        if [[ -n "$entry_pred" ]] && ! [[ "$entry_pred" =~ $KNOWN_PREDICATES_RE ]]; then
            warn "$label: kill_criteria entry ${entry_name:-$idx}: predicate \`$entry_pred\` not in known vocabulary (engine will log WARN and skip at runtime)"
        fi
    done <<< "$entries"

    ok "$label: kill_criteria has $count entr$([[ $count -eq 1 ]] && echo y || echo ies)"
}

# kill_criteria always lives in the single-doc plan's own frontmatter (the
# heading form is no longer authored - G1).
if [[ -f "$PLAN_DIR" ]]; then
    check_kill_criteria_file "$PLAN_DIR" "$(basename "$PLAN_DIR")"
fi

# -------------------------------------------------------------------
# Check 6b-bis: Consolidation block (step 2d gate)
# -------------------------------------------------------------------
# A blueprint MUST record exactly one consolidation outcome in frontmatter:
# absorb | append | proceed_alone, with a non-empty reason for every id
# listed. This is a positive marker, not an absence check - the gate passes
# only on a string the real outcome produces. It cannot live in skill prose
# alone: a direct `fno` call or a non-claude worker skips the skill layer and
# would ship green. Missing block, empty block, or out-of-enum outcome is an
# ERROR, never a warn.
check_consolidation_file() {
    local file="$1"
    local label="$2"
    # This check reports through its OWN counter, not the global ERRORS: the
    # positive marker below must print on a clean block even when an unrelated
    # earlier check already failed, or the gate goes silent exactly when the
    # output is longest.
    local c_errors=0
    c_error() { error "$@"; c_errors=$((c_errors + 1)); }

    # Presence only. Shape is the model's job (see below), but whether a block
    # exists at all is a policy date rather than a shape, so it stays here.
    # `grep -q` exits at the first match and SIGPIPEs the upstream awk, which
    # `pipefail` then reports as a failed pipeline - and this one is NEGATED,
    # so the plan would be told it has no block when it has one. Same trap the
    # parent_epic check below documents. Read all the input instead.
    if ! awk '/^---/ { c++; if (c==2) exit; next } c==1 { print }' "$file" \
            | grep -E '^consolidation:' >/dev/null; then
        # Grandfather: the gate governs plans written AFTER it shipped. Every
        # pre-existing plan would otherwise halt /do and /target on work
        # already in flight, so they WARN until backfilled. The boundary is
        # strictly-after: a plan created ON the gate date predates the gate
        # reaching its author, and nine live plans carry that date.
        local created gate_date="2026-08-17"
        created=$(awk '
            /^---/ { c++; if (c==2) exit; next }
            c==1 && /^created:/ {
                line=$0
                sub(/^[[:space:]]*created:[[:space:]]*/, "", line)
                sub(/[[:space:]]#.*$/, "", line)
                gsub(/["'"'"']/, "", line)
                sub(/[[:space:]].*$/, "", line)
                sub(/T.*$/, "", line)
                print line
                exit
            }
        ' "$file")
        # Normalize a compact YYYYMMDD stamp (real plans carry both spellings);
        # anything else is unparsable and must not be compared lexicographically,
        # because a malformed value sorts arbitrarily against the gate date.
        if [[ "$created" =~ ^[0-9]{8}$ ]]; then
            created="${created:0:4}-${created:4:2}-${created:6:2}"
        fi
        # No frontmatter at all is a different defect with a different owner,
        # and the gate cannot speak about a file that carries no plan header.
        # Say that plainly rather than passing: silence here would read as a
        # clean block. No live plan is in this shape (0 of 1056 gate targets).
        if ! awk '/^---/ { c++ } END { exit(c >= 2 ? 0 : 1) }' "$file"; then
            warn "$label: consolidation gate did not run - this file has no --- frontmatter block, and frontmatter is mandatory on every plan. Add one carrying created: and the consolidation block"
            return 0
        fi
        # A plan the gate cannot DATE would be grandfathered forever, so the
        # gate would fire on a date being present and new and read absence as
        # pre-gate. That is a silent, permanent opt-out for any plan that just
        # omits the key. Fall back to the plan's own name, and refuse loudly
        # when neither carries a date.
        local created_raw="$created"
        if [[ ! "$created" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
            created=$(_plan_name_date "$file")
        fi
        if [[ ! "$created" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
            c_error "$label: no consolidation: block, and no readable date to tell this plan from a pre-gate one (created: ${created_raw:-<missing>}, and the filename carries no YYYY-MM-DD). Add both - a plan the gate cannot date is grandfathered forever"
        elif [[ "$created" > "$gate_date" ]]; then
            c_error "$label: no consolidation: block in frontmatter - the step 2d gate must record exactly one outcome (absorb | append | proceed_alone), and silence is not an outcome"
        else
            warn "$label: no consolidation: block (created $created, not after the $gate_date gate) - backfill one before the next blueprint of this node"
        fi
        # Never abort the validator here: the later checks and the summary
        # must still run on the most common failure path.
        return 0
    fi

    # Shape check: hand the block to the model that already defines it.
    # This used to be an awk walk over the YAML, and five review rounds each
    # found a shape it misread - block scalars, flow lists, key order,
    # hyphenated sibling keys, duplicate keys, integer ids. Several of those
    # hard-FAILED valid plans. A second implementation of a shape the
    # `ConsolidationBlock` model already pins can only ever diverge from it,
    # so there is now one implementation and bash keeps only what is policy.
    local _src="" python_bin="" source_root="" delegate_out="" delegate_rc=0
    _src="$(_fno_source_python)"
    if [[ -n "$_src" ]]; then
        python_bin="${_src%%|*}"
        source_root="${_src##*|}"
    else
        source_root=$(_fno_source_root)
    fi
    if [[ -z "$source_root" ]]; then
        # A tooling gap is not a clean plan. Say the check did not run rather
        # than printing the OK marker, which would read as "the block is fine".
        warn "$label: consolidation block NOT CHECKED (no fno source checkout to import the shape model from) - not a pass"
        return 0
    fi
    local consolidation_prog
    consolidation_prog=$(cat <<'PYEOF'
import sys

try:
    import yaml
    from pydantic import ValidationError

    from fno.plan.schema import ConsolidationBlock
except Exception as exc:  # missing PyYAML / pydantic / fno on this interpreter
    sys.stdout.write("U\t" + " ".join(str(exc).split())[:160] + "\n")
    raise SystemExit(0)

ENUM = "(absorb | append | proceed_alone)"


def frontmatter(path):
    """The text between the first two `---` lines.

    Deliberately the same rule as the `/^---/` awk every other check in this
    script uses. A stricter reader here would report NOT CHECKED on a plan the
    presence check had already accepted, which is the divergence this rewrite
    exists to remove.
    """
    lines = open(path, encoding="utf-8").read().splitlines()
    opened = None
    for i, line in enumerate(lines):
        if line.startswith("---"):
            if opened is None:
                opened = i
            else:
                return "\n".join(lines[opened + 1:i])
    return None


def dup_keys(node, out):
    """Duplicate keys anywhere under the consolidation subtree.

    PyYAML takes the LAST of a repeated key silently, so two `outcome:` lines
    parse to one value and the discarded decision leaves no trace. The node
    graph still has both, which is why this reads `compose` and not the dict.
    """
    if isinstance(node, yaml.MappingNode):
        seen = set()
        for key, value in node.value:
            name = getattr(key, "value", None)
            if name in seen:
                out.append(name)
            seen.add(name)
            dup_keys(value, out)
    elif isinstance(node, yaml.SequenceNode):
        for item in node.value:
            dup_keys(item, out)
    return out


def render(err, block):
    loc, kind = err["loc"], err["type"]
    if loc == ("outcome",):
        if kind == "missing":
            return "consolidation block present but has no outcome: line (expected %s)" % ENUM
        raw = block.get("outcome")
        return "consolidation outcome `%s` is not in the enum %s" % (raw, ENUM)
    if len(loc) >= 2 and isinstance(loc[1], int):
        section, index = loc[0], loc[1]
        entry = None
        listed = block.get(section)
        if isinstance(listed, list) and index < len(listed):
            entry = listed[index]
        field = loc[2] if len(loc) > 2 else None
        if field == "id":
            if kind == "missing":
                return "consolidation section `%s` has an entry with no id" % section
            raw = entry.get("id") if isinstance(entry, dict) else entry
            return (
                "consolidation entry id `%s` (%s) is not a node id "
                "(expected <prefix>-<hex>, e.g. x-3bd3)" % (raw, section)
            )
        if field == "reason":
            named = entry.get("id") if isinstance(entry, dict) else entry
            return (
                "consolidation entry `%s` (%s) has an empty reason - the recorded "
                "decision must be checkable by a later reader" % (named, section)
            )
        return (
            "consolidation section `%s` entry %d is not an id/reason mapping"
            % (section, index + 1)
        )
    message = err["msg"].replace("Value error, ", "")
    for outcome, key in (("absorb", "absorbed"), ("append", "appended_to")):
        if message == "outcome %s requires at least one %s entry" % (outcome, key):
            return "consolidation outcome is %s but the %s: list is empty" % (outcome, key)
    where = ".".join(str(part) for part in loc)
    return "consolidation block%s: %s" % (" " + where if where else "", message)


path = sys.argv[1]
text = frontmatter(path)
if text is None:
    sys.stdout.write("U\tno closed --- frontmatter block\n")
    raise SystemExit(0)
try:
    loaded = yaml.safe_load(text)
    composed = yaml.compose(text)
except yaml.YAMLError as exc:
    # The frontmatter check above already errors on this; do not double-report.
    sys.stdout.write("U\t" + " ".join(str(exc).split())[:160] + "\n")
    raise SystemExit(0)

block = (loaded or {}).get("consolidation")
if not isinstance(block, dict):
    sys.stdout.write(
        "E\tconsolidation: must be a block of keys with an outcome: line %s, not `%s`\n"
        % (ENUM, block)
    )
    raise SystemExit(0)

for key, value in (composed.value if composed else []):
    if getattr(key, "value", None) == "consolidation":
        for name in dup_keys(value, []):
            sys.stdout.write(
                "E\tconsolidation block has more than one `%s:` line - a repeated key "
                "silently discards the earlier value\n" % name
            )

try:
    validated = ConsolidationBlock.model_validate(block)
except ValidationError as exc:
    for err in exc.errors():
        sys.stdout.write("E\t" + " ".join(render(err, block).split()) + "\n")
    raise SystemExit(0)
sys.stdout.write("O\t%s\n" % validated.outcome)
PYEOF
    )
    # Same ladder _semantic_validate walks: the checkout's own interpreter
    # first, then uv, which is the only arm a fresh worktree with no cli/.venv
    # can take.
    if [[ -n "$python_bin" ]]; then
        delegate_out=$(PYTHONPATH="$source_root/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
            "$python_bin" -c "$consolidation_prog" "$file" 2>&1) || delegate_rc=$?
    fi
    if [[ -z "$python_bin" || "$delegate_out" == U$'\t'* ]] \
            && command -v uv >/dev/null 2>&1; then
        delegate_rc=0
        delegate_out=$(uv run --project "$source_root/cli" \
            python -c "$consolidation_prog" "$file" 2>&1) || delegate_rc=$?
    fi
    if [[ -z "$delegate_out" && "$delegate_rc" -eq 0 ]]; then
        warn "$label: consolidation block NOT CHECKED (no interpreter with the fno CLI importable at $source_root) - not a pass"
        return 0
    fi

    local outcome="" line kind payload
    if [[ "$delegate_rc" -ne 0 ]]; then
        warn "$label: consolidation block NOT CHECKED (the shape check failed to run: ${delegate_out##*$'\n'}) - not a pass"
        return 0
    fi
    while IFS=$'\t' read -r kind payload; do
        case "$kind" in
            E) c_error "$label: $payload" ;;
            O) outcome="$payload" ;;
            U) warn "$label: consolidation block NOT CHECKED ($payload) - not a pass"; return 0 ;;
        esac
    done <<< "$delegate_out"

    # An append decision means the content went onto the OTHER node and no
    # second plan was written. This file existing contradicts that, and it is
    # the one self-contradiction the gate can catch mechanically - which makes
    # it the caller's knowledge, not the block's shape.
    if [[ "$outcome" == "append" ]]; then
        c_error "$label: consolidation outcome is append, but a plan file was written - append records that the content went onto the other node instead, so either delete this plan or record absorb / proceed_alone"
    fi

    if [[ $c_errors -eq 0 ]]; then
        ok "$label: consolidation outcome is ${outcome:-<missing>} (step 2d gate)"
    fi
}

echo ""
echo "--- Consolidation Gate ---"

if [[ -f "$PLAN_DIR" ]]; then
    check_consolidation_file "$PLAN_DIR" "$(basename "$PLAN_DIR")"
elif [[ -d "$PLAN_DIR" && -f "$PLAN_DIR/00-INDEX.md" ]]; then
    # Folder plans carry the frontmatter in 00-INDEX.md; the gate's "every
    # plan" scope includes them, so do not silently skip the check.
    check_consolidation_file "$PLAN_DIR/00-INDEX.md" "$(basename "$PLAN_DIR")/00-INDEX.md"
fi

# -------------------------------------------------------------------
# Check 6c: Wave section headers (parity with Execution Strategy YAML)
# -------------------------------------------------------------------
echo ""
echo "--- Wave Section Headers ---"

validate_wave_section_headers() {
    local index_file="$PLAN_DIR"
    if [[ ! -f "$index_file" ]]; then
        ok "Plan doc not found — header check skipped"
        return 0
    fi

    # Extract wave numbers declared in the Execution Strategy YAML block.
    # Slightly forgiving header regex: accept `## Execution Strategy`,
    # `### Execution Strategy`, trailing colon, or trailing whitespace.
    # If the user wrote `# Execution Strategy` (h1) the check skips, but
    # h1 in body text is unusual enough to be a user error worth catching
    # elsewhere.
    #
    # `|| true` on the outer command-sub: under `set -o pipefail`, any
    # transient failure inside the awk-awk-sort pipeline would abort the
    # whole script mid-function with no context. Treating awk/sort
    # failure as "no waves declared" is the right default; a malformed
    # plan should surface via the missing-headers check below, not via
    # a bare non-zero exit from validate-plan.sh.
    # Capture the RAW list (no `sort -un`) so duplicate wave IDs surface
    # as their own diagnostic. Two `- wave: 1` blocks in YAML is malformed
    # input that `/do waves`'s scheduler can't sensibly act on; collapsing
    # them silently would let that error reach merge.
    local yaml_waves_raw
    yaml_waves_raw=$( { awk '
        /^##+[[:space:]]+Execution Strategy[[:space:]]*:?[[:space:]]*$/ { found=1; next }
        found && /^##+[[:space:]]/ { exit }
        found && /^```/ { in_fence=!in_fence; next }
        found && in_fence { print }
    ' "$index_file" | awk '
        /^[[:space:]]*-[[:space:]]*wave:[[:space:]]*[0-9]+/ {
            gsub(/^[[:space:]]*-[[:space:]]*wave:[[:space:]]*/, "")
            gsub(/[^0-9].*/, "")
            print
        }
    '; } || true )
    # Normalize raw wave numbers to canonical integers so `- wave: 01`
    # and `- wave: 1` are treated identically by both dedup (`sort -un`)
    # and duplicate detection (`sort | uniq -d`). Without normalization
    # the two would disagree: `sort -un` numerically dedupes them; plain
    # `sort | uniq -d` sees distinct strings and misses the duplicate.
    # `awk '{printf "%d\n", $0+0}'` collapses to integer form.
    # The upstream awk in yaml_waves_raw extraction already constrains
    # output to digit-only lines via the regex+gsub, so the previous
    # `grep -E '^[0-9]+$'` filter here was redundant. The awk
    # normalization stays — that's what handles `01` vs `1` equivalence.
    # (`header_waves_raw` keeps its grep because sed there can leave
    # non-numeric lines unchanged on a non-matching input.)
    local yaml_waves
    yaml_waves=$(printf '%s\n' "$yaml_waves_raw" \
        | awk 'NF{printf "%d\n", $0+0}' | sort -un || true)
    local yaml_dupes
    yaml_dupes=$(printf '%s\n' "$yaml_waves_raw" \
        | awk 'NF{printf "%d\n", $0+0}' | sort -n | uniq -d || true)

    # Extract wave numbers from `## Wave N: <name>` section headers.
    # Capture grep into a variable (don't `done < <(...)`): process
    # substitution swallows grep's exit code, so a real failure (file
    # unreadable, permission denied) would silently look like "no
    # headers" and false-pass the parity check at the bottom.
    local header_grep
    header_grep=$( { grep -E '^## Wave [0-9]+:' "$index_file" 2>/dev/null || true; } )
    local header_waves_raw
    header_waves_raw=$(printf '%s\n' "$header_grep" \
        | sed -E 's/^## Wave ([0-9]+):.*/\1/' | grep -E '^[0-9]+$' || true)
    local header_waves
    header_waves=$(printf '%s\n' "$header_waves_raw" \
        | awk 'NF{printf "%d\n", $0+0}' | sort -un || true)
    local header_dupes
    header_dupes=$(printf '%s\n' "$header_waves_raw" \
        | awk 'NF{printf "%d\n", $0+0}' | sort -n | uniq -d || true)


    # Duplicate detection runs BEFORE the `-z "$yaml_waves"` early
    # return: a plan with two `## Wave 1:` headers but no Execution
    # Strategy YAML still has ambiguous wikilink-fragment routing that
    # the validator must surface. Same shape for malformed YAML with
    # duplicate `- wave: N` entries.
    for d in $yaml_dupes; do
        error "Execution Strategy declares wave $d more than once - each wave number must appear exactly once in the YAML manifest"
    done
    for d in $header_dupes; do
        error "'## Wave $d:' section header appears more than once - each wave number must have exactly one section"
    done

    if [[ -z "$yaml_waves" ]]; then
        if [[ -n "$header_waves" ]]; then
            warn "## Wave N: headers present but no waves declared in ## Execution Strategy YAML"
        else
            ok "No waves declared — header check skipped (single-phase plan)"
        fi
        return 0
    fi

    local missing=""
    local orphan=""
    # `missing` and `orphan` MUST stay initialized above; `set -u` plus
    # the `for w in $var` word-split below depend on the empty string
    # being a defined value.
    while IFS= read -r w; do
        [[ -z "$w" ]] && continue
        # The `! ... | grep -qx` shape is load-bearing under `set -e`:
        # the `!` converts grep's exit-1-on-no-match into a tested
        # condition rather than a script abort. Removing the `!` would
        # silently abort the loop on the first non-matching wave.
        if ! echo "$header_waves" | grep -qx "$w"; then
            missing+="$w "
        fi
    done <<< "$yaml_waves"

    while IFS= read -r w; do
        [[ -z "$w" ]] && continue
        if ! echo "$yaml_waves" | grep -qx "$w"; then
            orphan+="$w "
        fi
    done <<< "$header_waves"

    # Transitional severity: existing plans authored before this
    # convention adopt have YAML waves but zero `## Wave N:` headers.
    # Surfacing them as ERROR would block every running `/target` pipeline
    # at init the moment this lands. Until `/blueprint` itself is updated
    # to emit the headers AND a backfill pass lands, missing/orphan are
    # WARN. Flip back to `error` once the backfill PR ships - see
    # `plans/2026-05-23-blueprint-canonical-wave-headers.md` "Why
    # fail-loud over fail-quiet" for the eventual hard-error rationale.
    for w in $missing; do
        warn "Execution Strategy declares wave $w but no '## Wave $w: <name>' section header exists (legacy plan? backfill once /blueprint emits headers)"
    done

    for w in $orphan; do
        warn "'## Wave $w:' section header has no matching wave in ## Execution Strategy YAML"
    done

    # Naming weakness check (WARN). Scope to headers whose wave number
    # IS declared in the YAML - flagging "<name>" or "Wave 2" on an
    # orphan header is double-reporting the same problem and just adds
    # noise to the output.
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        local wave_num
        wave_num=$(echo "$line" | sed -E 's/^## Wave ([0-9]+):.*/\1/')
        # Skip naming check for orphan headers (already warned above).
        if ! echo "$yaml_waves" | grep -qx "$wave_num"; then
            continue
        fi
        local name_part
        name_part=$(echo "$line" | sed -E 's/^## Wave [0-9]+:[[:space:]]*//')
        if [[ -z "$name_part" ]]; then
            warn "Wave header '$line' has empty name"
        elif [[ "$name_part" == "<name>" ]]; then
            warn "Wave header '$line' has placeholder '<name>' (template not customized)"
        elif [[ "$name_part" =~ ^[Ww]ave[[:space:]]+[0-9]+$ ]]; then
            warn "Wave header '$line' has tautological name (just 'Wave N')"
        fi
    done <<< "$header_grep"

    # Gate the success message on absence of duplicates too. Without
    # this, a plan with duplicate `- wave: 1` blocks but otherwise
    # matching sets would emit both ERROR (from the dup loop above) and
    # OK (from here), confusing humans and log parsers.
    if [[ -z "$missing" && -z "$orphan" && -z "$yaml_dupes" && -z "$header_dupes" ]]; then
        local count
        count=$(printf '%s\n' "$yaml_waves" | grep -c '^[0-9]' || true)
        ok "All ${count} wave(s) in YAML have matching '## Wave N: <name>' headers"
    fi
}

if [[ "$SEMANTIC_SINGLE_DOC" -eq 1 ]]; then
    ok "Wave topology validated from Execution Strategy YAML"
else
    validate_wave_section_headers
fi

# -------------------------------------------------------------------
# Check 7: impeccable_stages pin validator (Phase 02.2)
# -------------------------------------------------------------------
echo ""
echo "--- impeccable_stages ---"

# Known /impeccable subcommand list (locked baseline per brief decision 2).
KNOWN_STAGES="craft critique polish harden audit layout animate bolder colorize delight overdrive quieter typeset distill extract adapt shape teach"

_validate_stage_entry() {
    # Usage: _validate_stage_entry <phase_name> <stage>
    # Validates a single stage string against KNOWN_STAGES; emits error if unknown.
    local phase_name="$1"
    local stage="$2"
    [[ -z "$stage" ]] && return
    local found=0
    for known in $KNOWN_STAGES; do
        if [[ "$stage" == "$known" ]]; then
            found=1
            break
        fi
    done
    if [[ $found -eq 0 ]]; then
        error "$phase_name: impeccable_stages contains unknown stage '$stage'. Known stages: $KNOWN_STAGES"
    fi
}

_check_impeccable_stages_in_file() {
    local phase_file="$1"
    local phase_name
    phase_name=$(basename "$phase_file")

    # -------------------------------------------------------------------
    # Inline list form: impeccable_stages: [craft, critique, harden]
    # -------------------------------------------------------------------
    while IFS= read -r stages_line; do
        # Extract the content between [ and ]. Strip everything before
        # impeccable_stages:'s opening [ and everything after the closing ]
        # so trailing comments (`impeccable_stages: [craft] # ...`) don't
        # poison the entry list.
        local stages_raw
        stages_raw=$(echo "$stages_line" | sed 's/.*impeccable_stages:[[:space:]]*\[//; s/\].*//')

        # Empty list check: bracket pair with only whitespace inside.
        local inner
        inner=$(echo "$stages_raw" | tr -d ' ')
        if [[ -z "$inner" ]]; then
            error "$phase_name: impeccable_stages: [] is empty (intent unclear - list at least one stage or remove the field)"
            continue
        fi

        # Check each comma-separated entry
        IFS=',' read -ra stage_entries <<< "$stages_raw"
        for entry in "${stage_entries[@]}"; do
            local stage
            stage=$(echo "$entry" | tr -d ' ')
            _validate_stage_entry "$phase_name" "$stage"
        done
    done < <(grep -E '^[[:space:]]*impeccable_stages:[[:space:]]*\[' "$phase_file" 2>/dev/null || true)

    # -------------------------------------------------------------------
    # Block-list form:
    #   impeccable_stages:
    #     - craft
    #     - foo   <- must also be validated
    # -------------------------------------------------------------------
    # Detect a bare "impeccable_stages:" key (no "[" on the same line).
    while IFS= read -r key_line_num; do
        [[ -z "$key_line_num" ]] && continue
        local key_lineno
        key_lineno=$(echo "$key_line_num" | cut -d: -f1)

        # Collect continuation lines that start with optional whitespace + "- "
        local block_entries=()
        while IFS= read -r cont_line; do
            # A new top-level key or blank line without leading spaces ends the block
            if [[ "$cont_line" =~ ^[^[:space:]] || -z "$cont_line" ]]; then
                break
            fi
            # Only accept lines that are a list item under this key
            if [[ "$cont_line" =~ ^[[:space:]]+-[[:space:]] ]]; then
                local entry
                entry=$(echo "$cont_line" | sed 's/^[[:space:]]*-[[:space:]]*//' | tr -d ' ')
                block_entries+=("$entry")
            fi
        done < <(tail -n +"$((key_lineno + 1))" "$phase_file")

        if [[ ${#block_entries[@]} -eq 0 ]]; then
            error "$phase_name: impeccable_stages: [] is empty (intent unclear - list at least one stage or remove the field)"
            continue
        fi

        for stage in "${block_entries[@]}"; do
            _validate_stage_entry "$phase_name" "$stage"
        done
    done < <(grep -n -E '^[[:space:]]*impeccable_stages:[[:space:]]*$' "$phase_file" 2>/dev/null || true)
}

STAGES_CHECKED=0
if [[ -f "$PLAN_DIR" ]] && grep -qE '^[[:space:]]*impeccable_stages:' "$PLAN_DIR" 2>/dev/null; then
    _check_impeccable_stages_in_file "$PLAN_DIR"
    STAGES_CHECKED=1
fi

if [[ $STAGES_CHECKED -eq 0 ]]; then
    ok "No impeccable_stages pins found (opt-in field)"
else
    ok "Validated impeccable_stages in the plan doc"
fi

# -------------------------------------------------------------------
# Check 7b: Stamp field awareness
# -------------------------------------------------------------------
echo ""
echo "--- Stamp Fields ---"

# Stamp fields (status, shipped_at, urls, session_ids) are written by the
# /target ship gate - they are always valid and never flagged as unknown.
target_file=""
if [[ -f "$PLAN_DIR" && "$PLAN_DIR" == *.md ]]; then
    target_file="$PLAN_DIR"
fi

if [[ -n "$target_file" ]]; then
    # "Already shipped" is a rung question, so ask the rung authority rather than
    # re-listing the terminal spellings here. The old grep hardcoded
    # `done|in_review|shipped`, which silently rots every time a spelling
    # retires - `shipped` is itself a retired spelling of `in_review`, and
    # `fno plan rung` resolves it through the same alias table the Python side
    # uses instead of a second copy that has to be remembered.
    STATUS_FM="$(_plan_rung "$target_file")"
    if [[ "$STATUS_FM" == \!* ]]; then
        # The verb could not answer. Saying "not yet shipped" here would be an
        # affirmative claim built on a non-answer; the same reason is already
        # warned about above, so keep this line honest and short.
        ok "Stamp fields not checked (rung unavailable)"
    elif [[ "$STATUS_FM" == "in_review" || "$STATUS_FM" == "done" ]]; then
        ok "INFO: plan is already shipped (rung: $STATUS_FM) - stamp fields present and accepted"
    else
        ok "No stamp fields detected (plan not yet shipped)"
    fi
fi

# -------------------------------------------------------------------
# Summary
# -------------------------------------------------------------------
echo ""
echo "=== Result ==="
echo "Errors: $ERRORS | Warnings: $WARNINGS"

if [[ $ERRORS -gt 0 ]]; then
    echo "FAIL -- fix errors before execution"
    exit 1
fi

if [[ $WARNINGS -gt 0 ]]; then
    echo "PASS with warnings"
else
    echo "PASS -- plan looks good"
fi
exit 0
