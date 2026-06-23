# Ship Phase: Rebase + Conflict Resolution

Reference for the target ship phase. Describes when and how to invoke
`fno pr rebase` and dispatch the conflict-resolver agent on exit 42.

## Preconditions (before entering the ship phase)

The ship phase creates the PR. Whatever is on the feature branch at ship
time is what ships. Therefore, before the ship phase fires:

1. The validate command has run locally green (tests / typecheck / build); CI
   green on the PR is verified by the loop-check verb at promise time.
2. Docs (architecture + how-to) are committed to the feature branch (unless
   `no_docs` is set) so they ride in the same PR, get reviewed alongside the
   code, and cannot be stranded by an auto-merge firing on external review.
3. Browser testing has run for `has_ui` surfaces (unless `no_browser` is set).
   Browser testing is advisory run-and-log: it never blocks the PR and is not
   a loop-check input, but running it before ship lets any findings ride in the
   same PR.

If the docs phase has not run and `no_docs` is not set, target should loop
back and run it before creating the PR. The ship phase itself should NOT
create missing docs - that would couple concerns and hide bugs in the phase
resolver.

## Link PR→node (right after /pr create, before any merge)

The moment `/pr create` returns the new PR number/URL - for EVERY ship run,
including `no-merge` - link it to the backlog node so the selection guard
(`_has_unmerged_open_pr`, `cli/src/fno/graph/cli.py:68`) sees the node is in
flight. Without this, a `no-merge` worker opens the PR, exits, its PID-based
`node:<id>` claim goes stale, and the node reads as fresh `ready` work again -
a duplicate dispatch then rebuilds the already-shipped work into a conflicting PR.

Do this BEFORE the optional rebase/merge step below, so the link lands even
when no merge ever happens:

```bash
# graph_node_id lives in the target-state.md manifest BODY (below the
# frontmatter) - the same field finalize.rs reads (finalize.rs:125-161).
# Strip leading space, quotes, and CR so the extraction is copy-paste robust.
NODE_ID=$(sed -n 's/^[[:space:]]*graph_node_id:[[:space:]]*//p' .fno/target-state.md 2>/dev/null | tr -d "'\"\r" | head -n 1)
if [[ -n "$NODE_ID" && "$NODE_ID" != "-" && "$NODE_ID" != "null" && -n "${PR_NUMBER:-}" ]]; then
  fno backlog update "$NODE_ID" --pr-number "$PR_NUMBER" --pr-url "$PR_URL" \
    || echo "ship: WARN failed to link PR #$PR_NUMBER to node $NODE_ID (non-fatal)" >&2
fi
```

Non-fatal: a failed link must never block the ship - it is a best-effort
in-flight signal. `_reconcile.py` still overwrites `pr_number` authoritatively
from merge ground truth at merge time, so a double-link cannot break close.

## When to Invoke fno pr rebase

Before every `fno pr merge` call, run:

```bash
fno pr rebase --base=origin/main
```

This ensures the feature branch is rebased onto fresh `origin/main` before
attempting merge. Without rebase, `gh pr merge` fails when another PR has
already merged to main.

## Exit Code Contract

| Exit | JSON status | Meaning | Action |
|------|-------------|---------|--------|
| 0 | `clean` | Rebase succeeded, no conflicts | Proceed to `fno pr merge` |
| 0 | `resolved` | Rebase complete after conflict resolution | Proceed to `fno pr merge` |
| 1 | `failed` | Conflict with `conflict_resolution: fail` | Abort; report to user |
| 1 | `refused` | Guardrail blocked auto-resolve | Abort; report files to user |
| 2 | `dirty` | Working tree has uncommitted changes | Abort; stash or commit first |
| 3 | `refused` | Called on main/master/develop/dev | Bug in caller; abort |
| 42 | `needs_resolver` | Conflicts present, guardrails passed | Dispatch conflict-resolver agent (see below) |

Parse the JSON from stdout to get the `status` field and the `files` list.
All human-readable messages from git go to stderr.

## Exit 42 Protocol: Dispatch conflict-resolver Agent

When `fno pr rebase` exits 42, the rebase is paused mid-flight with
conflict markers in the working tree. The caller must:

1. Parse stdout JSON to get `files` (list of conflicting paths) and `diff_preview`
2. Dispatch the `conflict-resolver` agent via the Task tool with a prompt like:

```
Resolve git rebase conflicts.

Conflicting files:
<files list from JSON>

Diff preview:
<diff_preview from JSON>

PR context:
<title and description from target-state.md if available>

Instructions:
- Read each conflicting file
- Resolve conflicts preserving both sides where semantically independent
- Stage each resolved file with `git add <file>`
- Commit each file separately: `git commit -m "resolve: <file> conflicts from rebase onto origin/main"`
- Do NOT run git rebase --continue or git rebase --abort
- Emit JSON summary on the last line of stdout
```

3. After the agent completes, call back:

```bash
fno pr rebase --continue
```

4. Repeat from step 1 if exit is 42 again (multi-patch rebase can surface
   conflicts in successive commits)

## Full Loop (Skill Pseudocode)

```bash
fno pr rebase --base=origin/main
exit_code=$?
stdout=$(...)  # capture stdout

while [[ $exit_code -eq 42 ]]; do
    files=$(echo "$stdout" | jq -r '.files[]')
    diff_preview=$(echo "$stdout" | jq -r '.diff_preview')

    # Dispatch via Task tool - wait for agent to complete
    dispatch_conflict_resolver "$files" "$diff_preview"

    fno pr rebase --continue
    exit_code=$?
    stdout=$(...)
done

if [[ $exit_code -ne 0 ]]; then
    report_failure "$(echo "$stdout" | jq -r '.status')" \
                   "$(echo "$stdout" | jq -r '.reason')"
    return
fi

# exit 0 - proceed to merge
fno pr merge --invoker=target "$PR_NUMBER"
```

## Guardrails (refuse list)

`fno pr rebase` refuses auto-resolution for these file types:

- Migration files: `**/migrations/**`, `schema.prisma`, `supabase/migrations/**`
- Secret / env files: `.env`, `*.env.*`, `**/secrets/**`
- Lock files: `package-lock.json`, `yarn.lock`, `Cargo.lock`, `Gemfile.lock`,
  `uv.lock`, `poetry.lock`
- Git config: `.gitattributes`, `.gitignore`
- Mass conflicts: any file with more than 3 `<<<<<<<` markers

When refused, `status` is `"refused"` and `files` lists the problem paths.
Report these to the user; do not retry.

## Design Note: No Standalone Agent Runner

The `conflict-resolver` agent is defined in `agents/conflict-resolver.md` and
is only invokable via the Claude Code Task tool from within a running skill
context. There is no standalone shell wrapper (`run-conflict-resolver.sh`).
`fno pr rebase` is intentionally mechanical - it detects the conflict state
and hands off via exit 42. The skill layer owns the Task dispatch.
