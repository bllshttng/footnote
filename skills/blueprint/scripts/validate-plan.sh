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

error() { echo "  ERROR: $*"; ((ERRORS++)) || true; }
warn()  { echo "  WARN:  $*"; ((WARNINGS++)) || true; }
ok()    { echo "  OK:    $*"; }

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
# an empty-why sentinel; it is born `status: stub` and MUST be inline-filled (or
# designed by a fan-out /think pass) before its plan_path is linked - a linked
# child derives `ready` and dispatchers launch fresh-context workers against it.
# This check refuses to pass a plan still carrying any stub marker, so the link
# step (skill body) and this validator agree on "filled". Keep STUB_MARKERS in
# sync with cli/src/fno/graph/_decompose.py.
echo ""
echo "--- Stub Markers (decompose child) ---"
if [[ -f "$PLAN_DIR" ]]; then
    STUB_MARKERS=(
        "<!-- Seeded from epic waves"
        "<!-- From the epic's File Ownership Map"
        "<!-- The checks that prove"
        "<!-- Why (from epic):"
    )
    _found_stub=0
    for _m in "${STUB_MARKERS[@]}"; do
        if grep -Fq "$_m" "$PLAN_DIR"; then
            error "unfilled stub marker '${_m} ...' in $(basename "$PLAN_DIR"); inline-fill the scaffold before linking plan_path"
            _found_stub=1
        fi
    done

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

if [[ -z "$TASK_HEADINGS_RAW" ]]; then
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

if [[ -f "$PLAN_DIR" ]]; then
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

if [[ -f "$PLAN_DIR" ]]; then
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

if [[ -f "$PLAN_DIR" ]]; then
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
        in_block && /^[A-Za-z_][A-Za-z0-9_]*:/ { in_block=0 }
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

validate_wave_section_headers

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
    # Scope to frontmatter only: extract lines between the first two --- delimiters.
    FRONTMATTER=$(awk '/^---/{c++; if(c==2) exit; next} c==1{print}' "$target_file")
    STATUS_FM=$(echo "$FRONTMATTER" | grep -oE "^status:[[:space:]]*(done|shipped)" 2>/dev/null | head -1 | sed 's/status:[[:space:]]*//' || true)
    if [[ -n "$STATUS_FM" ]]; then
        ok "INFO: plan is already shipped (status: $STATUS_FM) - stamp fields present and accepted"
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
