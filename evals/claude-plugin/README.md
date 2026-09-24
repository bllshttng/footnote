# Skill trigger and outcome eval (`claude plugin eval`)

This suite asks two questions. A user types a natural request: does Claude pick the fno skill for it? And is the result better with the plugin than without it? Each case sends a prompt that never says the skill name. One case covers each advertised verb (`target`, `think`, `review`, `pr`, `fix`). A sixth case sends a plain language question. Its one grader demands zero skill calls.

The fno eval bank (`evals/bank`) and the trend reader in `crates/fno-agents` grade what a dispatched worker produced. This suite grades a person typing into a fresh session: skill choice on free phrasing, and the task result, with the plugin and without it. It is a different question, so it is not a second harness for the same answer.

## The cases

Each positive case has a `fixture.sh` that builds a small git repo before Claude starts, and two graders:

- `skill-fired` (`tool_used: Skill`, pinned to the fno skill name) records whether the skill fired. In a two-arm run it is an indicator only and does not count toward the score.
- `outcome` (`llm`) grades the result with concrete PASS and FAIL conditions. It reads the fixed `calc.py`, the rate limit in `app.py`, or the cited `findings.md`. For `review` and `pr` it reads the reply. It checks the off-by-one verdict, or an honest answer about the missing remote.

The `fix`, `target`, and `think` fixtures end on a feature branch. On `main`, the fno write guard refuses every edit and asks for a worktree outside the workspace, and the eval sandbox cannot write there.

## Run it

Claude Code 2.1.269 or later ships the command. Run it from the repo root:

```sh
claude plugin eval . --eval-dir evals/claude-plugin --model claude-opus-5-5 \
  --judge-model claude-sonnet-5 --scaffold --allow-tools Bash Edit Write --max-cost-usd 20
```

`--scaffold` runs each `fixture.sh` as you, outside the sandbox. Read them first. They only write inside the run's workspace. `--allow-tools` lets Claude edit files and run the tests. Use a Sonnet judge: the default small judge failed a correct `findings.md`. Every run is a real model call on your account. Add `--ablation none` to skip the no-plugin arm and halve the cost. Add `--case <name> --runs 1` to try one case once. Each invocation writes `results/<timestamp>/`, which git ignores. No CI job runs this suite.

## Read the table

`WITH` and `W/OUT` are the outcome scores with and without the plugin, and `Δ` is their difference. The skill-fired indicator shows in the report for each with-plugin run. The negative case scores in both arms (`arm: both`). Its `Δ` shows whether the plugin makes Claude call a skill on a request that needs none.

On macOS, `/usr/bin/git` is an `xcrun` shim, and the eval's Bash sandbox blocks it in both arms. Claude often finds the real binary under `/Applications/Xcode.app`, or reads the files directly. The `pr` rubric accepts a broken git as a true reason for not opening a PR.

## Isolation

Each run gets a throwaway home and working directory. The plugin hooks still run `fno` inside that home. Each run writes a fresh `.fno/` there and never touches your own `~/.fno`. To check this on your machine, add `--keep-temp` to a `--runs 1` case. Before and after the run, compare the mtime of `~/.fno/graph.db` and `~/.fno/graph.db-wal`. Other sessions on a busy machine also write there, so also search the files that changed for the kept directory's path. The kept directory shows the `home/.fno/` that the hooks wrote.
