# Test-audit campaigns

Running home for footnote's test-cut campaigns: one campaign PR per campaign key, one ledger per campaign in this folder. Method and junk patterns live in the test-audit skill (`skills/test-audit/`). Footnote campaigns run the keep rule below, which replaces the skill's value bar, retention bar and ledger shape. Target: at most 15,447 declarations, half the 30,893 measured at 325d7e4ddb (2026-09-28).

## Census

The count of record, re-run at each campaign base and after each merge. Command for Python (the naive `^def test_` count misses the class-method tests):

```sh
git ls-files -z '*.py' | xargs -0 grep -hcE '^[[:space:]]*(async )?def test_' | awk '{s+=$1} END {print s}'
git ls-files -z 'crates/*.rs' | xargs -0 grep -hcE '#\[(tokio::)?test' | awk '{s+=$1} END {print s}'
```

At 325d7e4ddb: 19,570 Python declarations in 1,115 files. 11,323 Rust in 772 files. 30,893 total, before parametrize expansion. The Rust command is the prefix form `'#\[(tokio::)?test'`: the `\]`-terminated form misses 92 `#[tokio::test(flavor ...)]` declarations.

## Keep rule

One contract, one keeper. The burden sits on the test. A deletion owes no per-test evidence.

- When its ledger row names the one contract a test alone guards, the test stays. It must also name a credible failure that turns it red. A test that cannot name both is deleted.
- When several tests guard one contract, keep the strongest boundary (the real verb, store or transport). Delete the rest.
- When the text is a user-facing byte (a refusal string, config key, or path), the grep stays. This rule covers greps of docs, skills, workflows and source files. A behavior test must not cover the same text.
- When the dual-implementation inventory (docs/architecture/dual-implementation-inventory.md) marks the port complete, delete the test that forces both legs to agree. Principle 9: delete a leg. Never keep a harness forcing two legs to agree.
- Per-flag, per-key and per-default tests fold into one table test per surface. One row covers each distinct branch, not each value.
- When a second campaign scope uses a test helper or CI selftest double, its test stays. Otherwise the helper test is deleted.
- Slow, static or flaky is still not a reason to keep.
- Parametrize conversion is not a cut. The scoped pytest collected-case count must fall by the same half as the declaration count.
- The skill's contract families (public API, config, storage and the rest) are kinds of contract a test can name. They are not a reason to keep a test another test already covers.
- Never delete a test that is the only guard for a fail-closed path.

## Campaigns

Ten campaigns, K1 to K10, run in key order, one PR each. Two or three can run at once. The map (`campaign-map.tsv`) is the scope contract: a campaign edits only files its key names, plus its own ledger. Each campaign re-baselines on the origin/main it starts from. The before-count can drift from the one here. The ceiling moves with the recount. A PR's bound is the 3,000 added-line cap, not a declaration split. Expected landing zone is 45 percent kept, about 13,900 declarations.

| Key | Scope | Py | Rust | Total | Files | Ceiling | Ledger |
|---|---|---:|---:|---:|---:|---:|---|
| K1 | tooling (CI gates, hooks, lint, skills, repo tests/, goldens, benchmarks) plus config, paths, setup, doctor, update, worktree | 2,675 | 333 | 3,008 | 185 | 1,504 | [k1-campaign.md](k1-campaign.md) |
| K2 | spawn and dispatch | 2,093 | 518 | 2,611 | 152 | 1,306 | [k2-campaign.md](k2-campaign.md) |
| K3 | session registry, watchdog, discover, recovery, reap, role | 2,534 | 535 | 3,069 | 163 | 1,535 | [k3-campaign.md](k3-campaign.md) |
| K4 | harness adapters and providers (cli/src/fno/adapters colocated tests, harnesses, claude/codex Rust) | 1,593 | 669 | 2,262 | 122 | 1,131 | [k4-campaign.md](k4-campaign.md) |
| K5 | graph store, board, backlog verbs, claims, carveouts | 3,547 | 999 | 4,546 | 273 | 2,273 | [k5-campaign.md](k5-campaign.md) |
| K6 | pr, review, merge, pr_watch, mail, bus, events, relay, inbox, decide | 3,208 | 1,041 | 4,249 | 236 | 2,125 | [k6-campaign.md](k6-campaign.md) |
| K7 | plan, retro, scoreboard, provenance, evals, target | 1,773 | 476 | 2,249 | 132 | 1,125 | [k7-campaign.md](k7-campaign.md) |
| K8 | mux crate (all of crates/fno) | 0 | 3,012 | 3,012 | 204 | 1,506 | [k8-campaign.md](k8-campaign.md) |
| K9 | loopcheck, finalize, daemon, gc, lead, role, lead, heal | 0 | 2,200 | 2,200 | 99 | 1,100 | [k9-campaign.md](k9-campaign.md) |
| K10 | long tail (everything the other keys did not claim) | 2,147 | 1,540 | 3,687 | 321 | 1,844 | [k10-campaign.md](k10-campaign.md) |

## Ledgers

Each campaign writes `docs/test-audit/k<N>-campaign.md` and never edits this README. Parallel campaigns share no file. The table above links the ten ledgers once. Ledger shape: a header with the base sha, declarations before and after, and collected cases before and after. Then one row per in-scope test file: `file | before | after | contracts kept`. Name each kept contract once with its credible failure. The preservation spot-check (five kept contracts, one mutation each, keeper goes red) is recorded in the ledger.
