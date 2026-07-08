# Preflight check catalog

Reference for target's environment audit step. Run from `skills/target/SKILL.md` step 3g at the start of every run. Read-only, fast, fail-fast.

**3-second environment check before target spends 20 minutes hitting an avoidable blocker.**

Catches the most common environmental reasons target gets blocked: dirty working tree, expired auth, missing deps, stale codemap. Read-only. Fast.

## Invocation

```bash
# From a target phase (the canonical path)
bash "${SKILL_DIR}/scripts/preflight/run-checks.sh"

# Override per-invocation (skip-flag)
/target --skip-preflight "feature description"
```

## Output Format

Each check emits one line on stdout:

```
  ✓ working-tree-clean: clean working tree
  ✗ branch-state: on protected branch 'main' - create a feature branch before making changes
  ⚠ codemap-fresh: codemap is 26h old (>24h) - consider refreshing with `fno codemap`
  ? test-suite-green: opt-in not set (set PREFLIGHT_RUN_TESTS=1 or test_suite_check: true in config.toml)
```

Glyphs: `✓` pass, `✗` fail, `⚠` warn, `?` unknown

Final line (machine-readable JSON summary):

```json
{"passed":5,"failed":1,"warned":1,"unknown":1,"total":8,"failed_checks":["branch-state"]}
```

## Exit Codes

| Exit | Meaning |
|------|---------|
| 0 | All checks pass, warn, or unknown (no blockers) |
| 1 | One or more checks failed (blockers present) |

## Check Contract

Each check script follows this protocol:

- **Stdout:** exactly one line: `{check-name} {pass|fail|warn|unknown} {message}`
- **Exit:** always 0 (failure encoded in stdout, not exit code)
- **Runtime:** under 2 seconds
- **Read-only:** no writes (except to `.fno/artifacts/preflight-{session}.md`)

## Available Checks

See [preflight-checks.md](preflight-checks.md) for the full catalog with pass criteria, failure conditions, and config options.

| Check | Default | Notes |
|-------|---------|-------|
| `working-tree-clean` | always | Supports `.fno/preflight-ignore.txt` allowlist |
| `branch-state` | always | Fails on main/master/prod/release/production; warns on hotfix |
| `deps-installed` | always | Warns on missing tools (not fail) |
| `test-suite-green` | unknown | Opt-in: `PREFLIGHT_RUN_TESTS=1` or `test_suite_check: true` |
| `codemap-fresh` | always | Warns when `.fno/codemap.md` >24h old |
| `auth-valid` | always | Skips if no GitHub remote detected |
| `disk-space` | always | Fails <1GB, warns <5GB |

## Configuration

### Allowlist (working-tree-clean)

Create `.fno/preflight-ignore.txt` with one pattern per line to suppress specific untracked files from blocking preflight:

```
# .fno/preflight-ignore.txt
# Lines starting with # are ignored
scratch.md
tmp/
.env.local
```

### Opt-in tests

```bash
# Run test suite check for this invocation
PREFLIGHT_RUN_TESTS=1 bash "${SKILL_DIR}/scripts/preflight/run-checks.sh"

# Enable permanently for a project
# In .fno/config.toml:
# preflight:
#   test_suite_check: true
```

### Custom checks directory

```bash
# Override checks directory (mainly for testing)
PREFLIGHT_CHECKS_DIR=/path/to/checks bash "${SKILL_DIR}/scripts/preflight/run-checks.sh"
```

## Integration with target

Target invokes preflight automatically at step 3g (after state init, before any pipeline phase). If any check fails, target cannot proceed - the BLOCKED state is set by the stop hook on the next spawn, not by target directly (typed-blocker invariant).

To skip (use sparingly):

```bash
/target --skip-preflight "feature description"
```

The external loop (`scripts/run-target-loop.sh`) re-runs preflight at the start of every autonomous iteration, since these can span hours and environment conditions can change between restarts.

## Adding New Checks

1. Create `scripts/preflight/checks/{check-name}.sh` following the contract above
2. Add it to [preflight-checks.md](preflight-checks.md)
3. Make it executable: `chmod +x scripts/preflight/checks/{check-name}.sh`
4. Test: `bash scripts/preflight/run-checks.sh` should pick it up automatically

No registration needed - the runner discovers all `*.sh` files in the checks directory.
