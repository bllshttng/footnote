# Preflight Check Catalog

This is the canonical list of target-preflight checks (v1). Each check is an independent shell script in `skills/target/scripts/preflight/checks/`.

## Check Contracts

All checks follow the same contract:

- **Stdout:** exactly one line: `{check-name} {pass|fail|warn|unknown} {message}`
- **Exit:** always 0 (failure encoded in stdout)
- **Runtime:** under 2 seconds
- **Read-only:** no file writes

Status semantics:

| Status | Meaning | Effect on preflight suite |
|--------|---------|--------------------------|
| `pass` | Condition met | No action |
| `fail` | Condition not met, must fix | Suite exits 1, blocks target |
| `warn` | Potential issue, may not matter | Suite exits 0, user sees warning |
| `unknown` | Cannot determine | Suite exits 0, user sees ? |

## Canonical Checks

### working-tree-clean

**Purpose:** Detect uncommitted changes before target starts making more changes.

**Command:** `git status --porcelain`

**Pass:** Empty output (no uncommitted changes).

**Fail:** Any untracked, modified, staged, or deleted files present. Message includes file count and up to 5 filenames.

**Config:** `.fno/preflight-ignore.txt` - one pattern per line. Files matching any pattern are excluded from the dirty check. `.fno/` directory itself is always excluded (it's preflight's own config).

**Why it matters:** target creates commits during execution. Starting on a dirty tree mixes target's changes with pre-existing untracked work, making it hard to review what target actually did.

---

### branch-state

**Purpose:** Prevent target from executing directly on protected branches.

**Fail conditions:**
- Detached HEAD state
- Branch is exactly: `main`, `master`, `prod`, `production`, `release`, `stable`

**Warn conditions:**
- Branch starts with: `release/`, `hotfix/`, `v[0-9]`

**Pass:** Any other branch (feature/*, fix/*, personal branches, etc.)

**Why it matters:** target creates commits and PRs. Running on `main` risks a direct push to the protected branch before the push gate fires.

---

### deps-installed

**Purpose:** Check that project dependencies are up to date before execution.

**Node.js:** Checks for `pnpm-lock.yaml`, `package-lock.json`, or `yarn.lock`. Warns if:
- The package manager is not in PATH
- `node_modules/` is missing
- The lockfile is newer than `node_modules/` (deps may be out of date)

**Python:** Checks for `pyproject.toml`, `requirements.txt`, or `uv.lock`. Warns if:
- `uv` is not in PATH (for `uv.lock` projects)
- `.venv/` is missing

**Unknown:** No recognized lockfile found.

**Note:** Missing tooling is `warn`, not `fail`. Tool availability is informational. The actual blocker is missing `node_modules/` or `.venv/`.

**Why it matters:** Missing deps causes import errors and build failures that target must then spend time diagnosing. Installing them first is 30 seconds vs. 5 minutes of confused debugging.

---

### test-suite-green

**Purpose:** Verify the test suite passes at HEAD before target makes changes.

**Default:** `unknown` (opt-in only). Skipped by default because it can take 60+ seconds.

**Opt-in:**
```bash
PREFLIGHT_RUN_TESTS=1 bash run-checks.sh
# or in .fno/settings.yaml:
# preflight:
#   test_suite_check: true
```

**Pass:** Test suite exits 0 within 60 seconds.

**Fail:** Test suite exits non-zero.

**Warn:** Test suite times out after 60 seconds.

**Detection logic:**
- Node: runs `pnpm test --bail` or `npm test -- --bail`
- Python: runs `pytest -x --timeout=55`

**Why it matters:** If tests are already broken at HEAD, target's changes will appear to have introduced the failures. Knowing the baseline avoids false blame and wasted debugging.

---

### codemap-fresh

**Purpose:** Check if the structural codebase map is current.

**Pass:** `.fno/codemap.md` exists and was modified within the last 24 hours.

**Warn:** File missing, or older than 24 hours. Includes age in the message.

**Why it matters:** target uses codemap for structural context. A stale codemap can cause target to misunderstand the codebase layout and generate code that doesn't fit the project structure.

---

### auth-valid

**Purpose:** Verify GitHub CLI authentication before target tries to create a PR.

**Skip conditions (unknown):**
- `gh` not installed
- No GitHub remote (`github.com`) AND no `.github/` directory

**Pass:** `gh auth status` exits 0.

**Fail:** `gh auth status` exits non-zero. Message includes the fix command: `gh auth login`.

**Why it matters:** target's ship phase requires `gh pr create`. Auth failure at the end of a 20-minute run is a particularly painful blocker.

---

### disk-space

**Purpose:** Warn before running out of disk space during execution.

**Thresholds:**
| Available | Status | Message |
|-----------|--------|---------|
| >= 5 GB | pass | normal |
| 1-5 GB | warn | getting low |
| < 1 GB | fail | critically low |

**Measurement:** `df -k $HOME` (macOS) or `df -B1 $HOME` (Linux).

**Why it matters:** LLM completions can generate large files. Docker pulls, npm installs, and build artifacts also consume disk. Running out mid-execution causes hard-to-diagnose partial failures.

## Adding New Checks

1. Create `scripts/preflight/checks/{check-name}.sh`
2. Follow the contract (one stdout line, always exit 0, under 2s)
3. Add entry to this catalog
4. `chmod +x scripts/preflight/checks/{check-name}.sh`
5. No registration needed - runner auto-discovers

## Ordering

Checks run in filesystem order (alphabetical). Each check runs independently - a failing check does not prevent subsequent checks from running. The runner collects all results and reports them together.
