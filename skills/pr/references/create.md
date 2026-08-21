
# Abilities Create PR

Create a PR using `gh` CLI.

**Model routing:** This worker runs on the declared `pr-create` role, resolved through `config.model_routing` (`config.model_routing.roles.pr-create`). An explicit configured route wins; with no route configured it runs on the invoking harness's primary model - no tier or model literal is hardcoded. Declare the role at the spawn boundary (`fno agents spawn --role pr-create`, or omit any `model:` override so the resolved route selects the model). Do NOT use `context: fork` - forking passes the parent's full context into the worker; instead spawn a fresh agent with only the gathered context from Step 1 below.

## Process

### 1. Gather Context

```bash
# Get branch name (rev-parse works on older Git and in detached HEAD)
BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Get commits since main (these tell the story)
COMMITS=$(git log origin/main..HEAD --oneline)

# Check for any related plan files
ls -la .fno/*.md 2>/dev/null || echo "No plan files"
```

### 2. Pre-PR Checks

```bash
# Ensure we have commits to push
echo "Commits to include:"
git log origin/main..HEAD --oneline

# Ensure working tree is clean
git status --short
```

If uncommitted changes exist, stop and report - don't create incomplete PR. Emit `RESULT: FAILED uncommitted changes in working tree` as your final line so a dispatcher does not report a PR that was never opened.

### 2.5 Run CI Validation (REQUIRED)

**Purpose:** Run the same checks CI will run to catch failures before push.

#### Step A: Discover CI Configuration

```bash
# Find CI workflow files
ls .github/workflows/*.yml .github/workflows/*.yaml 2>/dev/null
```

#### Step B: Read and Parse Workflows

**If workflow files exist:**

1. Read each `.yml`/`.yaml` file in `.github/workflows/`
2. Check the `on:` trigger - only consider workflows that run on:
   - `pull_request`
   - `push` to main/master
   - NOT `workflow_dispatch` only (manual triggers don't count)
3. For matching workflows, extract all `run:` commands from job steps
4. Ignore setup actions (anything with `uses:` like `actions/checkout`, `actions/setup-node`)

**Example workflow parsing:**
```yaml
# .github/workflows/ci.yml
on:
  pull_request:
  push:
    branches: [main]

jobs:
  test:
    steps:
      - uses: actions/checkout@v4      # IGNORE (setup action)
      - uses: actions/setup-node@v4    # IGNORE (setup action)
      - run: npm ci                    # EXTRACT → run locally
      - run: npm run build             # EXTRACT → run locally
      - run: npm run test              # EXTRACT → run locally
```

#### Step C: Run Extracted Commands

Run the extracted `run:` commands in order. Skip dependency install commands if deps are already installed:
- `npm ci` / `npm install` → skip if `node_modules` exists and is recent
- `pip install` → skip if in active venv with deps

**Report what you're running:**
```
🔍 Found CI workflow: .github/workflows/ci.yml
📋 Commands to run:
   1. npm run build
   2. npm run test

Running CI validation...
```

#### Step D: Handle Results

**If any command fails:**
```
❌ CI validation failed

Command: npm run build
Exit code: 2
Error: [actual error output]

Fix the issues before creating PR.
```

**STOP here - do not push or create PR until CI passes locally.** Emit `RESULT: FAILED CI validation failed: <command>` as your final line so a dispatcher does not report a PR that was never opened.

**If all commands succeed:**
```
✅ CI validation passed (2/2 commands)
   ✓ npm run build
   ✓ npm run test

Proceeding to push...
```

#### Step E: No CI Workflows Found

**If no `.github/workflows/` directory or no PR-triggered workflows exist:**

```
⚠️ No CI workflows found in .github/workflows/

Skipping CI validation - no automated checks configured.
Consider adding CI workflows to catch issues early.
```

Proceed to push (user has chosen not to have CI).

---

### 3. Push Branch

```bash
# Push with upstream tracking
git push -u origin "$(git rev-parse --abbrev-ref HEAD)"
```

### 4. Generate PR Description from Commits

Analyze the commits to build the PR description:

```bash
# Get detailed commit messages for context
git log origin/main..HEAD --pretty=format:"- %s%n%b" | head -50
```

**Build the description from what the commits say:**
- Group related commits into summary bullets
- Use commit messages as the source of truth
- Don't invent features not in commits

### 4.5 Out-of-scope items are born tracked

**Purpose:** the CI gate `check-oos-tracked.sh` reds any PR whose body has an "Out of scope" / "Not touched here" section containing an item with no tracked reference. This step makes every such item born tracked so the PR lands gate-green. Advisory and best-effort - identical error posture to step 5.5: a tracking failure degrades to today's behavior (a red gate for a human to resolve), NEVER a blocked or failed PR.

**Fires ONLY when** the description you just composed already contains an ATX heading matching (case-insensitive) `Out of scope`, `Out-of-scope`, or `Not touched here`. Never invent such a section, and never invent items - the section exists only when there is genuinely deferred work grounded in the commits, the plan, or the dispatch context.

Read the graph node id once (the same value step 5.5 reads):

```bash
NODE_ID=$(sed -n 's/^[[:space:]]*graph_node_id:[[:space:]]*//p' .fno/target-state.md 2>/dev/null | head -1 | tr -d "\"'")
```

For each item line under that heading, in order:

1. **Cite first.** If a `<prefix>-<hex>` node id, a `cv-<hex>` carveout id, or an inline `oos-ok: <rationale>` is **already on the item line**, leave the line byte-identical (idempotent). If the item's deferred work is tracked **elsewhere** - a node/`cv-` id in the plan frontmatter, a commit trailer, or `.fno/carveouts.jsonl` - append that existing id to the line (` - tracked as <id>`) rather than filing a new one; the gate reads only the item line, so an off-line citation must be brought onto it. File nothing new in either case.
2. **File second, with inherited weight.** Otherwise classify the item and file it. Strip markdown/backticks AND shell metacharacters (`` ` ``, `$`, `"`) from the item text so nothing can break out of the quoting or trigger `$(...)` expansion, and pass a concise plain title (the trimmed item line, not the whole paragraph) as a double-quoted argument:
   - a pre-existing **bug** being deferred (a missed defect, not a new feature):
     ```bash
     # Branch rather than build an argv array. An unquoted ${VAR:+...} splits
     # into two argv entries under bash but not zsh; and NO array form is
     # portable either - plain "${a[@]}" errors under bash set -u, while the
     # guarded "${a[@]+...}" form passes one EMPTY argument under zsh.
     if [ -n "$NODE_ID" ]; then
       RECEIPT=$(fno backlog idea "<item title>" -t task -p p2 --description "deferred from PR: <pr title> (<branch>)" --parent "$NODE_ID")
     else
       RECEIPT=$(fno backlog idea "<item title>" -t task -p p2 --description "deferred from PR: <pr title> (<branch>)")
     fi
     ```
   - a genuine **nice-to-have / future feature**: the same command with `-p p3` (and drop `-t task`).
   - Extract the id from the JSON receipt and **validate it before appending** - a command can exit 0 yet print an empty/unparsable receipt, and appending a blank id leaves the line untracked while reading as cited:
     ```bash
     NEW_ID=$(printf '%s' "$RECEIPT" | grep -o '"id": *"[^"]*"' | head -1 | sed -E 's/.*"id": *"([^"]*)".*/\1/')
     ```
     If `NEW_ID` matches the tracked-ref grammar (`<prefix>-<hex>`), rewrite the item line so it ends ` - tracked as $NEW_ID`. If it is empty or malformed, treat this as a filing failure and drop to step 3 - never append an empty ` - tracked as `.
3. **Degrade loud, not silent.** If `fno backlog idea` exits non-zero (lock contention, missing CLI) OR returned no usable id above, leave the item untracked, print a `warn:` line naming it and stating that no tracking object was created, and continue. Do NOT mint a carveout to repair the citation: `fno carveout add` records an operator's deliberate decision to defer substantial work, so filing one because a command failed mutates graph state purely to make prose pass validation. The CI gate is the backstop and will red the check for a human, who fixes the work inline, cuts the line, or creates and cites tracking deliberately. NEVER write an `oos-ok:` waiver to route around a tooling failure - a waiver asserts "nothing to track", a judgment a tooling error cannot establish; that call is a human's, never the worker's.

**Idempotent by construction:** step 1 skips any item that already carries a tracked reference, so a re-run over a body whose items already read `- tracked as <id>` files nothing.

**Report** each action as `item -> <id>` (and any `warn:` lines) so the dispatcher transcript shows exactly what was filed. Then continue to step 5 with the rewritten body.

### 4.6 Add the exact Backlog-Closure trailer

**Purpose:** a PR body naming several nodes in prose only ever closed the ONE node stamped in step 5.5. Every other named node stayed open forever. The exact `Backlog-Closure:` trailer is what the merge-time reconcile binds. Free-text mentions never count.

Reuse `$NODE_ID` from step 4.5 (or read it fresh the same way). Render the trailer - the node plus every `contained_in` descendant already in the graph:

```bash
if [[ -n "$NODE_ID" && "$NODE_ID" != "null" ]]; then
  CLOSURE_TRAILER=$(fno do pr closure-trailer "$NODE_ID")
fi
```

When `$NODE_ID` is empty or unresolvable, `fno do pr closure-trailer` prints nothing on stdout (not an error), so `$CLOSURE_TRAILER` is safe to append unconditionally. The verb is a moved spelling: expect one `is now` deprecation notice on stderr and treat it as expected output, never as a failure signal. Most genuine extra deliveries ARE `contained_in` already. On the rare case where the commits or plan show a real extra one, add it explicitly: `fno do pr closure-trailer "$NODE_ID" --extra <other-id>`.

Append the non-empty `$CLOSURE_TRAILER` as its own paragraph at the end of the body composed in step 5, before calling `gh pr create`. Never hand-write a `Backlog-Closure:` line. Never add an id this command did not produce: a wrong id silently binds the wrong node at merge.

### 5. Create PR

```bash
# Build title from branch name or primary commit
TITLE="[type]: [description based on commits]"

BODY="$(cat <<'EOF'
## Summary

[2-4 bullets derived from commit messages]

## Changes

[List key files/components changed based on commits]

## Test Plan

- [ ] [How to verify - based on what commits touched]

## Linear

[{TEAM}-XXX](https://linear.app/{workspace}/issue/{TEAM}-XXX) (only if Linear configured and ticket exists in commits)
EOF
)"
# The quoted heredoc above is a literal template (bracket placeholders, no
# expansion) - $CLOSURE_TRAILER never belongs inside it. Append it as its own
# paragraph afterward, with a real (unquoted) variable expansion instead.
if [[ -n "${CLOSURE_TRAILER:-}" ]]; then
  BODY="${BODY}

${CLOSURE_TRAILER}"
fi

# --body-file, not --body: the git-protection hook judges a --body-file on the
# file's own trailer, where a "$BODY" variable it cannot expand leaves it
# guessing from the command text. Write the composed body, then pass the path.
printf '%s\n' "$BODY" > .fno/pr-body.md
gh pr create \
  --title "$TITLE" \
  --body-file .fno/pr-body.md
```

**Capture PR number** from the output URL (e.g., `/pull/105` → `105`).

**Verify the trailer round-tripped.** Best-effort, non-fatal, same posture as step 4.5. A mismatch here means the body `gh pr create` actually wrote differs from what was composed. Merge-time binding then misses a claim silently.

```bash
if [[ -n "${CLOSURE_TRAILER:-}" && -n "${PR_NUMBER:-}" ]]; then
  ACTUAL_BODY=$(gh pr view "$PR_NUMBER" --json body -q .body)
  if ! printf '%s' "$ACTUAL_BODY" | grep -qF "$CLOSURE_TRAILER"; then
    echo "warn: PR #$PR_NUMBER body does not contain the composed Backlog-Closure trailer verbatim - merge-time closure may miss a claim" >&2
  fi
fi
```

### 5.5 Link the PR to the backlog node

If `.fno/target-state.md` carries a `graph_node_id`, stamp the node so the
dispatcher's selection guard (`_has_unmerged_open_pr`) and `fno backlog
reconcile` see the in-flight PR - otherwise the node's `pr_number` stays null
through the whole review window and a lapsed claim lets the 5-min dispatcher
re-spawn a finished node (x-a166). Best-effort: a stamp failure is logged, never
fatal; re-stamping the same PR is a no-op.

This is the *fast path* only: `fno-agents finalize` re-runs the same link at every terminal loop decision as a deterministic backstop (`stamp_node_pr`), so a skipped step here still gets stamped at session end. Idempotent.

```bash
NODE_ID=$(sed -n 's/^[[:space:]]*graph_node_id:[[:space:]]*//p' .fno/target-state.md | head -1 | tr -d "\"'")
if [[ -n "$NODE_ID" && "$NODE_ID" != "null" && -n "$PR_NUMBER" ]]; then
  fno backlog update "$NODE_ID" --pr-number "$PR_NUMBER" --pr-url "$PR_URL" \
    || echo "warn: node<->PR stamp failed for $NODE_ID PR #$PR_NUMBER (PR still created)" >&2
  # Ship-phase provenance is no longer a separate stamp: the `update --pr-number`
  # above fires it on the pr_number unset->set transition (ambient identity), so
  # a `session add --phase ship` here would double-fire.
fi
```

### 6. Report the result (RESULT contract)

After creating the PR, state the human-readable line AND emit the machine-readable `RESULT:` contract as your FINAL line. A dispatcher (e.g. `/pr create`) parses the `RESULT:` line to decide success vs failure; without it, a successfully-opened PR can be misread as a failed worker.

On success:

```
PR #[NUMBER] created: https://github.com/[owner]/[repo]/pull/[NUMBER]

Next step: Run /pr check [NUMBER] to wait for external review

RESULT: SUCCESS pr=#[NUMBER] url=https://github.com/[owner]/[repo]/pull/[NUMBER]
```

If PR creation could not complete (uncommitted changes, CI validation failed, or `gh pr create` errored), do NOT print a success line. Emit the failure contract as your final line instead, so the dispatcher does not report a PR that does not exist:

```
RESULT: FAILED <one-line reason>
```

---

## Title Convention

Derive from commits:
- `feat:` - New functionality
- `fix:` - Bug fixes
- `chore:` - Maintenance, docs, refactoring
- `refactor:` - Code restructuring

---

## Command Reference

### Create PR
```bash
# The body must carry the exact `Backlog-Closure:` trailer when the branch names
# a node, or check-pr-node-closure reds the PR. Compose it into a file (see the
# step-5 block above) rather than passing a bare --body.
gh pr create --title "title" --body-file .fno/pr-body.md
```

### Get Detailed Commit Log
```bash
git log origin/main..HEAD --pretty=format:"- %s%n%b"
```

### Check Existing PR
```bash
gh pr view --json number,url
```

---

## Integration with Workflow

```
/think → /blueprint → /execute → /review → /pr create → /pr check
```

**Flow:**
1. `/pr create` runs as a fresh, role-routed `pr-create` worker with targeted context (branch, commits, plan summary)
2. `/pr check` polls for external review and processes feedback
3. Human reviewer merges

---

## Key Principles

- **Commits tell the story** - PR description comes from `git log`, not imagination
- **Always push first** - Can't create PR without commits on remote
- **Clear PR titles** - Start with type based on commit types
- **Meaningful descriptions** - Derived from actual changes made
- **Reference Linear tickets** - Extract from commits if present (only when `config.linear.enabled`)
- **Output PR number clearly** - Needed for `/pr check`
