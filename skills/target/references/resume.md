# Resume

**Load when:** the user invokes `/target resume`, OR a fresh session starts and an existing `target-state.md` is found.

```bash
/target resume   # Continue from target-state.md
```

Reads state, skips completed steps, continues from last position.

## Re-read Project Vision (on resume, MANDATORY)

After compaction, project vision and goals are lost from context. On every `resume`:

1. Read `project.vision` and `project.goals` from config.toml (local → global lookup)
2. Hold these in working memory for the remainder of the session
3. Use goals (G1-G5 etc.) to validate that current work aligns with project direction

This is a ~200 token re-read. Do it every time — the cost is negligible vs the risk of post-compaction drift.

## Backend Merge Sync (on resume)

On resume with `needs_backend: true`: check if backend PR merged via `gh pr view`, update state (`backend_merged: true`, `using_mocks: false`), create follow-up task to swap mocks for real API, re-run validation.

See [cross-project.md](cross-project.md) for details on the cross-project mocks/swap workflow.

## Read the builder crumb trail (before re-deriving context)

A prior session (or a self-handoff predecessor) can leave `builder_step` crumbs in the shared `.fno/events.jsonl`. That is the tried/found/fixed/outcome trail from its fix and revision rounds.

Read the current node's tail before re-deriving anything so a failed approach is not repeated. A malformed, rotated, or legacy uncorrelated row is skipped, never fatal.

```bash
EVENTS=".fno/events.jsonl"
NODE_ID="$(awk '/^graph_node_id:[[:space:]]*/ { sub(/^graph_node_id:[[:space:]]*/, ""); gsub(/["'\''\r]/, ""); print; exit }' .fno/target-state.md)"
# Parse each line independently (`-R` + `fromjson?`): a malformed or non-object
# line mid-file is skipped, not fatal - a bare `jq -c 'select(...)'` aborts the
# whole stream at the first bad line and silently drops every crumb after it.
CRUMBS="$( { [ -f "$EVENTS.1" ] && cat "$EVENTS.1"; [ -f "$EVENTS" ] && cat "$EVENTS"; } 2>/dev/null \
  | jq -Rc --arg node "$NODE_ID" \
    'fromjson? | select(.type? == "builder_step" and .data.node_id? == $node)' 2>/dev/null )"
N="$(printf '%s' "$CRUMBS" | grep -c . || true)"
if [ "${N:-0}" -gt 0 ]; then
  LAST="$(printf '%s\n' "$CRUMBS" | tail -1 | jq -r '.data.outcome // "?"' 2>/dev/null)"
  echo "crumbs: $N attempts, last outcome=$LAST"
  # Surface the most recent FAILED attempt so it is not re-tried blindly.
  printf '%s\n' "$CRUMBS" | jq -r 'select(.data.outcome=="failed")
    | "  last failed: tried \(.data.tried) - found \(.data.found // "?")"' 2>/dev/null | tail -1
else
  echo "crumbs: none"
fi
```

If a failed attempt is surfaced, carry it into working memory: do NOT repeat that
approach without stating why it is worth retrying. Zero crumbs is normal on a
fresh node - proceed with reduced context, no error.

## What "resume" actually does

Resume is not "restart with prior state." It is "continue from where the pipeline left off, using the immutable manifest." The pipeline reads:

- `plan_path` — used by every phase that touches the plan (read from the immutable manifest)
- `session_id` — used to key events and logs
