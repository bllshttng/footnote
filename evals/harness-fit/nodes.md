# Replay sample

x-632f drew the first sample of 10 by the preregistered rule. x-272d ran a grade-only dry run on every task before Run 1 and replaced five of them under amendment 3 in `README.md`.

| task | node | merged | merge sha | first-parent | hidden tests |
|---|---|---|---|---|---|
| replay-x-1018 | x-1018 | 2026-09-09 | 8b0dd9cf6bca43449d91c716dfae44182a3ae0d1 | 6e693726027a849749c5b28a2b2553bb09054f9e | cli/tests/unit/test_workflow_authority_contract.py |
| replay-x-5978 | x-5978 | 2026-09-26 | c5cb8e1a397a7aff9aa1390b44bd3dea94570b7f | c4840341d151a6524deded3e9c8e54675e6c306a | cli/tests/agents/test_transcript_paths.py, crates/fno-agents/tests/provider_cap_transcript_bridge.rs |
| replay-x-5baf | x-5baf | 2026-09-04 | f02c3f760f2042dfca9bc6e1582e64d844e8065c | 0eba748b36cb57f4cda693764918aad2f8ff15a5 | crates/fno/tests/mux_restore_shell_cwd_e2e.rs |
| replay-x-70e1 | x-70e1 | 2026-09-08 | fa00bdc7dcf9d398805f86d55ab1e35bde6b0585 | bd8c575bbdc9c991e0c053c72dd6986a8dab5cbc | cli/tests/unit/test_spawn_phase.py, crates/fno-agents/tests/retirement_e2e.rs |
| replay-x-71e9 | x-71e9 | 2026-09-17 | cf072030af1b14eaef6ce850872afcdedb182b8e | 50925ebb3932341ca44c643d30f6b17df7ff270b | cli/tests/agents/test_dsh_acp.py |
| replay-x-861c | x-861c | 2026-09-15 | 504fe8872b10ec7b6cedba03444c48879a5d36cf | 2a80048f71c5042de1d11bd4333668303b6db5a6 | crates/fno-agents/tests/cli_registry.rs, crates/fno/tests/cli_registry.rs |
| replay-x-a23c | x-a23c | 2026-09-24 | f5af0c1580666c1e99d5e82eb06711e9589eb605 | 7893a86607ad1336852b983269e44d27a1284f4f | cli/tests/lint/test_agent_skill_loading.py |
| replay-x-a8f3 | x-a8f3 | 2026-09-04 | 469b2356a62e39f119caba5e6e78032a844e7106 | 3b12743738da7c98f9da631343b77e35837572ae | cli/tests/test_pr_read_budget.py, cli/tests/unit/test_harness_map_roster.py |
| replay-x-c911 | x-c911 | 2026-09-12 | 7ca0d256f35186b6ea25719c9f199bb24d5edd3e | 3c8b4b0da38a016735ca1c7f55fe6a89748e49f2 | cli/tests/unit/test_shim_check.py |
| replay-x-f188 | x-f188 | 2026-09-11 | 9f01454cea12e7f9c151a145196bc8ba0afd3c88 | 78084fcddb051926f46cd578939008184d78c2b8 | crates/fno-agents/tests/census.rs |

## Replaced before Run 1

| removed | why | replaced by | candidates skipped on the way |
|---|---|---|---|
| replay-x-43bd | its one hidden test is skipped at both shas, so it grades nothing | replay-x-a8f3 | the merges between them, x-7b1c onward, name no node in the graph |
| replay-x-32de | its hidden test fails at its own merge commit | replay-x-5978 | x-d1dc (its hidden test is an in-crate module; the generated grade fails at the merge) |
| replay-x-0c0b | the node is not in the graph; the prompt was one invented sentence | replay-x-5baf | none |
| replay-x-1397 | the node is not in the graph; the prompt was one invented sentence | replay-x-1018 | none |
| replay-x-1f09 | the node is not in the graph; the prompt was one invented sentence | replay-x-70e1 | none |

Every Rust grade also changed: the repo has no Cargo workspace manifest, so each grade now runs `cargo test` inside its crate. The dry-run log is `/Users/bb16/evals-workspace/harness-fit/logs/dryrun*.jsonl` and `dryrun-*.out` in the run workspace.
