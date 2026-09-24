# Skill trigger eval (`claude plugin eval`)

This suite asks one question. A user types a natural request: does Claude pick the fno skill for it? Each case sends a prompt that never says the skill name. One case covers each advertised verb (`target`, `think`, `review`, `pr`, `fix`). A sixth case sends a plain language question. Its one grader demands zero skill calls.

The fno eval bank (`evals/bank`) and the trend reader in `crates/fno-agents` grade what a dispatched worker produced. This suite grades the step before that: skill choice on free phrasing, with and without the plugin loaded. It is a different question, so it is not a second harness for the same answer.

## Run it

Claude Code 2.1.269 or later ships the command. Run it from the repo root:

```sh
claude plugin eval . --eval-dir evals/claude-plugin --model claude-opus-5-5 --max-cost-usd 10
```

Every run is a real model call on your account. Six cases at three runs in two arms cost about $5 at list price on `claude-opus-5-5`. Add `--ablation none` to skip the no-plugin arm and halve the cost. Add `--case <name> --runs 1` to try one case once. Each invocation writes `results/<timestamp>/`, which git ignores. No CI job runs this suite.

## Read the table

A positive case has one grader, `tool_used: Skill`, pinned to the fno skill name. A skill that the plugin supplies can never fire without the plugin, so `W/OUT` is 0 by design and `Δ` equals the trigger rate. A `Reached maximum number of turns` note is expected on `target` and `fix`: the skill starts work that the read-only sandbox cannot finish. The grader reads the trace, so the run still passes. The negative case scores in both arms (`arm: both`). Its `Δ` shows whether the plugin makes Claude call a skill on a request that needs none.

## Isolation

Each run gets a throwaway home and working directory. The plugin hooks still run `fno` inside that home. Each run writes a fresh `.fno/` there and never touches your own `~/.fno`. To check this on your machine, add `--keep-temp` to a `--runs 1` case. Before and after the run, compare the mtime of `~/.fno/graph.db` and `~/.fno/graph.db-wal`. The kept directory shows the `home/.fno/` that the hooks wrote.
