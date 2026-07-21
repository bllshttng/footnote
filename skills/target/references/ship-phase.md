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

## Preflight before every push (CI's verdict, earlier)

Distinct from the environment preflight in Step 3g (working-tree / deps / auth
checks): this is the deterministic **test-parity** runner that reproduces CI's
smoke + rust legs locally so a green here means a green PR, killing the
push-wait-red-fix loop where each ~10-minute CI round surfaces one new failure.

**Existence-guarded rule (repo-neutral, no hardcoded repo-root path):**

```bash
# Before the first PR push, and again before the push you expect to settle
# green. Skip when FNO_SKIP_PREFLIGHT=1 or the diff is docs-only. Capture the
# non-docs paths into a var (|| true) rather than piping grep inside the `if`
# condition - grep -q in a pipeline can SIGPIPE git diff under pipefail.
non_docs="$(git diff --name-only origin/main...HEAD | grep -vE '^(docs/|internal/|.*\.md$)' || true)"
if [[ "${FNO_SKIP_PREFLIGHT:-0}" != "1" && -x scripts/ci/preflight.sh && -n "$non_docs" ]]; then
  scripts/ci/preflight.sh; pf_rc=$?
  # Branch on the code. Collapsing every non-zero into "RED" is what sends a
  # loop hunting a test failure that does not exist: 5 means the run lost the
  # shared worktree and earned no verdict at all, and 3 means it never started.
  case "$pf_rc" in
    0) : ;;
    5) scripts/ci/preflight.sh || { echo "preflight VOID twice - shared worktree contention, retry later"; exit 1; } ;;
    3) echo "another preflight holds the lock - retry when it finishes"; exit 1 ;;
    *) echo "preflight RED - fix before pushing"; exit 1 ;;
  esac
fi
```

- **First push / settle-green push:** run `scripts/ci/preflight.sh` (full).
- **Between fix-loop commits** (external-review fixes, iteration pushes): run
  `scripts/ci/preflight.sh --retry-failed` for a fast re-check of only the
  steps that failed last time, then one **full** run before the push you expect
  to go green (a subset green is not a full green - the runner labels this).
- The guard is `-x scripts/ci/preflight.sh`, a relative existence check, so
  this no-ops in any repo that does not ship the script (the self-containment
  lint forbids repo-root-anchored script refs; the relative form is portable).
- Escape hatches are explicit and auditable: `FNO_SKIP_PREFLIGHT=1` shows in the
  transcript, and a docs-only diff (only `docs/`, `internal/`, `*.md`) skips by
  policy. The script itself never self-skips; the skip decision lives here.

See the repo-root `docs/preflight.md` for the full convention.

## Link PR→node (right after /pr create, before any merge)

The moment `/pr create` returns the new PR number/URL - for EVERY ship run,
including `no-merge` - link it to the backlog node so the selection guard
(`_has_unmerged_open_pr`, `cli/src/fno/graph/cli.py:68`) sees the node is in
flight. Without this, a `no-merge` worker opens the PR, exits, its PID-based
`node:<id>` claim goes stale, and the node reads as fresh `ready` work again -
a duplicate dispatch then rebuilds the already-shipped work into a conflicting PR.

This link is the linchpin of the promise-to-merge window: `pr-watch` discovery,
`fno backlog reconcile`, and merge-triggered auto-continue all find the PR via
`node.pr_number`. A dropped link blinds the whole chain for exactly that window,
so the step is **verified, not fire-and-forget**: write, read back, retry once,
and **refuse to promise** if it still did not stick (x-e106). Do this BEFORE the
optional rebase/merge step below, so the link lands even when no merge happens:

```bash
# graph_node_id lives in the target-state.md manifest BODY (below the closing
# frontmatter fence) - the same field finalize.rs reads (finalize.rs:125-161).
# Scope to the body so a frontmatter line (e.g. an `input:` value that happens
# to contain `graph_node_id:`) can never be grabbed first; strip quotes/CR and
# skip a `null` placeholder.
NODE_ID=$(awk '
  /^---[[:space:]]*$/ { fence++; next }            # frontmatter fences
  fence < 2 { next }                               # ignore everything above the body
  /^graph_node_id:[[:space:]]/ {
    sub(/^graph_node_id:[[:space:]]*/, ""); gsub(/[[:space:]"\047\r]/, "")
    if ($0 != "" && $0 != "null") { print; exit }
  }
' .fno/target-state.md 2>/dev/null)

if [[ -n "$NODE_ID" && "$NODE_ID" != "-" && -n "${PR_NUMBER:-}" ]]; then
  existing=$(fno backlog get "$NODE_ID" --field pr_number 2>/dev/null | tr -d '[:space:]' || true)
  if [[ -n "$existing" && "$existing" != "null" && "$existing" != "$PR_NUMBER" ]]; then
    # AC1-EDGE: an out-of-band authority (reconcile's merge ground truth) already
    # linked a DIFFERENT PR. Surface, do NOT overwrite - the node is linked and
    # the window is owned; reconcile stays the merge-time authority.
    echo "ship: node $NODE_ID already linked to PR #$existing (out-of-band authority); leaving as-is, not writing #$PR_NUMBER" >&2
  else
    link_ok=""
    for attempt in 1 2; do
      # Idempotent: re-writing the same PR_NUMBER converges (AC2-FR crash re-run).
      # The verify+retry below reports a real failure as a promise blocker, so
      # this swallow is bounded and visible.
      # lint-ok: fno-mutation-swallowed
      fno backlog update "$NODE_ID" --pr-number "$PR_NUMBER" --pr-url "$PR_URL" 2>/dev/null || true
      # `|| true`: a failing get (lock contention) must fall through to the retry
      # and ultimately the promise blocker, never abort the shell under set -e.
      got=$(fno backlog get "$NODE_ID" --field pr_number 2>/dev/null | tr -d '[:space:]' || true)
      [[ "$got" == "$PR_NUMBER" ]] && { link_ok=1; break; }
    done
    if [[ -z "$link_ok" ]]; then
      # AC2-ERR: the write did not stick after one retry. REFUSE TO PROMISE - an
      # unlinked node re-dispatches as duplicate work; blocking is cheaper than
      # the cleanup. Emit help and STOP; do NOT emit <promise>.
      echo "<help reason=\"pr-node-link-failed\" evidence=\"node $NODE_ID pr_number=${got:-<none>} expected=$PR_NUMBER\">PR #$PR_NUMBER did not link to backlog node $NODE_ID after one retry; refusing to promise. Fix then re-ship: fno backlog update $NODE_ID --pr-number $PR_NUMBER --pr-url $PR_URL</help>"
      exit 1
    fi
  fi
fi
```

The read-back is the gate: a `fno backlog update` that reports success but does
not persist (graph-lock contention, a partial write) is caught by re-reading
`pr_number` and comparing. `_reconcile.py` remains the merge-time authority on
`pr_number` - the verified retry never fights it (the different-value branch
above defers to it), and the same-value race converges benignly.

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
