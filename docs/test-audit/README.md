# Test-audit campaigns

Running home for the global test audit: one campaign PR per subsystem, one ledger per campaign in this folder. Method, value bar, and junk patterns live in the test-audit skill (`skills/test-audit/`). Campaign order of work is that skill's `CAMPAIGN.md`. Optimize for confidence, not deletion count.

## Census

Measured on main (2026-09-25). Command for Python (the naive `^def test_` count misses 2,078 class-method tests):

```sh
git ls-files -z '*.py' | xargs -0 grep -hcE '^[[:space:]]*(async )?def test_' | awk '{s+=$1} END {print s}'
git ls-files -z 'crates/*.rs' | xargs -0 grep -hcE '#\[(tokio::)?test\]' | awk '{s+=$1} END {print s}'
```

20,337 Python declarations in 1,130 files. 10,137 Rust in 672 files. About 287 test-shaped shell files. About 30,500 declarations before parametrize expansion.

The src-read column counts tests in files that read repo source or docs (`SKILL.md`, `AGENTS.md`, `getsource`, `.md` read_text). It is a suspicion signal for the exact-source-grep junk pattern, not a verdict.

| Python owner (tests, files) | src-read | Rust owner (tests) |
|---|---|---|
| fno.agents 4,908 / 235: registry 977, harnesses 631, dispatch 427, cli 354, mux_spawn 325, spawn_defaults 312, harness_map 250, spawn_gate 203, session_truth 169, account_env 160 | 33% | fno-agents root files 4,747 (king 322, spawn 209, client 207, claude 179, daemon 137, gc 135, claims 125, codex 107) |
| fno.graph 2,991 / 150: store 1,192, cli 345, _reconcile 303, _intake 264, ladder 129, collision 97 | 16% | fno-agents/tests 1,125; loopcheck 440; daemon 308; backlog 167 |
| fno.claims 1,101 / 40 (claim-owner files alone: about 550) | 19% | claims family 189 |
| fno.adapters 1,027 / 26 | 0% | - |
| no fno import (CI-gate, hook, skill tests) 882 / 106 | 64% | - |
| fno.pr 821, pr_watch 505, review 143, review_capability 108 | 6-26% | pr/merge/review 484 |
| fno.config 716, paths 545, config_cli 88 | 26-36% | config/paths/route/provider 337 |
| fno.plan 541, retro 225, scoreboard 186, provenance 189 | 12-41% | plan/acceptance/manifest 157 |
| fno.mail 409, events 400, bus 140, relay 101 | 27-31% | mail 87, event 72 |
| repo tests/ Python 396 | 72% | - |
| long tail: about 60 owners, 3,700 tests | varies | fno crate (mux) 2,120: client 506 + client_tests.rs, server 282, squad 97; loopcheck/finalize family 1,007; daemon/gc 635; king/crown/reign 700 |

## Campaign queue

Rank rule: suites that test both legs of a dual implementation first, then src-read density times size, then Rust-only suites. Each campaign re-baselines on the main it starts from, so counts drift. A campaign measuring over about 1,400 declarations at baseline splits along a production owner boundary before its ledger starts. After each merge the merging session files the next row as a child node and blueprints it from its row here.

| # | Campaign | In scope (approx) | Python leg | Rust leg | Why this rank |
|---|---|---|---|---|---|
| 3 | claims | 550 Py + 189 Rust | cli/src/fno/claims/io.py:158; claims/cli.py | claims_root.rs:82; claim_verbs.rs:130 | dual, parity-guarded, smallest dual owner |
| 4 | spawn and dispatch | 1,267 Py + 276 Rust | agents/spawn_gate.py, dispatch, mux_spawn | spawn_gate.rs | dual; test_spawn_pane.py is 5,324 lines (shrink-only) |
| 5 | session registry | 1,230 Py | agents/registry.py | registry_json.rs | dual; 977 tests on one owner |
| 6 | graph store and board view | 1,192 Py + graph_store Rust | graph/render.py | backlog_view.rs | dual; unguarded mirror per the dual-implementation inventory |
| 7 | mail, bus, events, relay | 1,050 Py + 159 Rust | events/log.py | claude_ask.rs; claims.rs | dual emitters |
| 8 | config and paths | 1,349 Py + 337 Rust | paths.py | paths.rs | partial dual, 36% src-read |
| 9 | CI gates, hooks, skill tests | 882 Py + 396 repo Py + shell | none (tooling) | none | highest src-read density |
| 10 | graph verbs | about 1,400 Py + 167 Rust | graph/cli.py | fno-agents/src/backlog | junk density 16% but big |
| 11 | pr and review | 1,072 Py + 484 Rust | fno.pr, fno.review | pr_*, merge_*, review | dual in part |
| 12 | plan, retro, scoreboard, provenance | about 1,140 Py + 157 Rust | fno.plan | acceptance, manifest | 41% src-read in plan |
| 13 | harness adapters | 1,041 Py (harnesses, harness_map, account_env) | fno.agents.harnesses | claude*, codex* | lower src-read |
| 14 | king, crown, reign | about 400 Py + 700 Rust | fno.king | king*, crown*, reign* | Rust-heavy |
| 15 | mux (fno crate) | 2,120 Rust, split client/server if over the bound | absent | crates/fno/src | Rust-only; client_tests.rs 16,930 lines shrink-only |
| 16 | loopcheck, finalize, daemon, gc | 1,642 Rust | absent in production | loopcheck/, daemon/ | coordinate with the CI-sharding plan on loop_check.rs |
| 17-18 | long tail (adapters 1,027, cli 479, setup, inbox, ...) | about 3,700 Py, split in two by owner | varies | varies | lowest measured density |

(The stress e2e audit predates this queue and is row two below. The skill's own trial campaign is row one.)

## Running total

Declarations are `def test_` / `#[test]` counts. Campaign rows link their ledger in this folder. CI minutes come from the smoke-duration-report lines of the campaign PR run against the last main run.

| Campaign | Declarations before | Declarations after | CI minutes before | CI minutes after | Ledger |
|---|---|---|---|---|---|
| 1: spec-trial campaign | 17 pytest cases + 6 doc greps | 16 + 6 | - | - | the trial PR body |
| 2: stress e2e | 53 | 48 | 20.6 (20-trial job) | 15.9 | stress-e2e-campaign.md |
| 3: claims | 446 Py + 169 Rust | 431 Py + 168 Rust | shards skipped: the PR touches docs and tests only | same (10 collected cases and 1 Rust test less to run) | claims-campaign.md |
