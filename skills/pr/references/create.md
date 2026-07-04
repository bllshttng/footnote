
# Abilities Create PR

Create a PR using `gh` CLI.

**Model routing:** When spawned as a subagent (e.g., from target Phase 6), use `model: "haiku"` on the Agent call. Do NOT use `context: fork` - forking passes the parent's full context (potentially 300K+ tokens) into Haiku's 200K window, causing failures. Instead, spawn a fresh agent with only the gathered context from Step 1 below.

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

### 5. Create PR

```bash
# Build title from branch name or primary commit
TITLE="[type]: [description based on commits]"

gh pr create \
  --title "$TITLE" \
  --body "$(cat <<'EOF'
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
```

**Capture PR number** from the output URL (e.g., `/pull/105` → `105`).

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
gh pr create --title "title" --body "body"
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
/think → /blueprint → /do → /review → /pr create → /pr check
```

**Flow:**
1. `/pr create` runs as fresh Haiku agent with targeted context (branch, commits, plan summary)
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

