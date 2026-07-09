# Contributing to footnote

Welcome. This guide covers the dev environment, how to run the tests and gates locally, and the conventions a PR is expected to follow.

## Development setup

Requires Python 3.11+ and [uv](https://docs.astral.sh/uv/). Most contributors also have `jq`, `gh` (authenticated), and `git`.

```bash
git clone https://github.com/bllshttng/fno.git
cd footnote
uv tool install ./cli      # puts `fno` on your PATH
fno --version
```

For iterating on the CLI itself, work inside `cli/` and run your local changes with `uv run fno` (no reinstall needed; `uv tool install` above pins a static copy):

```bash
cd cli
uv sync                    # create the dev venv with test/lint deps
uv run fno --version       # runs the local development version
```

## Running the tests

The Python suite runs from `cli/` via uv. This mirrors the per-PR CI command:

```bash
cd cli
uv run pytest --tb=short -q -m "not flaky_socket"
```

Pytest markers (declared in `cli/pyproject.toml`) tag the slower and special tiers: `slow_e2e` and `slow_e2e_25min` (nightly only), `e2e` (run explicitly with `-m e2e`), `smoke` (real-binary, gated by `GEMINI_SMOKE=1`), and `flaky_socket` (socket/timing-sensitive, quarantined). The per-PR command above excludes the quarantined tier via `-m "not flaky_socket"`.

The Rust agent runtime lives in `crates/fno-agents/`:

```bash
cd crates/fno-agents
cargo test --all-targets
```

### Soaking the socket/timing tests under load (on-demand)

The `claude_ask` socket tests (Rust `crates/fno-agents/src/claude_ask.rs` and
the Python `cli/tests/agents/test_provider_claude_followup.py` twin) are
synchronized by observed readiness signals, not fixed sleeps, so they should
pass deterministically even under CPU saturation. This is kept as an on-demand
local check rather than a per-PR CI lane (it would burn the minutes budget).
To soak it, saturate the cores and loop the clusters (expect zero failures):

```bash
# saturate all cores in one shell
for i in $(seq 1 "$(sysctl -n hw.ncpu 2>/dev/null || nproc)"); do yes >/dev/null & done
# then loop the clusters from the repo root in another shell (this crate has
# no top-level Cargo.toml, so the Rust loop pins --manifest-path)
for i in $(seq 1 50); do cargo test --manifest-path crates/fno-agents/Cargo.toml --lib claude_ask::tests || break; done
for i in $(seq 1 50); do (cd cli && uv run pytest tests/agents/test_provider_claude_followup.py::test_ask_followup_happy_path -q) || break; done
# stop the load when done: pkill yes
```

There is also a large set of bash integration tests under `tests/` (hooks, gates, events, dispatch) that CI runs individually; run the ones relevant to your change, e.g. `bash tests/hooks/test_target_bg_job_governance.sh`.

## Gates your PR must pass

CI runs these on every PR (`.github/workflows/`):

- **cli-ci** - `uv build`, the pytest suite, `fno paths verify`, and the bash hook/gate/event integration tests.
- **rust-ci** - `cargo test --all-targets`, `cargo build --bin fno-agents`, and `scripts/check-event-schema-parity.sh`.
- **provider-smoke** - provider-adapter smoke checks.

Run these locally before pushing:

- **Skill-bundle freshness.** Driver skills are self-contained; shared content is assembled at build time from `skill-bundles.yaml`. If you edit a file that is a bundle source (a shared script, a `references/` doc, or a subagent prompt), regenerate and verify:
  ```bash
  bash scripts/generate-skill-bundles.sh
  fno bundle check          # must report "skill bundles fresh"
  ```
- **Static checks** in `scripts/ci/`: `check-no-hardcoded-paths.sh`, `check-no-stale-skill-refs.sh`, `check-registry-schema-parity.sh`. Run the one that covers your area.

## Skill self-containment

Driver skills (`/target`, `/megawalk`) must stay portable to any markdown-aware runtime. CI enforces four invariants: no `${REPO_ROOT}/scripts/` references, no `${SKILL_DIR}/../../scripts/` path escapes, no `Skill()` runtime calls between driver skills, and no `../../_shared/` or sibling-skill path escapes. Reuse shared content through the bundler, not through relative paths. See [docs/architecture/skill-encapsulation.md](docs/architecture/skill-encapsulation.md).

## State files are CLI-owned

Never hand-edit `~/.fno/graph.json` or the immutable session manifest `.fno/target-state.md` (an inputs-only file; the only legal post-init write is first-fill of `plan_path` via `fno state set`). Use the `fno` verbs (e.g. `fno backlog`). Hooks detect direct graph mutations post-hoc. See `skills/using-fno/SKILL.md` for the full surface map.

## Conventions

- **Vocabulary.** Abilities uses **target** as the verb for autonomous delivery. We do not use "AFK", "autonomous coding agent", "background agent", or generic search terms. The distinctive verb is the brand; PRs that introduce generic terms will be asked to rename.
- **Surgical changes.** Touch only what the change requires. Match the surrounding style. Don't refactor adjacent code that isn't broken.
- **Tests first** for behavior changes: a failing test that captures the bug or feature, then the code that makes it pass.

## Security

Do not file security issues as public issues or PRs. See [SECURITY.md](SECURITY.md).
