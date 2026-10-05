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

## Amendment 5 (2026-09-30, a machine restart, before any results commit)

The machine restarted at 12:21:17Z on 2026-09-30. Harbor last wrote its log at 10:43:47Z. The Run 1 runner wrote its last row at 09:43:11Z. Both drivers died with work in flight. This amendment changes no grade and no rule.

- Run 0. Four claude-code trials were in flight and wrote no result: video-processing, protein-assembly, path-tracing and compile-compcert. Nothing exists to score, so `harbor jobs resume` runs each one again. The six trials that wrote a result stand. The opencode and Terminus 2 arms then run in the same seeded order.
- Run 1. The opencode lane had reached task 6 of 10. At most one attempt was in flight, and it wrote no row. The lane finishes through the top-up script in one pass. The zcode lane runs next. Then the top-up runs its 3 passes over the claude and opencode lanes.
- Resume token. Harbor stores the z.ai token masked in the job file, and the first resume at 14:13Z sent the masked value. The four reruns ended in HTTP 401 with zero tokens. The job was stopped at 14:33Z with four more trials still in setup. No agent reached the model, so all eight are parked and run again with the real token.
- The driver then started the opencode job out of order. It was stopped within one minute, before any agent ran. That job starts fresh after the claude-code arm.
- Pause. The 1-minute load read 359 on 12 cores at 16:43Z. The fleet lead ordered a pause, and both runs stopped at 16:45:48Z. Harbor has no drain, so four claude-code trials in flight stopped with no result. One opencode attempt stopped with no row. All five run again on resume, and the 24 claude-code results stand.
- Load. From 16:40Z a sampler records the load average once a minute. run-0.md and run-1.md print the load beside each timeout and stall. They also print each rate without the results that ran above load 50. No load record exists before 16:40Z.
- A trial or attempt that a restart kills again gets the same treatment. run-0.md and run-1.md count each one by reason.

## Amendment 6 (2026-09-30, a new machine, before any results commit)

The user moved the study to an Intel iMac with 128 GB of RAM. Every Terminal-Bench 2 task image is `linux/amd64`. The first machine was an arm64 Mac, so it emulated every task. The 1-minute load there swung from 24 to 331 during Run 0. A load guard paused both runs again at 18:54:58Z, 9 minutes after a resume.

- Pilot. Every result from the arm64 Mac is a pilot: all of Run 0 and all of Run 1 to date. The results files report the pilot apart from the study. A pilot result fires no decision rule.
- Clean restart. Both runs restart clean on the iMac. Run 0 runs the one-task smoke per arm first, then the same seeded arm order. Run 1 runs the same seeded lane order, then the top-up. Harbor, the bank, the prices and every rule stay the same.
- Concurrency. Run 0 runs min(4, half the physical cores) trials at once. Run 1 runs one attempt at a time beside it.
- Scripts. The run scripts move into `evals/harness-fit/run/`, so the iMac runs the reviewed code from this branch. `evals/harness-fit/setup-imac.sh` installs Docker, Harbor, the harness CLIs and the key. The zcode adapter stays out of the repo, so zcode reads `unavailable` unless its files are copied in.
- Account. The z.ai account and key stay the same. Rate limits follow the key, not the machine, so the 1302 rule stays.
- Load. The sampler runs on the iMac from the first trial. The results print the load beside each timeout and stall.
- Dollars. The pilot spend counts against the $200 ceiling.
- Results reach this branch by push from the iMac.

## Amendment 7 (2026-10-01, the move waits, before any results commit)

The user deferred the move: "For now let's continue here." Both runs continue on the arm64 Mac from where they paused.

- Rows. The arm64 rows count as the study again, not as a pilot. If the iMac run happens later, run-0.md and run-1.md report each machine apart, and the iMac rows decide.
- Emulation. Every Terminal-Bench 2 task runs under amd64 emulation on this machine. run-0.md names this beside every rate.
- Load. The sampler keeps a load reading beside each result. If the 5-minute load stays at 150 or more for 10 minutes, a guard pauses both runs. The fleet's line of 48 cannot hold while the study runs.
- Concurrency stays at 4 Run 0 trials. Every other rule stays.

## Amendment 8 (2026-10-01, low load, before any results commit)

The 150 guard paused both runs at 03:17Z. The user then said: "resume but with low loads, let's not crush our systems."

- Concurrency. Run 0 runs 2 trials at a time, down from 4. A resumed job takes the new value in both `config.json` and `lock.json`.
- Guard. If the 5-minute load stays at 48 or more for 5 minutes, the guard pauses both runs. That is the fleet's own line of 4 per core. When the load stays at 24 or less for 10 minutes, the guard resumes both runs. No mail goes out. `logs/guard.jsonl` records each change.
- Paused trials. A pause stops in-flight trials with no result. They rerun on resume and are never counted as failures.
- Docker networks. Each killed trial left its network behind. At 02:36Z, 46 trials failed on "all predefined address pools have been fully subnetted". Those trials never started an agent. They reran and count as infrastructure, not as the arm. A pause now removes the leftover networks.
- Every other rule stays.

## Amendment 9 (2026-10-01, drain, never kill, before any results commit)

Under Amendment 8 the guard paused every 20 to 60 minutes. Each pause killed the trials in flight, and a trial takes 15 to 45 minutes. Run 0 finished 2 trials in 3 hours. The user said the guards make the study struggle. Vellum then ruled on the question: drain, never kill.

- Drain. When the 5-minute load stays at 48 or more for 5 minutes, both runs start no new trial or attempt. In-flight work finishes and counts.
- Lift. When the load stays at 24 or less for 10 minutes, new work starts again.
- Kill. Only a load of 150 or more for 5 minutes kills both runs. Killed work reruns on resume, as before.
- Mechanism. A `logs/drain` file holds each new Run 0 trial before its first timer, so a held trial loses nothing. Run 1 checks the same file before each attempt. The zcode lane now runs one attempt at a time through the top-up for the same reason.
- Concurrency stays at 2 Run 0 trials. Every other rule stays.

## Amendment 10 (2026-10-02, the iMac run and the Claude Code 1M window, before any results commit)

The user moved the study to the Intel iMac on 2026-10-02. Both runs started clean there at 11:24Z, as Amendment 6 describes. The iMac rows decide, and the arm64 rows are reported apart, as Amendment 7 says.

- Concurrency. Run 0 runs 4 trials at once on the iMac, the Amendment 6 value for 8 physical cores. Amendments 8 and 9 lowered it to 2 on the arm64 Mac only. The iMac runs the load sampler. It runs no drain guard.
- Claude Code model. The user ruled that the Claude Code harness must ask for `glm-5.3-flash[1m]`. The words: "you need to pass in [1m] otherwise the cap is low." The suffix is Claude Code syntax for the 1M context window. A probe ran at 17:59Z on the pinned Claude Code 2.1.286. It read a context window of 1,000,000 with the suffix and 200,000 without it. z.ai answered both. The Run 0 claude-code arm and the Run 1 claude lane now ask with the suffix. Effort stays high. The opencode, pi and Terminus 2 model strings stay the same.
- Not comparable. Every Claude Code result from before this amendment ran on the 200,000 window. That covers the arm64 Run 0 claude-code arm and its smokes. It covers the arm64 Run 1 claude lane and one Run 1 claude attempt on the iMac. The results files report them apart. They fire no decision rule. The iMac attempt sits in `logs/pre-amendment-10` in the run workspace, so its task gets three fresh attempts.
- Smoke, a false negative. The iMac claude-code smoke ran at 16:03Z without the suffix. It hit the 900 second task timeout. Harbor counts tokens per finished turn, and no turn finished. The job read zero tokens, so the driver logged the arm `unavailable`. The agent log shows the session reached glm-5.3-flash and streamed about 40,000 thinking tokens. Only a smoke that errors before any model call makes an arm `unavailable`, so this smoke passed. When the token count is zero, the driver now reads the agent log.
- Arm order. The seeded order was pi, claude-code, opencode, terminus-2. The driver had started the opencode arm before the false negative was read. In-flight trials are never killed for an order, so claude-code runs last, after its smoke runs again with the suffix. run-0.md names the order that ran.
- Identity. A Claude Code transcript stores `glm-5.3-flash`, never the suffix. The observe door compared the two strings exactly, so every row asked with the suffix read `substituted`. It now compares by model family, the rule the agent registry uses. One trailing bracket suffix is dropped from each side. A different model still reads `substituted`. The Run 1 scorer applies the same rule to any row an older binary wrote.
- Run 1 restarts. Run 1 was stopped and started again four times on the iMac. The first start resolved no lane, because the installed CLI came from a different commit than the branch's fno-agents. The second and third were stopped on the pi refusals below. Those refusals later proved to be the branch's own behaviour. At 11:51Z a plugin installer replaced the installed CLI mid-lane, and 29 claude attempts never started. No worker started in any set-aside attempt and no token was spent. Those rows sit outside the history file, in `logs/aborted-start-1` to `aborted-start-4` in the run workspace. Run 1 now runs the branch CLI from its own environment in the run workspace.
- pi in Run 1. Every pi attempt ends before a worker starts: the headless substrate has no pi lane on this branch. The lane reads `unavailable` on both machines.
- Every other rule stays.

## Amendment 11 (2026-10-02, one load rule for every arm, before any results commit)

The user ruled: "the key is consistency. i hope the trials all have similiar conditions." A process storm starved the iMac from about 18:07Z to 18:38Z. A study shim made a cleanup command call itself, and about 9,000 processes piled up. No trial was killed.

- Rule. A unit is one Run 0 trial or one Run 1 attempt. When the 5-minute load average passed 16 during a unit, it is an infrastructure exclusion. It runs again. Any one sample between its start and its end is enough. 16 is two per physical core on the iMac. The sampler reads once a minute.
- Scope. The rule applies to every arm and lane on the iMac, finished or not. A re-run uses the same arm, job and settings.
- When it was set. The threshold was chosen after reading the load series and before reading any outcome. Before 18:00Z the 5-minute load never passed 12.0. It passed 16 from 18:21Z to 18:38Z and peaked at 43.4.
- Why the 5-minute load. One build can spike the 1-minute load. It touched 17.7 in normal running, where the 5-minute load stayed under 12.
- In-flight trials. A trial already running in a bad window finishes and is then set aside. Nothing is killed for the rule.
- Concurrency. Run 0 stays at 4 trials for every remaining arm, as the pi arm ran.
- Claude Code version. The Run 0 claude-code container now installs Claude Code 2.1.286, the Run 1 pin. The 16:03Z smoke had installed 2.1.287.
- Images. Unused task images were removed at 18:29Z to free disk. Later arms pull them again. A pull happens in environment setup, before the agent timer starts.
- No setting, image or environment changes mid-arm from here, unless the machine is at risk.
- Every other rule stays.

## Amendment 12 (2026-10-04, one retry for every rate-limited trial, after the first results commit)

The user ruled that the rate-limited Run 0 trials run again before the results ship. The reason is the same as for Amendment 11: the arms are graded under similar conditions. The first results excluded 54 Run 0 trials under the 1302 rule: 7 pi, 27 claude-code, 7 opencode and 13 Terminus 2. So the arms were graded on different task sets.

- Rule. Every Run 0 trial excluded under the 1302 rule, in every arm, runs once more. It runs in the same job, with the same model, effort, Claude Code pin and concurrency. The original attempt is set aside, as Amendment 11 does.
- One retry. A re-run that ends in a 1302 timeout again stays excluded. It does not run a third time.
- Spend. The cost tables count the attempts in the final record, as they did after Amendment 11. The set-aside attempts and their spend are reported apart in run-0.md. decision.md shows the cost margin both with and without them.
- Order. The arms run again one at a time, in the seeded order: pi, claude-code, opencode, Terminus 2.
- When it was set. The rule was set after the first results were written and read, and before any re-run started. At commit time, no re-run outcome existed. run-0.md and decision.md keep the first results as a labelled section beside the new ones.
- Run 1. Nothing changes. Its top-up already refilled every rate-limited attempt.
- Every other rule stays, including the Amendment 11 load rule, which also applies to the re-runs.

## Amendment 13 (2026-10-05, the 1302 test reads the error, after the second results commit)

A review of the scorer found that it did not apply Amendment 2 as written. The rule names "a 1302 error in its agent log". The scorer counted any timeout whose agent log held the four characters `1302` anywhere. Those characters turn up in ordinary output. Claude Code's thinking-token counter passes 1302 in most long sessions. uuids, line numbers, file sizes and timestamps carry them too.

- What the bug did. 91 timeouts were flagged as rate limits across the record and the Amendment 12 retries. 17 carried z.ai's actual error. The other 74 were ordinary timeouts.
- What else it missed. opencode prints the error as `Rate limit reached for requests` with no code. The substring test missed 7 opencode timeouts that did hit the limit, and graded them as failures.
- The test now. If a timeout's agent log carries `Rate limit reached for requests`, it is a 1302 exclusion. z.ai sends that message with code 1302 in every harness's log. `run/score_run0.py` applies it. `HARNESS_FIT_LEGACY_1302=1` restores the old test, so earlier tables stay reproducible.
- The record. Amendment 4 says a started attempt is never run again and is scored as it stands. So an original attempt stands unless it really hit the limit. That covers 44 of the 54 trials Amendment 12 retried. Their originals stand as graded timeouts. Their retries are discarded and reported apart. For the other 10, which really hit the limit, the retry stands.
- The 7 opencode timeouts. Amendment 12 gives every rate-limited trial one retry. These 7 never had one. Each runs once more in the same job, with the same settings and concurrency 4. A second rate limit stays excluded.
- When it was set. After the second results were written and read, and before the 7 retries started. At commit time, none of those 7 outcomes existed. run-0.md and decision.md keep both earlier sets of tables, labelled.
- Run 1. Its rate-limit check already reads `Rate limit reached` in the row's reason, so nothing changes there.
- Every other rule stays.

## Scope and limits

One machine, one model, 10 replay tasks, 3 repeats: n is small. Bootstrap intervals at this n are wide, and a difference inside the interval is noise. An arm under 20 graded attempts is underpowered and fires no rule alone. Run 0 and Run 1 grade different task distributions (Terminal-Bench 2 is generic, the replay bank is footnote's own), so arms can differ across runs. The Terminus 2 reference tells a harness effect from a model effect. It does not measure footnote's own loop. The collector is the runner's own history rows. The observe door reads the attempt's own transcript for identity and usage. When the transcript is unreadable, `usage` is null, never zero. A row whose identity reads `unverified` still counts toward attempts but never toward a rule.
