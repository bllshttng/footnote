#!/usr/bin/env bash
# mission-emit.sh - write fleet/megatron mission-completion JSON files
# when a target session reaches status: COMPLETE.
#
# Lifted from hooks/target-stop-hook.sh (Phase 1 of stop-hook refactor).
# Behavior is identical to the inline definitions.
#
# Per plan 2026-05-13-megatron-discoveries-field (ab-bc919f7f), the brief
# assembler at cli/src/fno/megatron/brief.py needs each wave's
# cross-wave learnings to seed wave N+1 dispatch context. emit_mission_
# complete_if_needed sources those learnings from HANDOFF.md via
# _extract_handoff_discoveries and writes them into the completion JSON
# under ~/.fno/fleet/{slug}/completions/wave-{N}/{project}.json.
#
# Idempotent via mission_complete_emitted_at sentinel in target-state.md.
# Non-fatal on any failure: a missing PR URL emits <help> and stays
# unsent so the next stop-hook invocation retries.
#
# Requires (set by caller):
#   STATE_FILE - path to target-state.md
#   STATE_DIR  - directory containing target-state.md (typically .fno/)
#   REPO_ROOT  - project repo root
#   log()      - logging function from the hook
#   read_state_field KEY - single-arg form (uses global STATE_FILE)

# _resolve_canonical_project_name REPO_ROOT
#   Match REPO_ROOT against settings.yaml's work.workspaces.*.projects[]
#   path field and return the canonical project name. Falls back to the
#   repo's basename when the lookup can't resolve. Defined as a function
#   so the heredoc is not nested inside command substitution + trailing
#   redirects, which bash 3.2 / older zsh parse inconsistently.
_resolve_canonical_project_name() {
    local repo_root="$1"
    # Pre-compute the main-worktree root so the Python script can match
    # against it even when called from a linked worktree. `git rev-parse
    # --git-common-dir` resolves to the main repo's .git directory; its
    # parent is the main worktree root. Megawalk runs child sessions in
    # .claude/worktrees/<node_id>/ (linked worktrees) so the cwd basename
    # is the node id, not the canonical project name (Codex review on
    # PR #254). Pass the common-dir parent into the resolver as a second
    # argument so it can try both paths before falling back.
    local common_root=""
    if common_root=$(git -C "$repo_root" rev-parse --git-common-dir 2>/dev/null); then
        # --git-common-dir prints relative-to-cwd or absolute depending on
        # the git version. Resolve to absolute via python below; here just
        # strip the trailing "/.git" so the parent is the main worktree.
        common_root="${common_root%/.git}"
        common_root="${common_root%.git}"
    fi
    python3 - "$repo_root" "${common_root:-}" <<'PYEOF'
import os
import sys
from pathlib import Path

repo_root = Path(sys.argv[1]).resolve()
common_root_arg = sys.argv[2] if len(sys.argv) > 2 else ""
candidate_roots: list[Path] = [repo_root]
if common_root_arg:
    try:
        # common_root may be relative to repo_root or already absolute.
        common_root = Path(common_root_arg)
        if not common_root.is_absolute():
            common_root = (repo_root / common_root).resolve()
        else:
            common_root = common_root.resolve()
        if common_root != repo_root:
            candidate_roots.append(common_root)
    except OSError:
        pass

settings = Path(os.path.expanduser("~")) / ".fno" / "config.toml"
fallback = repo_root.name

if not settings.exists():
    print(fallback)
    sys.exit(0)

import tomllib

try:
    data = tomllib.loads(settings.read_text(encoding="utf-8")) or {}
except tomllib.TOMLDecodeError:
    print(fallback)
    sys.exit(0)

if not isinstance(data, dict):
    print(fallback)
    sys.exit(0)

workspaces = (data.get("work") or {}).get("workspaces") or {}
if not isinstance(workspaces, dict):
    print(fallback)
    sys.exit(0)

for ws in workspaces.values():
    if not isinstance(ws, dict):
        continue
    for proj in ws.get("projects") or []:
        if not isinstance(proj, dict):
            continue
        name = proj.get("name")
        path = proj.get("path")
        if not isinstance(name, str) or not isinstance(path, str):
            continue
        try:
            resolved = Path(os.path.expanduser(path)).resolve()
        except OSError:
            continue
        # Match against the linked-worktree root OR the main-worktree
        # root (parent of git-common-dir). The latter is what
        # settings.yaml records for projects under megawalk control.
        if resolved in candidate_roots:
            print(name)
            sys.exit(0)

print(fallback)
PYEOF
}

# _extract_handoff_discoveries HANDOFF_PATH
#   Echo the markdown body under `### Discoveries` (or `## Learnings` /
#   `### Learnings` as fallback) from HANDOFF.md, capped at 8 KB with a
#   clear truncation marker. Empty string when the file is missing,
#   unreadable, or contains neither section. All failure modes are
#   silent (no stderr, return 0).
_extract_handoff_discoveries() {
    local handoff_path="$1"
    [[ -f "$handoff_path" && -r "$handoff_path" ]] || return 0
    local out
    # Heading match. `### Discoveries` is preferred; `## Learnings` /
    # `### Learnings` is a fallback so child sessions that emitted a
    # learnings section still feed the brief (both heading levels because
    # _generate_handoff in handoff-generator.sh writes `## Learnings`
    # at H2, while older conventions used `### Learnings`). Both sections
    # may exist in any order; we buffer each independently. We track
    # `disc_seen` separately from `disc != ""` so an explicitly empty
    # Discoveries section is RESPECTED rather than falling through to a
    # stale prior Learnings body (Codex P2 finding on PR #256). Section
    # body runs from the matched heading until the next H1/H2/H3 heading
    # or EOF. Heading regex accepts one-or-more spaces after the hashes
    # (markdown norm; Gemini medium-priority on PR #256).
    out=$(awk '
        BEGIN { state = ""; disc = ""; learn = ""; disc_seen = 0 }
        /^###[[:space:]]+Discoveries[[:space:]]*$/ { state = "disc"; disc_seen = 1; next }
        /^###[[:space:]]+Learnings[[:space:]]*$/   { state = "learn"; next }
        /^##[[:space:]]+Learnings[[:space:]]*$/    { state = "learn"; next }
        /^#[[:space:]]+/   { state = ""; next }
        /^##[[:space:]]+/  { state = ""; next }
        /^###[[:space:]]+/ { state = ""; next }
        state == "disc"  { disc = disc $0 "\n" }
        state == "learn" { learn = learn $0 "\n" }
        END {
            if (disc_seen)        printf "%s", disc
            else if (learn != "") printf "%s", learn
        }
    ' "$handoff_path" 2>/dev/null) || return 0
    # Trim trailing whitespace + cap at 8 KB. UTF-8-safe truncation via
    # python3 (already a hook dep) so jq never sees mid-multibyte garbage
    # (Gemini high-priority on PR #256: head -c slices bytes and can land
    # mid-codepoint, which makes the downstream jq --arg call abort and
    # the entire mission completion emit silently skip).
    local trimmed="${out%"${out##*[![:space:]]}"}"
    [[ -z "$trimmed" ]] && return 0
    local byte_count
    byte_count=$(printf '%s' "$trimmed" | wc -c | tr -d ' ')
    if (( byte_count > 8192 )); then
        local safe
        # Pass via here-string, NOT a pipe: under `set -o pipefail` (in
        # effect at line 14 of the hook), printf gets SIGPIPE when python
        # closes stdin after reading 8 KB, which kills the whole pipeline
        # and trips the `||` fallback. Here-string sends the full content
        # via a temp file so python can read partial without killing a
        # writer process.
        safe=$(python3 -c "
import sys
# Read only the first 8 KB. Drop trailing bytes that don't form a complete
# UTF-8 codepoint via errors='ignore'.
data = sys.stdin.buffer.read(8192)
text = data.decode('utf-8', errors='ignore')
sys.stdout.write(text)
" 2>/dev/null <<<"$trimmed") || safe="$trimmed"
        printf '%s\n\n...(truncated to 8 KB; see HANDOFF.md)\n' "$safe"
    else
        printf '%s\n' "$trimmed"
    fi
}

# emit_mission_complete_if_needed
#   Task 3.3 of plan 2026-05-13-megatron-gap-closure. When a target
#   session reaches status: COMPLETE with mission_* fields populated
#   (set by init-target-state.sh from megawalk's TARGET_MISSION_* env
#   vars), write a completion JSON file at
#     ~/.fno/fleet/{slug}/completions/wave-{N}/{project}.json
#   Idempotent via mission_complete_emitted_at sentinel in
#   target-state.md. Non-fatal on any failure.
emit_mission_complete_if_needed() {
    local mission_id mission_wave mission_slug mission_from_msg_id
    local emitted_at pr_url project commit_sha completed_at fleet_root
    local target_dir target_path tmp_path payload event_line

    [[ -f "$STATE_FILE" ]] || return 0

    mission_id=$(read_state_field "mission_id" 2>/dev/null)
    if [[ -z "$mission_id" || "$mission_id" == "null" ]]; then
        return 0  # non-mission session: silent no-op
    fi

    emitted_at=$(read_state_field "mission_complete_emitted_at" 2>/dev/null)
    if [[ -n "$emitted_at" && "$emitted_at" != "null" ]]; then
        return 0  # already emitted: idempotent skip
    fi

    mission_wave=$(read_state_field "mission_wave" 2>/dev/null)
    mission_slug=$(read_state_field "mission_slug" 2>/dev/null)
    mission_from_msg_id=$(read_state_field "mission_from_msg_id" 2>/dev/null)
    if [[ -z "$mission_wave" || "$mission_wave" == "null" \
       || -z "$mission_slug" || "$mission_slug" == "null" ]]; then
        log "WARNING: mission_id set but mission_wave or mission_slug missing/null - skipping completion emit"
        return 0
    fi

    pr_url=$(read_state_field "pr_url" 2>/dev/null)
    if [[ -z "$pr_url" || "$pr_url" == "null" ]]; then
        # Non-fatal: ledger doesn't have the PR URL yet (Phase 6 may not have run,
        # or the artifact has not been written). Log + <help> + retry next time.
        local mc_failed_event
        mc_failed_event=$(jq -nc \
            --arg type "mission_complete_emit_failed" \
            --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
            --arg mid "$mission_id" \
            --arg reason "pr_url_missing" \
            '{type:$type, ts:$ts, mission_id:$mid, reason:$reason}' 2>/dev/null)
        [[ -n "$mc_failed_event" ]] && \
            echo "$mc_failed_event" >> "$STATE_DIR/hook-events.jsonl" 2>/dev/null || true
        echo "<help reason=\"mission-pr-url-missing\" evidence=\"mission_id=$mission_id, wave=$mission_wave\">PR URL not yet captured in target-state.md; mission completion file deferred to next stop-hook invocation.</help>" >&2
        log "mission_complete_emit deferred: pr_url not yet set in state.md"
        return 0
    fi

    # Resolve canonical project name. Strategy: walk settings.yaml
    # work.workspaces.*.projects[] looking for a record whose expanded
    # path matches the current cwd. Fall back to cwd basename if no match.
    project=$(_resolve_canonical_project_name "$REPO_ROOT" 2>/dev/null || echo "")
    if [[ -z "$project" ]]; then
        project="$(basename "$REPO_ROOT")"
    fi

    # Defensive: reject project names with path-traversal characters so the
    # downstream join cannot escape the fleet root. The fleet_root contract
    # is a directory under ~/.fno/fleet/.
    if [[ "$project" == *".."* || "$project" == */* ]]; then
        log "WARNING: refusing to emit completion for unsafe project name: $project"
        return 0
    fi
    if [[ "$mission_slug" == *".."* || "$mission_slug" == */* ]]; then
        log "WARNING: refusing to emit completion for unsafe mission_slug: $mission_slug"
        return 0
    fi
    if ! [[ "$mission_wave" =~ ^[0-9]+$ ]]; then
        log "WARNING: refusing to emit completion: mission_wave not a positive integer: $mission_wave"
        return 0
    fi

    commit_sha=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "")
    completed_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    fleet_root="${HOME}/.fno/fleet"
    target_dir="${fleet_root}/${mission_slug}/completions/wave-${mission_wave}"
    target_path="${target_dir}/${project}.json"

    mkdir -p "$target_dir" 2>/dev/null || {
        log "WARNING: failed to mkdir $target_dir - skipping completion emit"
        return 0
    }

    # Cross-wave discoveries: best-effort extract from HANDOFF.md. The brief
    # assembler (cli/src/fno/megatron/brief.py::assemble_wave_brief)
    # reads c.get("discoveries") to seed wave N+1 dispatch context. Always
    # threaded into the payload below so consumers can distinguish
    # missing-field (old completion JSON) from empty-but-present (new
    # shape, no discoveries) without ambiguity. See plan 2026-05-13-
    # megatron-discoveries-field for the design.
    local discoveries_md
    discoveries_md=$(_extract_handoff_discoveries "$REPO_ROOT/.fno/HANDOFF.md")
    if [[ -z "$discoveries_md" ]]; then
        local handoff_event reason
        if [[ ! -f "$REPO_ROOT/.fno/HANDOFF.md" ]]; then
            reason="file_not_found"
        elif [[ ! -r "$REPO_ROOT/.fno/HANDOFF.md" ]]; then
            reason="file_unreadable"
        elif [[ ! -s "$REPO_ROOT/.fno/HANDOFF.md" ]]; then
            reason="file_empty"
        else
            reason="no_sections_found"
        fi
        handoff_event=$(jq -nc \
            --arg type "mission_handoff_unreadable" \
            --arg ts "$completed_at" \
            --arg mid "$mission_id" \
            --arg reason "$reason" \
            '{type:$type, ts:$ts, mission_id:$mid, reason:$reason}' 2>/dev/null)
        [[ -n "$handoff_event" ]] && \
            echo "$handoff_event" >> "$STATE_DIR/hook-events.jsonl" 2>/dev/null || true
        log "mission_complete: HANDOFF.md $reason - discoveries field will be empty"
    fi

    # reply_to_msg_id: null unless we have a non-empty mission_from_msg_id.
    local from_msg_jq
    if [[ -z "$mission_from_msg_id" || "$mission_from_msg_id" == "null" ]]; then
        from_msg_jq='null'
    else
        from_msg_jq=$(jq -n --arg v "$mission_from_msg_id" '$v' 2>/dev/null || echo 'null')
    fi

    payload=$(jq -nc \
        --argjson schema_version 1 \
        --arg project "$project" \
        --argjson wave "$mission_wave" \
        --arg mission_id "$mission_id" \
        --arg pr_url "$pr_url" \
        --arg pr_status "open" \
        --arg commit_sha "$commit_sha" \
        --arg completed_at "$completed_at" \
        --argjson reply_to "$from_msg_jq" \
        --arg discoveries "$discoveries_md" \
        '{schema_version:$schema_version, project:$project, wave:$wave, mission_id:$mission_id, pr_url:$pr_url, pr_status:$pr_status, commit_sha:$commit_sha, completed_at:$completed_at, reply_to_msg_id:$reply_to, discoveries:$discoveries}' 2>/dev/null)
    if [[ -z "$payload" ]]; then
        log "WARNING: jq failed to build completion payload - skipping"
        return 0
    fi

    # Atomic same-fs rename: write tempfile in the target dir, then mv -f.
    tmp_path="${target_dir}/.${project}.json.tmp.$$"
    if ! printf '%s\n' "$payload" > "$tmp_path" 2>/dev/null; then
        log "WARNING: failed to write tempfile $tmp_path - skipping"
        rm -f "$tmp_path" 2>/dev/null || true
        return 0
    fi
    if ! mv -f "$tmp_path" "$target_path" 2>/dev/null; then
        log "WARNING: failed to atomic-rename $tmp_path -> $target_path"
        rm -f "$tmp_path" 2>/dev/null || true
        return 0
    fi

    # Stamp mission_complete_emitted_at in target-state.md so subsequent
    # stop-hook invocations are idempotent. The field is always present
    # (init-target-state seeds it with `null`); we replace `null` with the
    # timestamp. If the field is somehow absent (legacy state.md edited
    # by hand), append it so the next invocation sees a stamped value
    # and skips - otherwise sed substitutes nothing and rc=0, leading to
    # silent re-emission on every subsequent stop hook.
    local state_tmp
    state_tmp="${STATE_FILE}.tmp.$$"
    if grep -qE '^mission_complete_emitted_at:' "$STATE_FILE" 2>/dev/null; then
        if sed "s|^mission_complete_emitted_at:.*|mission_complete_emitted_at: \"${completed_at}\"|" "$STATE_FILE" > "$state_tmp" 2>/dev/null; then
            mv -f "$state_tmp" "$STATE_FILE" 2>/dev/null || rm -f "$state_tmp" 2>/dev/null
        else
            rm -f "$state_tmp" 2>/dev/null
            log "WARNING: sed failed to update mission_complete_emitted_at in state.md"
        fi
    else
        # Field absent - append it inside the frontmatter so the next
        # idempotency check fires. Append before the closing `---` line.
        if awk -v ts="$completed_at" '
            BEGIN { in_fm=0; closed=0 }
            NR==1 && /^---/ { print; in_fm=1; next }
            in_fm && !closed && /^---/ { print "mission_complete_emitted_at: \"" ts "\""; print; closed=1; in_fm=0; next }
            { print }
        ' "$STATE_FILE" > "$state_tmp" 2>/dev/null && [[ -s "$state_tmp" ]]; then
            mv -f "$state_tmp" "$STATE_FILE" 2>/dev/null || rm -f "$state_tmp" 2>/dev/null
        else
            rm -f "$state_tmp" 2>/dev/null
            log "WARNING: awk failed to append mission_complete_emitted_at to state.md"
        fi
    fi

    # Append the success event for forensic / observability use.
    event_line=$(jq -nc \
        --arg type "mission_complete_emitted" \
        --arg ts "$completed_at" \
        --arg mid "$mission_id" \
        --argjson wave "$mission_wave" \
        --arg project "$project" \
        --arg pr_url "$pr_url" \
        '{type:$type, ts:$ts, mission_id:$mid, wave:$wave, project:$project, pr_url:$pr_url, written_at:$ts}' 2>/dev/null)
    [[ -n "$event_line" ]] && \
        echo "$event_line" >> "$STATE_DIR/hook-events.jsonl" 2>/dev/null || true

    log "mission_complete_emitted: project=$project wave=$mission_wave mission=$mission_id pr=$pr_url -> $target_path"
    return 0
}
