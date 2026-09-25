
# Abilities Create PR

Create a PR using `gh` CLI.

This create flow runs inline in the invoking session. It does not dispatch a worker, use a routed model lane, or call another skill at runtime.

**Draft first, then act.** The flow composes the title and body from local history. It does this before any step that can fail on the network, the remote, or the tree. Both drafts land in `.fno/pr-title.txt` and `.fno/pr-body.md` early. Every later stop prints the failed step, the reason, the title and the full body, says no PR was created, and ends:

```
RESULT: BLOCKED step=<step-name> reason=<one line> draft=.fno/pr-body.md
```

`RESULT: FAILED` stays only for a failure after `gh pr create` opened a PR (the unbound-PR case).

## Failure table

| Failure | Detected by | Action |
|---|---|---|
| no upstream | nothing; `fno do pr push` sets it | proceed |
| no remote | `git remote` prints nothing, or push exit 4 naming fetch | draft, `BLOCKED step=push`, name `git remote add origin <url>` then re-run `/pr create` |
| dirty tree | `git status --short` non-empty | draft from committed work only, list dirty paths, `BLOCKED step=clean-tree` |
| detached HEAD | `git rev-parse --abbrev-ref HEAD` prints `HEAD` | draft, `BLOCKED step=branch`, name one `git switch -c <name>` from the first commit subject |
| no base ref | `origin/main` does not resolve | base = first of `origin/HEAD`, `main`, `master` that resolves, else the root commit; the draft names it |
| git cannot run | a git call fails to exec | draft from file reads and the request, `BLOCKED step=git` |

## Process

### 1. Gather Context

```bash
# Get branch name (rev-parse works on older Git and in detached HEAD)
BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Resolve the base: $BASE if set and resolvable, else the first that resolves
if [ -z "${BASE:-}" ] || ! git rev-parse --verify --quiet "$BASE" >/dev/null 2>&1; then
  for cand in origin/main "$(git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null)" main master; do
    if git rev-parse --verify --quiet "$cand" >/dev/null 2>&1; then BASE="$cand"; break; fi
  done
fi
# An empty BASE means no base ref at all: the draft is built from the whole
# history (the root commit), and the reply names that.
if [ -n "${BASE:-}" ]; then
  COMMITS=$(git log "$BASE"..HEAD --oneline)
else
  COMMITS=$(git log --oneline | tail -5)
  BASE="<root commit; no base ref resolved>"
fi

# Check for any related plan files
ls -la .fno/*.md 2>/dev/null || echo "No plan files"
```

If a git call fails to execute at all (sandbox, missing binary), do not stop: draft from file reads and the request, and end `RESULT: BLOCKED step=git reason=<the git failure> draft=.fno/pr-body.md`.

### 2. Draft the Title and Body FIRST

Analyze the commits to build the PR title and description, and write both draft files. This happens before the pre-PR checks, local CI and the push, so a failed git step still leaves the user something to read and reuse.

```bash
# Get detailed commit messages for context
git log ${BASE:+$BASE..}HEAD --pretty=format:"- %s%n%b" | head -50
```

**Build the description from what the commits say:**
- Group related commits into summary bullets
- Use commit messages as the source of truth
- Do not invent features not in commits

**Write both drafts:**

```bash
TITLE="[type]: [description based on commits]"
printf '%s\n' "$TITLE" > .fno/pr-title.txt

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
# expansion). The closure trailer and the reviewed-at line are appended later,
# at the create step, since they matter only for a real PR.
printf '%s\n' "$BODY" > .fno/pr-body.md
```

`.fno/pr-title.txt` and `.fno/pr-body.md` are the draft of record. Every later step reuses them.

### 3. Pre-PR Checks

```bash
# Ensure we have commits to include
echo "Commits to include:"
git log ${BASE:+$BASE..}HEAD --oneline

# Ensure working tree is clean
git status --short
```

If uncommitted changes exist, do not stop empty: the draft already exists. List the dirty paths, and end:

```
RESULT: BLOCKED step=clean-tree reason=<dirty paths> draft=.fno/pr-body.md
```

Commit or stash nothing yourself - the draft comes from committed work only.

### 4. Run CI Validation (REQUIRED)

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

On a failed command, the draft already exists. Show the failed command and its error, then end:

```
RESULT: BLOCKED step=ci reason=<command> failed draft=.fno/pr-body.md
```

On success, proceed to push.

#### Step E: No CI Workflows Found

**If no `.github/workflows/` directory or no PR-triggered workflows exist:**

```
⚠️ No CI workflows found in .github/workflows/

Skipping CI validation - no automated checks configured.
```

Proceed to push.

---

### 5. Push Branch

```bash
# Fetch, bring in origin/main (rebase, or merge when the branch holds merges), push once
fno do pr push
```

Handle the verb's refusals like any other refusal here. **Every push refusal keeps the draft.** Report it instead of stopping empty. Exit 3 names the fix: a protected branch, a dirty tree, or a conflict. A conflict leaves the rebase in progress: resolve it, run `fno do pr rebase --continue`, then re-run the push. A branch that already merges origin/main uses merge, not rebase. If that merge conflicts, abort it. Merge origin/main by hand, commit, then re-run the push. Exit 2 means a CI run is in flight: wait with `fno do pr wait <n> --until settled`, then re-run. Exit 1 means preflight is red. A failed fetch (exit 4) names the missing remote. The reply then names `git remote add origin <url>` and ends `RESULT: BLOCKED step=push reason=<verb refusal> draft=.fno/pr-body.md`. Do not open the PR yet.

If `git rev-parse --abbrev-ref HEAD` printed `HEAD` (detached), the draft already exists. Name one `git switch -c <name>` from the first commit subject and end `RESULT: BLOCKED step=branch reason=detached HEAD draft=.fno/pr-body.md`. Never create the branch yourself.

### 6. Out-of-scope items are born tracked

**Purpose:** the CI gate `check-oos-tracked.sh` reds any PR whose body has an "Out of scope" / "Not touched here" / "Not in this PR" section containing an item with no tracked reference. This step makes every such item born tracked so the PR lands gate-green. It is advisory and best-effort, with the same error posture as the body check. A tracking failure degrades to today's behavior: a red gate for a human to resolve. It NEVER blocks or fails the PR.

**Trigger:** the draft body you composed already contains a matching ATX heading. The match is case-insensitive: `Out of scope`, `Out-of-scope`, `Not touched here`, or `(Explicitly) not in this PR`. Never invent such a section. Never invent items. The section exists only for genuinely deferred work grounded in the commits, the plan, or the dispatch context.

When the plan frontmatter says `carveouts: forbidden`, filing a node for an exclusion does not make the PR mergeable. The plan fidelity gate refuses any PR that declares an exclusion section.

Read the graph node id once (the same value the closure step reads):

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
     If `NEW_ID` matches the tracked-ref grammar (`<prefix>-<hex>`), rewrite the item line so it ends ` - tracked as $NEW_ID`. If it is empty or malformed, treat this as a filing failure and drop to the next step - never append an empty ` - tracked as `.
3. If `fno backlog idea` fails or yields no usable id, leave it untracked and print a `warn:` line naming it. State that no tracking object was created and name the failure mode (lock contention, missing CLI). Degrade loud, not silent, and continue. Do NOT mint a carveout to repair the citation: `fno backlog carveout add` records a superuser's deliberate decision to defer substantial work. Filing one because a command failed mutates graph state purely to make prose pass validation. The CI gate is the backstop and will red the check for a human. That human fixes the work inline, cuts the line, or creates and cites tracking deliberately. NEVER write an `oos-ok:` waiver to route around a tooling failure. A waiver asserts "nothing to track" - a judgment a tooling error cannot establish. That call is a human's, never the worker's.

**Idempotent by construction:** step 1 skips any item that already carries a tracked reference, so a re-run over a body whose items already read `- tracked as <id>` files nothing.

**Report** each action as `item -> <id>` (and any `warn:` lines) so the dispatcher transcript shows exactly what was filed. Then continue to the next step with the rewritten `.fno/pr-body.md`.

### 7. Add the exact Fixes closure line

**Purpose:** a PR body naming several nodes in prose only ever closed the ONE node stamped in the bind step. Every other named node stayed open forever. The exact closure line (`Fixes <id> [<id>...]`) is what the merge-time reconcile binds. Free-text mentions never count.

Reuse `$NODE_ID` from the previous step (or read it fresh the same way). Render the trailer - the node plus every `contained_in` descendant already in the graph - and separate the two failures by exit code:

```bash
if [[ -n "$NODE_ID" && "$NODE_ID" != "null" ]]; then
  CLOSURE_TRAILER=$(fno do pr closure-trailer "$NODE_ID"); RC=$?
else
  CLOSURE_TRAILER=$(fno do pr closure-trailer); RC=$?
fi
if [[ $RC -eq 4 ]]; then
  echo "fail: closure trailer: the graph reader is dead (exit 4); nothing downstream is trustworthy" >&2
  echo "repair: bring the graph store back (probe fno-agents-worker --store-keeper), then re-run this step" >&2
  exit 1
elif [[ $RC -ne 0 ]]; then
  echo "warn: closure trailer: current branch resolves to no single real node; the CI annotation carries the remedy" >&2
fi
```

When `$NODE_ID` is empty or unresolvable, the bare verb resolves the node from the current branch instead. It demands exactly one real node, the same carrier the CI gate reads. An empty `$CLOSURE_TRAILER` now means exactly one thing: a readable graph that carries no matching node. That is why the empty case stays safe to append unconditionally. Exit 4 is different: the graph read itself failed. A dead reader once answered exactly like a missing node and three PRs shipped red on the closure gate with no named cause (measured 2026-09-16). On exit 4 STOP: do not create the PR. The draft exists, so end `RESULT: BLOCKED step=closure-trailer reason=graph reader dead (exit 4) draft=.fno/pr-body.md`. Any other nonzero prints the `warn:` and continues. The verb is a moved spelling: expect one `is now` deprecation notice on stderr and treat it as expected output, never as a failure signal. Most genuine extra deliveries ARE `contained_in` already. On the rare case where the commits or plan show a real extra one, add it explicitly: `fno do pr closure-trailer "$NODE_ID" --extra <other-id>`.

Append the non-empty `$CLOSURE_TRAILER` as its own paragraph at the end of `.fno/pr-body.md`, before calling `gh pr create`. Never hand-write the trailer. Never add an id this command did not produce: a wrong id silently binds the wrong node at merge.

### 8. Create PR

```bash
# Reuse the drafts written at step 2
TITLE="$(cat .fno/pr-title.txt)"
BODY="$(cat .fno/pr-body.md)"

# The reviewed-at line: the one claim a worker appends to the body, and only
# this verb authors it. It prints the line only when the attestation journal
# holds a clean `pass` attestation pinned to exactly this branch and HEAD, so
# a PR that arrives unreviewed opens with no Review section at all - the merge
# gate reads the journal, never this line. Idempotent like the trailer: a
# re-run over a body already carrying `## Review` adds no second one.
REVIEW_LINE="$(fno-agents review-summary \
  --branch "$(git rev-parse --abbrev-ref HEAD)" \
  --head "$(git rev-parse HEAD)" 2>/dev/null || true)"
if [[ -n "$REVIEW_LINE" && "$BODY" != *"## Review"* ]]; then
  BODY="${BODY}

## Review

${REVIEW_LINE}"
fi

if [[ -n "${CLOSURE_TRAILER:-}" ]]; then
  BODY="${BODY}

${CLOSURE_TRAILER}"
fi

# --body-file, not --body: the git-protection hook judges a --body-file on the
# file's own trailer, where a "$BODY" variable it cannot expand leaves it
# guessing from the command text. Write the composed body, then pass the path.
printf '%s\n' "$BODY" > .fno/pr-body.md

# The body-only CI guards are pure functions of the body, title and branch,
# so run them here, before the PR exists: a body failure found in CI costs a
# full workflow round and is indistinguishable from a code failure.
fno-agents pr-body-check --body-file .fno/pr-body.md --title "$TITLE" --base "${BASE:-main}"; RC=$?
if [[ $RC -eq 1 ]]; then
  echo "fail: PR body: a CI guard refuses this PR; follow the guard's own fix text above, then rerun the check" >&2
  exit 1
elif [[ $RC -ne 0 ]]; then
  echo "warn: PR body check could not run (exit $RC); CI still runs every body guard" >&2
fi

gh pr create \
  --title "$TITLE" \
  --body-file .fno/pr-body.md
```

On body-check exit 1, follow the guard's own fix text and rerun the check. If the guard still refuses, end `RESULT: BLOCKED step=body-check reason=<the guard's fix text> draft=.fno/pr-body.md`. Never open the PR: the CI guards read the PR body field, so no commit can fix a body failure. The session-URL guard also scans commit messages, so a commit hit needs a reword, not a body edit.

**Capture PR number** from the output URL (e.g., `/pull/105` → `105`).

**Verify the trailer round-tripped.** Best-effort, non-fatal, same posture as the OOS step. A mismatch here means the body `gh pr create` actually wrote differs from what was composed. Merge-time binding then misses a claim silently.

```bash
if [[ -n "${CLOSURE_TRAILER:-}" && -n "${PR_NUMBER:-}" ]]; then
  ACTUAL_BODY=$(gh pr view "$PR_NUMBER" --json body -q .body)
  if ! printf '%s' "$ACTUAL_BODY" | grep -qF "$CLOSURE_TRAILER"; then
    echo "warn: PR #$PR_NUMBER body does not contain the composed trailer verbatim - merge-time closure may miss a claim" >&2
  fi
fi
```

### 9. Bind the created PR to its backlog node

Bind the just-opened PR to its node through the one shared binder. That makes the dispatcher's selection guard (`_has_unmerged_open_pr`) and `fno backlog reconcile` see the in-flight PR. Otherwise the node's `pr_number` stays null through the whole review window. A lapsed claim then lets the 5-min dispatcher re-spawn a finished node.

When the manifest names a real node, the manifest node LEADS. When it does not, the branch is the fallback. That covers a PR whose manifest was never stamped. Both routes reach the same atomic writer, so the graph never sees two stamping paths. The binder stamps the ship lifecycle row itself. No second provenance stamp follows it.

```bash
if [[ -n "${PR_NUMBER:-}" ]]; then
  NODE_ID=$(sed -n 's/^[[:space:]]*graph_node_id:[[:space:]]*//p' .fno/target-state.md | head -1 | tr -d "\"'")
  BIND_ARGS=(fno do pr bind-created --url "$PR_URL" --repo "$(pwd)")
  if [[ -n "$NODE_ID" && "$NODE_ID" != "null" ]]; then
    BIND_ARGS+=(--node "$NODE_ID")
  fi
  if BIND_RECEIPT=$("${BIND_ARGS[@]}"); then
    echo "node<->PR bound: $BIND_RECEIPT"
  else
    # The PR EXISTS. An unbound PR is incomplete delivery, not a failed create:
    # name the PR and the one exact repair command (idempotent, safe to rerun
    # as-is). This is the one case that stays RESULT: FAILED - a PR was opened.
    echo "PR #$PR_NUMBER created but UNBOUND: $PR_URL" >&2
    echo "repair: ${BIND_ARGS[*]}" >&2
  fi
fi
```

This is the *fast path* only: it engages `in_review` mid-session, before the next dispatch selection. `fno-agents finalize` re-runs the same link at every terminal loop decision as a deterministic backstop (`stamp_node_pr` in crates/fno-agents/src/finalize.rs). A skipped step here still gets stamped at session end. Idempotent.

### 10. Report the result (RESULT contract)

After creating the PR, state the human-readable line AND emit the machine-readable `RESULT:` contract as your final line. The invoking workflow uses it to distinguish success from failure.

On success:

```
PR #[NUMBER] created: https://github.com/[owner]/[repo]/pull/[NUMBER]

Next step: Run /pr check [NUMBER] to wait for external review

RESULT: SUCCESS pr=#[NUMBER] url=https://github.com/[owner]/[repo]/pull/[NUMBER]
```

If PR creation did not complete, do NOT print a success line. A stop before `gh pr create` is BLOCKED (the draft exists - see the failure table). `RESULT: FAILED` stays only for a failure after a PR opened. That means an unbound PR in the bind step, or a `gh pr create` error that left a half-open PR:

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
# The body must carry the exact closure trailer when the branch names a node,
# or check-pr-node-closure reds the PR. Compose it into a file (see the
# create step above) rather than passing a bare --body.
fno-agents pr-body-check --body-file .fno/pr-body.md --title "title" --base "${BASE:-main}"
gh pr create --title "title" --body-file .fno/pr-body.md
```

### Get Detailed Commit Log
```bash
git log ${BASE:+$BASE..}HEAD --pretty=format:"- %s%n%b"
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
1. `/pr create` runs this flow inline in the invoking session
2. `/pr check` polls for external review and processes feedback
3. Human reviewer merges

---

## Key Principles

- **Draft first** - the title and body exist before any step that can fail. A blocked create still hands the user something to read and reuse
- **Commits tell the story** - PR description comes from `git log`, not imagination
- **Clear PR titles** - Start with type based on commit types
- **Meaningful descriptions** - Derived from actual changes made
- **Reference Linear tickets** - Extract from commits if present (only when `config.linear.enabled`)
- **Output PR number clearly** - Needed for `/pr check`
