# Testing Guide

Testing guide for the footnote Claude Code plugin. Covers test infrastructure, validation scripts, quality gates, review agents, verification levels, iteration protocols, and testing patterns.

---

## Table of Contents

- [Test Infrastructure](#test-infrastructure)
  - [Shell Test Scripts](#shell-test-scripts)
  - [Validation Scripts](#validation-scripts)
  - [Python Utilities](#python-utilities)
- [Running Tests](#running-tests)
- [TDD Discipline](#tdd-discipline)
- [Quality Gates](#quality-gates)
- [Review Agent Testing](#review-agent-testing)
- [Verification Levels](#verification-levels)
- [Iteration Protocols](#iteration-protocols)
  - [Fix Loop](#fix-loop)
  - [Debug Loop](#debug-loop)
  - [Target Loop](#target-loop)
- [Test Patterns](#test-patterns)
  - [State File Testing](#state-file-testing)
  - [Hook Testing](#hook-testing)
  - [Cross-CLI Testing](#cross-cli-testing)
- [Writing New Tests](#writing-new-tests)
- [Troubleshooting](#troubleshooting)

---

## Test Infrastructure

### Shell Test Scripts

Located in `scripts/`, these shell test scripts cover core plugin features:

| Script | Purpose |
|--------|---------|
| `test_stop_hook_events.sh` | Stop hook event validation and state transitions |
| `test-target-state-recovery.sh` | State file recovery after crashes or corruption |
| `test-thrashing-detection.sh` | Thrashing loop detection (unit) |
| `test-thrashing-integration.sh` | Thrashing loop detection (integration, end-to-end) |
| `test-parallel-wave-conflicts.sh` | Parallel wave conflict detection for shared outputs |
| `test-scan-antipatterns.sh` | Antipattern scanning correctness in plans |
| `test-sync-codex-agents.sh` | Codex agent synchronization from shared sources |
| `test-sync-gemini-agents.sh` | Gemini agent synchronization from shared sources |
| `test-validate-plan.sh` | Plan validation logic and format checking |

All test scripts follow the same convention:

- Exit 0 on success, non-zero on failure
- Print pass/fail status for each assertion
- Can be run standalone from the repo root

### Validation Scripts

Three validation scripts enforce structural and process rules:

| Script | Purpose |
|--------|---------|
| `validate-test-first.sh` | PreToolUse hook that blocks build/start commands when no recent test run detected (checks `.fno/.last-test-run` marker) |
| `validate-plan.sh` | Plan format validation, antipattern scanning, stub detection |
| `validate-roadmap.sh` | Roadmap structure validation - checks `00-INDEX.md` format and task references |

### Python Utilities

| Script | Purpose |
|--------|---------|
| `skills/do/orchestrator.py` | Wave orchestration engine with a testable CLI interface |
| `scripts/roadmap-tasks.py` | Task lifecycle management - status tracking, dependency resolution |
| `scripts/metrics/register-task.py` | Task registration for metrics tracking |
| `scripts/metrics/session-cost.py` | Session cost calculation and reporting |

### Supporting Infrastructure

| Component | Path | Purpose |
|-----------|------|---------|
| Events library | `scripts/lib/events.sh` | Event logging for test assertions |
| Config library | `scripts/lib/config.sh` | Settings reader used in tests |
| Config tests | `scripts/lib/test_config.sh` | Tests for the config library itself |
| Flock library | `scripts/lib/flock.sh` | File locking for concurrent test safety |

---

## Running Tests

### Quick Start

```bash
# Run all test scripts
for f in scripts/test*.sh; do
  echo "--- Running: $f ---"
  bash "$f"
  echo ""
done

# Run a specific test
bash scripts/test-thrashing-detection.sh

# Run the config library tests
bash scripts/lib/test_config.sh
```

### Orchestrator CLI

```bash
# Show help and available options
python skills/do/orchestrator.py --help

# Parse and validate a plan's execution strategy
python skills/do/orchestrator.py path/to/00-INDEX.md

# Route a task description to the correct agent
python skills/do/orchestrator.py --agent "Build React component" --tags ui,frontend
```

### Validation Scripts

```bash
# TDD guard: blocks build commands when tests haven't been run
# (runs as PreToolUse hook, not standalone - shown here for reference)
# ./scripts/validate-test-first.sh

# Validate a plan file for format issues and antipatterns
./scripts/validate-plan.sh path/to/plan-directory

# Validate roadmap structure
./scripts/validate-roadmap.sh path/to/roadmap

# Scan for antipatterns in plans
./scripts/scan-antipatterns.sh path/to/plan-directory
```

### Metrics and Health

```bash
# Track session costs
./scripts/metrics/cost-tracker.sh

# Register a completed task
python scripts/metrics/register-task.py --task-id 1.1 --status done

# Run doctor health check for the plugin installation
./scripts/doctor.sh
```

---

## TDD Discipline

The footnote plugin enforces strict test-first development in all archer agents. Every implementation task must follow the Red-Green-Commit cycle.

### The Cycle

1. **Write failing test (RED)** - Write the test that describes the expected behavior before any implementation code exists.

2. **Run test - verify it fails** - Confirm the test fails for the right reason (missing function, wrong return value) - not for an unrelated error like a syntax mistake.

3. **Write minimal implementation (GREEN)** - Write only enough code to make the test pass. No extra features, no premature optimization.

4. **Run test - verify it passes** - Confirm the test now passes. If it does not, fix the implementation (not the test).

5. **Verify database/state** - For stateful operations, verify actual database or file state. Do not rely solely on UI assertions or mocked returns.

6. **Atomic commit** - Commit the test and implementation together in a single commit. This preserves the linkage between what was tested and what was built.

### Enforcement Mechanisms

#### 1. validate-test-first.sh (PreToolUse Build Guard)

A PreToolUse hook that intercepts Bash commands and blocks build/start commands when tests haven't been run recently.

**How it works:**
- Reads JSON from stdin containing `tool_input.command`
- Maintains an allowlist of always-permitted commands (test runners, git, ls, cat, curl, etc.)
- For build/start commands (`npm run build`, `npm start`, etc.), checks for `.fno/.last-test-run` marker file
- **Blocks** the command if no recent test run detected, requiring tests to be run first
- Does NOT inspect git history or commits - it's a real-time hook guard

#### 2. Agent-Level Enforcement

The target and archer agents have TDD baked into their system prompt with mandatory steps: Write the Test First, Verify Test FAILS (Red), Implement Minimal Code (Green), Verify All Tests Pass, Atomic Commit.

Agents that skip the red phase risk producing tests that pass vacuously (testing nothing meaningful).

#### 3. Skills-Level Enforcement

The `/tdd` skill provides the TDD protocol definition. The `/verification` skill requires evidence before any completion claims. Doing agents that skip this protocol will be flagged by the sigma-review agents during the review phase.

### Test Naming Convention

Agents use this naming pattern for tests:

```
test('AC<N>-HP: [behavior description]', async () => {
  // Given [precondition]
  // When [action]
  // Then [expected result]
});
```

Where `AC<N>` = acceptance criterion number, `HP` = happy path (or `EP` for error path, `EC` for edge case).

---

## Completion (external truth)

The target pipeline has no completion gates. The gate machinery (gate booleans, the `fno gate` surface, `gate_reality_map.yaml`, phase verifiers) was deleted in the control-plane collapse. A session is done when external reality agrees, decided by the read-only `fno-agents loop-check` verb:

| Read | What it checks |
|------|----------------|
| PR exists | A PR is open for the HEAD commit (`gh pr view`) |
| CI green | CI passes on that PR (`gh pr checks`) |
| Required-bot review | Every bot in `config.review.required_bots` has a completed review pass with no unaddressed blocking inline finding |
| Budget | A cost / wall-clock ceiling is not exceeded |

PR / CI / review reads are skipped when the manifest declares the corresponding skip flag (`no_ship`, `no_external`) or `ci.declared_none`. See [architecture/control-plane-loop.md](architecture/control-plane-loop.md) for the full decision algorithm and `TerminationReason` enum.

### How blocking works

When the agent emits `<promise>` but the world is not yet done, `loop-check` blocks session exit and names the failing read; the loop continues:

1. Target attempts to complete and emits `<promise>`
2. `loop-check` runs the reads against the world
3. A failing read returns a block decision naming the gap (e.g. "PR #N: chatgpt-codex-connector has not reviewed")
4. The block reason becomes the next turn's input
5. Target takes corrective action
6. The cycle repeats until the reads agree or a budget / NoProgress backstop terminates the session

### Deviation Rules During Execution

When agents encounter issues not covered by the plan:

| Situation | Action |
|-----------|--------|
| Bug in the plan | Fix inline, note in SUMMARY.md |
| Minor enhancement (< 15 min) | Implement, note it in SUMMARY.md |
| Architecture decision needed | STOP, return BLOCKED |
| Missing dependency | STOP, return BLOCKED |

---

## Review Agent Testing

The `sigma-review` skill orchestrates 6 parallel review agents. Each agent focuses on a specific quality dimension.

### Review Agents

| Agent | Focus | Trigger |
|-------|-------|---------|
| `code-reviewer` | CLAUDE.md compliance, bugs, code quality | Always runs (confidence threshold >= 80) |
| `silent-failure-hunter` | Swallowed errors, empty catch blocks, missing error handling | Always runs |
| `type-design-analyzer` | Type invariants, encapsulation, type safety | Runs when type definitions change |
| `integration-test-analyzer` | Journey test coverage gaps, missing integration tests | Runs when API or data flow changes |
| `ux-flow-tester` | Simulates human QA testing of user flows | Runs when UI components change |
| `multi-device-checker` | Responsive design across viewports | Runs when layout or styling changes |

### Conditional Execution

Agents run based on the type of changes detected:

- **UI changes** (components, styles, layouts) - triggers `ux-flow-tester` and `multi-device-checker`
- **Type changes** (interfaces, type definitions) - triggers `type-design-analyzer`
- **API changes** (endpoints, data flow) - triggers `integration-test-analyzer`
- **All changes** - always triggers `code-reviewer` and `silent-failure-hunter`

### Review Output Format

Each agent returns findings with severity levels:

- **BLOCK** - Must fix before merge
- **CONCERN** - Should fix, may be acceptable with justification
- **NOTE** - Informational, no action required

---

## Verification Levels

The verifier agent performs a 3-level check on every completed task. Each level builds on the previous one.

### Level 1: EXISTS

Confirms that the expected files, functions, components, or configurations exist in the codebase.

- Does the file exist at the expected path?
- Does the function/class/component exist with the expected name?
- Are configuration entries present?

### Level 2: SUBSTANTIVE

Confirms that implementations are non-trivial - not stubs, placeholders, or TODO markers.

- Does the function contain real logic (not just `pass` or `return null`)?
- Are components rendering actual content (not empty divs)?
- Do tests contain meaningful assertions (not just `expect(true).toBe(true)`)?

### Level 3: WIRED

Confirms that code is actually connected and functional in the system.

- Is the component imported and rendered in a route or parent?
- Is the API endpoint registered in the router?
- Is the database migration applied and the schema accessible?
- Do integration points actually connect (not orphaned code)?

### Common Verification Failures

| Failure Type | Description | Caught At |
|--------------|-------------|-----------|
| Orphaned code | File exists but nothing imports or uses it | WIRED |
| Phantom completion | Agent claims done but implementation is incomplete | SUBSTANTIVE |
| Scope drift | Built something different than what was requested | Goal verification |
| Stub masquerading | Placeholder passes basic checks but does nothing real | SUBSTANTIVE |

---

## Iteration Protocols

Three iteration loops handle different types of autonomous work. Each follows the common protocol: do ONE thing, verify mechanically, keep or discard, repeat.

### Fix Loop

The `/fix` skill runs a bounded iteration loop for bug fixing.

**Configuration:**

- Maximum iterations: 15
- One fix per iteration
- Auto-revert on regression

**How it works:**

1. Identify the failing test or error
2. Apply a single, targeted fix
3. Run the test suite
4. If the fix causes a regression (breaks something else), auto-revert
5. If the fix passes, commit and move to the next issue
6. Track results in `fix-results.tsv`

**Exit conditions:**

- All identified issues fixed
- Maximum iterations reached
- Fix causes repeated regressions (stuck)

### Debug Loop

The `/fix investigate` skill uses the scientific method for bug investigation.

**Configuration:**

- Maximum hypotheses: 5 per investigation
- Tournament escalation after 3 failures
- Attempt deduplication via `track-attempt` skill

**How it works:**

1. **Observe** - Gather symptoms, error messages, reproduction steps
2. **Hypothesize** - Form a testable hypothesis about the root cause
3. **Test** - Design and run an experiment to confirm or reject
4. **Conclude** - Accept or reject the hypothesis based on evidence

**Escalation:**

After 3 failed hypotheses, the debug loop switches to tournament mode - testing multiple hypotheses in parallel to increase coverage. The `track-attempt` skill prevents re-testing hypotheses that have already been rejected.

BDD acceptance criteria are generated for each bug to define what "fixed" looks like before any code changes begin.

### Target Loop

The target pipeline runs as a persistent loop until the feature is complete.

**In-session loop:**

- `hooks/target-stop-hook.sh` is a thin read-only shim over `fno-agents loop-check` (the bash detector machinery was deleted in the control-plane collapse)
- On a `<promise>`, `loop-check` reads the world (PR + CI + required-bot review); it allows exit only when those reads agree, otherwise it blocks and names the failing read
- The session manifest is inputs-only and immutable - there is no `status: IN_PROGRESS` boolean for the hook to read

**Cross-session loop:**

- One Rust runtime drives the loop: `fno-agents loop run --driver target|megawalk|megatron` (the old `fno loop` verb is removed)
- The walk stops on a `TerminationReason` event (DonePRGreen, DoneAdvisory, NoWork, Budget, NoProgress, Interrupted, Aborted) or the iteration ceiling

**External review polling:**

- The `/pr check` skill no longer uses a bash polling loop
- Two one-shot `CronCreate` checks fire at +5 and +10 minutes after invocation
- If the review is already present at invocation, it is processed immediately without scheduling crons
- The required-bot review read is part of `loop-check`'s `done()` check; a session that never satisfies it resolves via the budget / NoProgress backstop

**Completion signal:**

```
<promise>MISSION COMPLETE: all tasks done, tests passing, review feedback addressed, PR created</promise>
```

---

## Test Patterns

### State File Testing

The target pipeline manages three state files. Test scripts validate their lifecycle:

| File | Purpose | Owner |
|------|---------|-------|
| `.fno/target-state.md` | Pipeline iteration tracking, gate status | target |
| `.fno/STATE.md` | Wave and task progress within a plan | /do, /do waves |
| `.fno/SUMMARY.md` | Task completion notes and concerns | archer agents |

**What tests verify:**

- Files are created at pipeline start with correct initial structure
- Files are updated at each gate transition
- Status transitions follow valid paths (PENDING -> IN_PROGRESS -> DONE)
- Files are cleaned up or archived on completion
- Recovery works when files are corrupted or missing (see `test-target-state-recovery.sh`)

**Key scenarios in `test-target-state-recovery.sh`:**

- State file missing closing `---` delimiter
- State file with unknown extra fields
- State file from a different provider (provider migration)
- Empty state file (zero-byte)
- State file locked by another process

### Hook Testing

Hooks are shell scripts triggered by CLI lifecycle events. Testing covers:

**Stop hook (`target-stop-hook.sh`):**

- Prevents premature session exit when work is in progress
- Allows exit when promise tag is present in output
- Handles edge cases (missing state file, corrupted state)
- Detects thrashing (repeated identical outputs)
- Tested by `test_stop_hook_events.sh`

**`init-target-state.sh` age guard:**

Before resetting a `COMPLETE` or `BLOCKED` state file, the helper checks the `created_at` timestamp. Files newer than 300 seconds are preserved - this prevents an infinite loop where a freshly-completed session would be reset to `IN_PROGRESS`, causing the stop hook to block exit again. Files older than 300 seconds are treated as stale (from a previous session) and reset.

**Session-start hook (`session-start.sh`):**

- Restores in-progress state from previous sessions
- Injects project vision and workspace context

**Context monitoring (`context-monitor.js`):**

- Tracks context window usage
- Triggers pre-compact preservation of critical state

**Testing hook scripts manually:**

```bash
# Pipe JSON to stdin to simulate the hook trigger
mkdir -p "$TMPDIR/.fno"
cat > "$TMPDIR/.fno/target-state.md" << 'EOF'
---
status: IN_PROGRESS
iteration: 3
gates:
  all_tests_passing: false
---
EOF

echo '{"transcript_path": "/tmp/transcript"}' | \
    CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" \
    bash "$PLUGIN_ROOT/hooks/target-stop-hook.sh"
```

### Cross-CLI Testing

The plugin supports three CLI platforms. Provider capability tests verify behavior across each.

**Claude Code** (native):

- Full hook support via `hooks.json`
- Native agent dispatch with Agent tool
- Full subagent orchestration

**Gemini CLI:**

- Lifecycle hooks via `hooks-gemini.json`
- Agent synchronization tested by `test-sync-gemini-agents.sh`

**Codex CLI:**

- Skill linking and adapter layer
- Agent synchronization tested by `test-sync-codex-agents.sh`
- Hook configuration via `hooks-codex.json`

**What cross-CLI tests verify:**

- Skills are portable and produce consistent results
- Hooks fire at the correct lifecycle events per platform
- Agent dispatch routes correctly on each platform
- State files are read/written consistently regardless of CLI

**Simulating different providers:**

```bash
# Simulate Claude Code
CLAUDE_PLUGIN_ROOT="/path/to/plugin" bash script.sh

# Simulate Gemini CLI
GEMINI_PROJECT_DIR="/path/to/project" bash script.sh

# Simulate Codex CLI
CODEX_PLUGIN_ROOT="/path/to/plugin" bash script.sh
```

---

## Writing New Tests

### Test Script Template

New test scripts should follow this pattern:

```bash
#!/usr/bin/env bash
# Test: <what this tests>
# Usage: bash scripts/test-<name>.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

PASS=0
FAIL=0
TOTAL=0

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    ((TOTAL++))
    if [[ "$expected" == "$actual" ]]; then
        echo "  PASS: $desc"
        ((PASS++))
    else
        echo "  FAIL: $desc (expected '$expected', got '$actual')"
        ((FAIL++))
    fi
}

assert_contains() {
    local desc="$1" haystack="$2" needle="$3"
    ((TOTAL++))
    if echo "$haystack" | grep -qF "$needle"; then
        echo "  PASS: $desc"
        ((PASS++))
    else
        echo "  FAIL: $desc (expected to contain '$needle')"
        ((FAIL++))
    fi
}

# --- Test Cases ---

echo "=== Test Suite: <name> ==="
echo ""

echo "--- Test 1: <description> ---"
# Setup
# ...

# Execute
# result=$(...)

# Assert
# assert_eq "description" "expected" "$result"

echo ""

# --- Summary ---
echo "=== Results: $PASS/$TOTAL passed, $FAIL failed ==="
if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
```

### Test Naming Convention

- File name: `test-<feature-being-tested>.sh`
- Location: `scripts/`

### Test Isolation

Tests should:

- Create temporary directories for state files (`mktemp -d`)
- Clean up with a trap (`trap 'rm -rf "$TMPDIR"' EXIT`)
- Not modify the actual `.fno/` directory
- Not require network access
- Not require specific CLI providers to be installed

### Testing Agents

Agent behavior is validated through the review pipeline rather than unit tests. To test a new or modified agent:

1. Run the agent against a known task
2. Verify the return contract is correct:
   - `RESULT: SUCCESS|DONE_WITH_CONCERNS|FAILED|BLOCKED`
   - `TASK: task-id`
   - `COMMIT: hash` (if SUCCESS or DONE_WITH_CONCERNS)
   - `CONCERNS: description` (if DONE_WITH_CONCERNS)
   - `ERROR: message` (if FAILED)
   - `REASON: why` (if BLOCKED)
   - `UNBLOCKS_AFTER: what needs to happen` (if BLOCKED)
3. Check that the agent respects TDD discipline
4. Verify state files are updated correctly

### Running All Tests in Batch

```bash
#!/usr/bin/env bash
# Run all footnote tests
set -euo pipefail

FAILURES=0
for test_file in scripts/test*.sh; do
    echo "=== Running: $test_file ==="
    if bash "$test_file"; then
        echo "PASSED"
    else
        echo "FAILED"
        ((FAILURES++))
    fi
    echo ""
done

echo "=== Total failures: $FAILURES ==="
exit $FAILURES
```

---

## Troubleshooting

### Common Test Failures

**Stop hook test fails with "jq not found":**
- Install jq: `brew install jq` (macOS) or `apt install jq` (Linux)
- The stop hook exits silently without jq (enhancement hook pattern)

**Thrashing detection test gives false positives:**
- Ensure test transcript files have different content between iterations
- Check similarity threshold in the detection logic

**Plan validation test fails on valid plans:**
- Check phase file naming: must match `NN-*.md` pattern (two-digit prefix)
- Check 00-INDEX.md has valid YAML frontmatter between `---` delimiters
- Check wave task references match actual phase file numbers

**State recovery test fails:**
- Ensure temp directory is writable
- Check that no other process holds a flock on the test state file

**validate-test-first.sh blocks build commands:**
- The hook detected a build/start command without a recent `.fno/.last-test-run` marker
- Run your test suite first, then retry the build command

### Debug Mode

Most test scripts respect `set -x` for verbose output:

```bash
bash -x scripts/test-thrashing-detection.sh
```

For hook scripts, check the log file:

```bash
cat .fno/target-stop-hook.log
```
