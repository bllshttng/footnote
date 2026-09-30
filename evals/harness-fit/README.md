# GLM harness-fit study (x-632f)

Compare how well the fleet's production model (`glm-5.3-flash` through the z.ai endpoint) is served by the harnesses the fleet actually runs it on. One attempt = one bank task in a fresh disposable worktree, one lane (harness + model + effort), one grade. This is an observed comparison, not proof of causality.

## Arms

**Run 0 (Terminal-Bench 2 through Harbor, external tooling).** Five arms: claude-code with the fleet's zai env. Opencode with a z.ai provider. Zcode with its model in its own config (no --model flag). Pi with a z.ai provider. Terminus 2 as the neutral reference. A one-task smoke per arm proves each CLI reaches the endpoint before the 89-task pass. When an arm's smoke fails, the arm is recorded `unavailable` with the error. The other arms still run.

**Run 1 (the replay bank, this repo's eval runner).** The same four harness lanes through `fno doctor evals run --lane <lane> --cohort harness-fit-<lane> --repeat 3`. A zcode lane the spawn refuses is recorded `unavailable`, never scored, and zcode is judged on Run 0 only.

## Fixed per comparison

These factors stay fixed per comparison. The model string is `glm-5.3-flash` at effort `high`. The provider endpoint is z.ai. The claude harness uses base url `https://api.z.ai/api/anthropic`. Every arm runs with a bypass permission mode. Each attempt gets a 45-minute wall budget, this machine, and a fresh disposable worktree. Arm order is randomized and each comparison repeats 3 times. Harness versions and the footnote plugin sha are recorded per run. The price table in `manifest.json` is filled before Run 0.

## Sampling rule (declared before any node is picked)

The eligible population: footnote PRs merged 2026-09-01 through 2026-09-28 that added at least one NEW test file, with tests that run in under 10 minutes. Epic and meta changes have no test files, so they self-exclude. The sample: order the eligible by merge time, then take every k-th entry with k = round(N/10) and offset k/2, for 10 tasks. Contamination checks: the task prompt carries no merge sha and no PR number. When an attempt's transcript shows it read commits past its fixture (a later sha through `git log` or `git show`), the attempt is excluded as contaminated.

## Metrics

The metrics: accepted changes, stalls and BLOCKED results, tokens, dollars per attempt and per accepted change, and median wall time. An accepted change means the replay bank's hidden tests pass from the merge commit and the scoped test subset is green. Tokens count input, output, cache read, and cache write. The dollar metrics price the tokens from the manifest price table through `evals-trend --by-cohort --prices`.

## Exclusions

`substituted` rows (the door observed a different harness or model than the lane requested), `unavailable` and infrastructure failures, and contaminated attempts (`excluded_reason: contaminated` on the row). Each is counted by reason (AC3-EDGE), never scored.

## Dollar ceiling and stop rule

The dollar ceiling is **$200.00 total across Run 0 and Run 1**. When the next attempt can push the cumulative spend past the ceiling, the worker stops and reports. It never raises the ceiling. The ceiling value is part of this preregistration. Changing it requires a new preregistration commit dated before the first results commit. The operator reviews this ceiling at PR review, before any paid run starts.

## Decision rules (applied in order)

1. If opencode or zcode beats Claude Code on accepted changes, recommend a GLM routing change. A tie counts only with 30% lower cost per accepted change. The recommendation targets `config.routing.models` and stays a recommendation for the operator. Never edit production lanes from this study.
2. If every external harness shares one named GLM failure, name it. The failure must be one a loop change can fix: tool-call format, context handling, or write corruption. Child 3 (`-H footnote` Stage 1) targets it.
3. If no arm differs beyond noise (about 15 points at this n), record that. Recommend closing the native branch under AC4-EDGE.

Report per-task paired results with bootstrap intervals (`fno doctor evals report --by-cohort`).

## Amendment 1 (2026-09-29, before any result)

This commit lands before the first results commit. It settles six points the first declaration left open. `manifest.json` carries the same values under `amendment_1`.

- Repeats. Run 0 is one pass per arm over the 89 Terminal-Bench 2 tasks, paired by task. The 3-repeat rule applies to Run 1 only. Terminal-Bench 2 already gives n = 89 per arm.
- Run 0 timeout. Each Terminal-Bench 2 task keeps its own agent timeout at `timeout_multiplier: 1.0`, the published protocol. Run 0 rates then compare with the public board. The 45-minute budget applies to Run 1.
- Terminus 2 model. Terminus 2 runs glm-5.3-flash through z.ai like every other arm. The model stays fixed, so this arm measures the harness. The Sonnet price row is gone.
- Prices. Per 1M tokens, glm-5.3-flash costs 0.15 USD input, 0.03 cached input and 0.50 output. Source: the z.ai pricing page, read 2026-09-29. z.ai lists no cache-write charge and bills written tokens as input, so cache write is 0.15. The report prices by the exact model string, so every string an arm can report carries the same rates.
- Concurrency. One Harbor job runs at a time with 4 concurrent trials. The Docker VM has 12 CPUs and 16 GB. Run 1 runs one attempt at a time beside Run 0.
- Stop-rule check. Harbor starts a whole arm at once, so the check runs per job. Before each job, the driver adds spent dollars to a projection: 3 times the smoke cost per task, times 89. The sum must stay under the ceiling. Before each Run 1 lane, spent dollars plus 3 times the lane's projection must stay under it.

Harbor 0.23.0 runs through `uvx`. The zcode arm needs a Harbor adapter. That adapter lives in the run workspace, not in this repo.

## Amendment 2 (2026-09-29, after the smokes, before any Run 0 or Run 1 result)

The smokes are not results: they check that each arm reaches z.ai. They exposed three measurement gaps, so these rules land first.

- Rate limits. z.ai answered smoke requests with error 1302, "Rate limit reached for requests". The fleet shares the same coding-plan account. A trial that ends in a timeout, with a 1302 error in its agent log, is an infrastructure failure. It is excluded and counted by reason. run-0.md also prints each arm's rate with those trials scored, as a check on the exclusion.
- Reasoning tokens. z.ai bills reasoning tokens as output. Harbor's opencode adapter counts only visible output: one smoke step reported 14 output tokens beside 31,986 reasoning tokens. Where an arm's log reports reasoning apart from output, the report adds it to output.
- Token source. Run 0 tokens come from Harbor's per-trial agent context. A trial whose arm reported no usage reads `unmeasured`, never zero.

## Amendment 3 (2026-09-29, the bank, before any Run 1 result)

A grade-only dry run checked every replay task before Run 1. The hidden tests must pass at the merge sha and fail at the first parent, with no worker. The dry run found five bad tasks and one bad grade shape. `nodes.md` lists each change.

- The repo has no Cargo workspace manifest. Each Rust grade now runs `cargo test` inside its own crate.
- x-632f kept no list of the eligible population, and the list cannot be rebuilt to match its picks. So the population for replacements is rebuilt from local git. It holds the first-parent PR merges on `origin/main` dated 2026-09-01 through 2026-09-28 (UTC) that add at least one test file. Each must name a node the graph resolves, because the prompt is that node's title and details. That gives 242 entries.
- The replacement rule: a bad task gives way to the next unsampled entry after it in merge order. That entry's grade must pass the dry run, and its hidden tests must run, not skip.
- A task whose node the graph cannot resolve is bad, because its prompt cannot come from the node. Three of x-632f's tasks carried a one-sentence prompt written by hand, and all three are replaced.

## Amendment 4 (2026-09-29, attempts that never started, before any results commit)

Two infrastructure stops hit the runs while they were in flight. This amendment lands after those rows exist and before any results commit. It changes no grade and no rule. It says only what happens to an attempt that never started.

- Docker. OrbStack was quit at 23:06:42Z. After that, 68 pi trials failed in `docker compose` before the agent ran. The claude-code, opencode and Terminus 2 jobs ran no trial. A trial whose environment never started is an infrastructure exclusion. When Docker answers again, `harbor jobs resume -f RuntimeError` reruns those pi trials in place. The other arms then run in the same seeded order.
- Provider cap. When the fleet already holds every zai lane, the spawn gate refuses a Run 1 worker (`provider_cap`). The row reads `unavailable` and the worker never runs. After the lanes finish, a top-up pass reruns missing graded attempts. It stops at 3 graded attempts per task, or after 3 passes. Each refusal stays in history and is counted by reason.
- Neither rerun touches a trial or attempt that started. A started attempt that fails is scored as it stands.
- Stalls. A Run 1 worker that runs out its 45-minute budget started. The spawn exits 12 (opencode) or 124 (claude), and the runner files the row `unavailable` with no grade. This study scores that row as a stall: an attempt with no accepted change. run-1.md prints each lane's rate with and without stalls.
- Run 1 rate limits. z.ai can kill a running worker with "Rate limit reached for requests". That row is an infrastructure exclusion, the same as Run 0's 1302 rule, and the top-up refills its slot. run-1.md also prints each lane's rate with those rows scored as failures.
- opencode identity. The observe door compared the requested `zai-coding-plan/glm-5.3-flash` with the stored modelID `glm-5.3-flash`. So every opencode row read `substituted`. It also left reasoning tokens out of output. The fix joins `providerID/modelID` and adds reasoning. The run keeps its binary. So run-1.md re-reads each opencode row from the store by session id, under the fixed rule.

## Scope and limits

One machine, one model, 10 replay tasks, 3 repeats: n is small. Bootstrap intervals at this n are wide, and a difference inside the interval is noise. An arm under 20 graded attempts is underpowered and fires no rule alone. Run 0 and Run 1 grade different task distributions (Terminal-Bench 2 is generic, the replay bank is footnote's own), so arms can differ across runs. The Terminus 2 reference tells a harness effect from a model effect. It does not measure footnote's own loop. The collector is the runner's own history rows. The observe door reads the attempt's own transcript for identity and usage. When the transcript is unreadable, `usage` is null, never zero. A row whose identity reads `unverified` still counts toward attempts but never toward a rule.
