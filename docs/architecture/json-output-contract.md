# JSON output contract

One machine-output contract for every verb in this repo: `--json` and `-J` mean JSON.

## The rule

`--json` and `-J` are the one request for machine output. Every verb that can print JSON accepts both spellings. A verb whose only output is JSON accepts the flag and prints the same bytes. With the flag, stdout carries JSON only. One JSON value prints, or one JSON object per line for a streaming verb (`subscribe`). Diagnostics go to stderr. The exit code carries the verdict. The bytes do not depend on whether stdout is a terminal. Without the flag, stdout is for people. Even JSON-looking text there is never a parse surface.

The caller rule: pass `-J`, parse one JSON value from stdout, and never parse output you did not ask for with `-J`. Tokens after an `--argv` or `--` boundary are a spawned process's payload and never trip JSON rendering.

Python is out of scope by the standing no-new-Python ruling, and `scripts/ci/check_flag_registry.py` refuses any new `typer.Option`. When a verb in `cli/src/fno` ports to `crates/`, it adopts this contract.

## The four shapes found

1. Opt-in. Text by default, `--json`/`-J` switches to JSON: `crates/fno-agents/src/needs.rs:551`, and most Python verbs.
2. Always JSON. The flag is a no-op parity flag (`cli/src/fno/graph/cli.py:4493`). Or there is no flag at all, and `-J` is refused (`fno backlog next`, `fno backlog get`).
3. TTY-auto. When stdout is not a terminal, JSON prints. When it is a terminal, text prints (`cli/src/fno/agents/cli.py`, `cli/src/fno/mail/cli.py`). The same command prints different bytes in a pane and in a pipe. The bytes must not depend on the terminal. With `-J` they never do.
4. Format selector. `--format markdown|json` (`cli/src/fno/decide/cli.py:550`, `cli/src/fno/plan/cli.py`, `crates/fno-agents/src/court_fold.rs`). On verbs that print JSON, `-J` selects the JSON format the same way.

## Where it lives

`json_output::is_flag` and `json_output::requested` in `crates/fno-agents/src/json_output.rs` serve the hand parsers in `fno-agents`. `requested` stops at an `--argv` or `--` boundary. The clap JSON output group in each crate's `cli_args.rs` serves the ported verbs. The guard test `crates/fno-agents/tests/json_output_contract.rs` fails any parse line under `crates/` that names `--json` without also naming `-J` or `json_output::`. A new verb cannot reintroduce the split.

## Port queue

These Python verbs still refuse `-J`. The list was regenerated 2026-09-15 with the AST census from the plan that created this doc. The census counts 65 no-flag rows and 2 registered `--format` commands. The third `--format` site is `decide/cli.py:559 backlog_decisions`. It carries no command decorator, so the census skips it. When a verb ports to `crates/`, it adopts this contract.

no-flag: cli/src/fno/agents/cli.py cmd_crown
no-flag: cli/src/fno/agents/cli.py cmd_spawn
no-flag: cli/src/fno/agents/cli.py cmd_discovered_json
no-flag: cli/src/fno/agents/cli.py cmd_heal_token
no-flag: cli/src/fno/agents/distress_reads.py cmd_distress_verdicts
no-flag: cli/src/fno/agents/gate_reads.py cmd_gate_status
no-flag: cli/src/fno/backlog/capture.py cmd_add
no-flag: cli/src/fno/backlog/capture.py cmd_scan
no-flag: cli/src/fno/backlog/capture.py cmd_empty_pass
no-flag: cli/src/fno/backlog/capture.py cmd_capture_pass
no-flag: cli/src/fno/backlog/capture.py cmd_promote
no-flag: cli/src/fno/backlog/capture.py cmd_dismiss
no-flag: cli/src/fno/backlog/capture.py cmd_archive
no-flag: cli/src/fno/cli.py review
no-flag: cli/src/fno/events/cli.py emit
no-flag: cli/src/fno/events/cli.py gate_escape
no-flag: cli/src/fno/events/cli.py gc
no-flag: cli/src/fno/events/cli.py audit
no-flag: cli/src/fno/graph/cli.py cmd_decompose
no-flag: cli/src/fno/graph/cli.py cmd_next
no-flag: cli/src/fno/graph/cli.py cmd_lane_fill
no-flag: cli/src/fno/graph/cli.py cmd_dispatch_lanes
no-flag: cli/src/fno/graph/cli.py cmd_join
no-flag: cli/src/fno/graph/cli.py cmd_groom
no-flag: cli/src/fno/graph/cli.py cmd_status
no-flag: cli/src/fno/graph/cli.py cmd_queued
no-flag: cli/src/fno/graph/cli.py cmd_contain
no-flag: cli/src/fno/graph/cli.py cmd_migrate_priorities
no-flag: cli/src/fno/graph/cli.py cmd_migrate_difficulty
no-flag: cli/src/fno/graph/cli.py cmd_migrate_updated_at
no-flag: cli/src/fno/graph/triage.py cmd_context
no-flag: cli/src/fno/graph/triage.py cmd_propose
no-flag: cli/src/fno/graph/triage.py cmd_rank
no-flag: cli/src/fno/graph/triage.py cmd_validate
no-flag: cli/src/fno/graph/triage.py cmd_apply
no-flag: cli/src/fno/graph/triage.py cmd_projects
no-flag: cli/src/fno/inbox/operator_turns.py cmd_ack
no-flag: cli/src/fno/king/cli.py drain_cmd
no-flag: cli/src/fno/mail/cli.py cmd_hold_release
no-flag: cli/src/fno/pr/cli.py coverage_publish
no-flag: cli/src/fno/pr/cli.py info
no-flag: cli/src/fno/pr/cli.py list_cmd
no-flag: cli/src/fno/pr/cli.py review_hold
no-flag: cli/src/fno/pr/cli.py evidence_required
no-flag: cli/src/fno/pr/cli.py bind_created
no-flag: cli/src/fno/resume/cli.py write_cmd
no-flag: cli/src/fno/resume/cli.py validate_cmd
no-flag: cli/src/fno/resume/cli.py show_cmd
no-flag: cli/src/fno/review/cli.py classify
no-flag: cli/src/fno/review/cli.py resolve_level
no-flag: cli/src/fno/route_cli.py set_cmd
no-flag: cli/src/fno/route_cli.py unset_cmd
no-flag: cli/src/fno/setup_cli.py plan_cmd
no-flag: cli/src/fno/state/cli.py show
no-flag: cli/src/fno/state/cli.py set_field
no-flag: cli/src/fno/state/cli.py init
no-flag: cli/src/fno/state/cli.py archive
no-flag: cli/src/fno/stub_manifest.py cmd_reconcile_validate
no-flag: cli/src/fno/stub_manifest.py cmd_reconcile_finalize
no-flag: cli/src/fno/target_cli.py blast_check
no-flag: cli/src/fno/target_cli.py request_self_review_cmd
no-flag: cli/src/fno/worker/cli.py blueprint
no-flag: cli/src/fno/worker/cli.py ship
no-flag: cli/src/fno/worker/cli.py external
no-flag: cli/src/fno/worker/cli.py reconcile
format: cli/src/fno/decide/cli.py list_cmd
format: cli/src/fno/plan/cli.py brief
format: cli/src/fno/decide/cli.py backlog_decisions (no command decorator, census skips it)
