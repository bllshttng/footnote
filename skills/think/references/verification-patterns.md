# Verification Patterns

Mechanical verification catalog for iteration-based skills.

## Detection Order

Inspect the repo in this order and pick the first matching toolchain:

1. `package.json`
2. `pyproject.toml`, `requirements.txt`, or `pytest.ini`
3. `Cargo.toml`
4. `go.mod`
5. shell-only repo conventions such as `scripts/doctor.sh` or project-specific test scripts

Always prefer the fastest command that still catches the target failure.

## Standard Commands by Project Type

| Project | Test | Type / Static Check | Lint | Build / Verify |
|---------|------|---------------------|------|----------------|
| TypeScript / Node | `pnpm test`, `npm test`, or framework-specific test path | `pnpm tsc --noEmit` or `npx tsc --noEmit` | `pnpm lint` or `npm run lint` | `pnpm build` or `npm run build` |
| Python | `pytest` | `python -m py_compile <files>` or `mypy` if configured | `ruff check .` or `flake8` | project-specific smoke command |
| Go | `go test ./...` | `go test` doubles as compile validation | `golangci-lint run` if configured | `go build ./...` |
| Rust | `cargo test` | `cargo check` | `cargo clippy -- -D warnings` | `cargo build` |
| Shell / Docs / Plugin repos | focused shell test scripts | `bash -n <script>` for syntax | `shellcheck` if configured | repo-specific doctor and integration scripts |

## Fast vs Thorough Tiers

| Tier | Use When | Examples |
|------|----------|----------|
| Fast | inside an iteration loop | targeted pytest path, single shell test, `cargo check`, `python -m py_compile` |
| Standard | before keeping a final change set | broader test category, repo smoke script |
| Thorough | pre-ship validation | full doctor/build/test suite |

Use fast checks inside the loop and reserve thorough checks for the target validate phase.

## Guard Pattern

A guard is a command that must always pass even when the target metric improves.

Examples:

- fixing test failures with `tsc --noEmit` as the guard
- reducing type errors with `pytest` as the guard
- updating docs with `git diff --check` as the guard

If the guard fails, revert or rework the iteration.

## Abilities-Specific Patterns

For this repository, prefer:

| Goal | Command |
|------|---------|
| Plan validation | `bash scripts/validate-plan.sh <plan-dir>` |
| Stop-hook behavior | `bash scripts/test_stop_hook_events.sh` |
| Anti-pattern scanner | `bash scripts/test-scan-antipatterns.sh` |
| Shell health | `bash scripts/doctor.sh` |
| Python syntax | `python3 -m py_compile <files>` |
| Patch integrity | `git diff --check` |

## Auto-Detection Heuristic for Fix Loops

1. Check build or compile failures first.
2. Then check type/static-analysis failures.
3. Then run tests.
4. Then lint.
5. Track warnings last as lowest priority.

Reason: when build or syntax is broken, downstream signals are noisy.
