# footnote documentation

The repo root [`README.md`](../README.md) is the quickstart. This index maps the rest of the docs. For the deepest technical detail, each skill's `SKILL.md` under `skills/` is the source of truth.

## Start here

- [getting-started.md](getting-started.md) - install to first run
- [troubleshooting.md](troubleshooting.md) - common failures and how to fix them
- [best-practices.md](best-practices.md) - how to get reliable, cost-bounded runs
- [auth.md](auth.md) - credential setup and multi-account notes
- [security-posture.md](security-posture.md) - the trust boundary for plan execution (see also [../SECURITY.md](../SECURITY.md))

## Guides (task-oriented)

- [guides/target.md](guides/target.md) - the autonomous loop: flags, gates, cross-project, resume
- [guides/think-and-plan.md](guides/think-and-plan.md) - design exploration, plan creation, wave execution
- [guides/pr-lifecycle.md](guides/pr-lifecycle.md) - review, create, check, merged: the PR arc by hand
- [guides/agents-quickstart.md](guides/agents-quickstart.md) - spawn and message peer agents (claude/codex/gemini)
- [guides/execution-modes.md](guides/execution-modes.md) - when to use target vs do vs operator
- [guides/per-task-executors.md](guides/per-task-executors.md) - how operator resolves an executor per task
- [guides/megawalk-walker.md](guides/megawalk-walker.md) - the continuous-delivery loop
- [guides/cross-project-inbox.md](guides/cross-project-inbox.md) - messaging between projects
- [guides/utilities.md](guides/utilities.md) - debug, code review
- [guides/reading-shipped-plans.md](guides/reading-shipped-plans.md) - the completion-stamp format

## Configuration

- [path-config.md](path-config.md) - the `config.paths.*` schema, env vars, template variables
- [configuration-guide.md](configuration-guide.md) - the full `settings.yaml` reference
- [provider-rotation.md](provider-rotation.md) - provider records, failover, per-agent routing, combos

## Architecture (how it works)

The [architecture/](architecture/) directory holds the design docs (one per subsystem). Entry points:

- [system-architecture.md](system-architecture.md) - the big picture
- [architecture/control-plane-loop.md](architecture/control-plane-loop.md) - the current completion model: external truth (PR + CI + required-bot review) plus a budget cap, decided by `fno-agents loop-check`- [architecture/megawalk-pipeline.md](architecture/megawalk-pipeline.md), [architecture/megatron.md](architecture/megatron.md) - the multi-feature and fleet loops
- [architecture/coordination.md](architecture/coordination.md) - the `fno claim` work-claim primitive
- [architecture/memory-system.md](architecture/memory-system.md) - the memory pass
- [architecture/multi-cli-hooks.md](architecture/multi-cli-hooks.md) - how footnote wires into each CLI's hooks
- [architecture/cost-accuracy.md](architecture/cost-accuracy.md) - session cost math: transcript dedup, version-aware pricing, the backfill runbook, `fno doctor --cost-check`

## Reference

- [api-reference.md](api-reference.md) - command surface reference
- [code-standards.md](code-standards.md) - coding conventions in this repo
- [testing-guide.md](testing-guide.md) - test layout, markers, and how to run each tier
- [HARNESSES.md](HARNESSES.md) - per-CLI substrate facts (hook events, frontmatter, skill dirs)
- [SKILL-COMPAT-MATRIX.md](SKILL-COMPAT-MATRIX.md) - per-skill cross-CLI compatibility
- [distribution.md](distribution.md) - packaging and release
- [deployment-guide.md](deployment-guide.md) - deployment notes

## Providers and alternate runtimes

- [providers/](providers/) - per-provider adapter notes (`gemini.md`, `codex.md`, `provider-adapters.md`)
- [SETUP-HERMES.md](SETUP-HERMES.md), [SETUP-OPENCLAW.md](SETUP-OPENCLAW.md) - alternate CLI runtimes

## Contributing

See [../CONTRIBUTING.md](../CONTRIBUTING.md) for dev setup, tests, and the CI gates.
