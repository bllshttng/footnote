# Run 0 results: Terminal-Bench 2 through Harbor

This is the iMac run, the one that decides (Amendments 6, 7 and 10). The arm64 pilot sits in its own section below and fires no rule.

## Setup

- Machine: Intel iMac, 8 physical cores, 128 GiB, native x86_64. Task images are `linux/amd64`, so nothing was emulated.
- Harbor 0.23.0. One job at a time, 4 trials at once (Amendment 6). Each task keeps its own agent timeout at multiplier 1.0 (Amendment 1).
- Model: `glm-5.3-flash` through z.ai at effort high. The claude-code arm asks for `glm-5.3-flash[1m]` (Amendment 10).
- Harness versions inside the task containers: Claude Code 2.1.286, opencode 1.18.34, pi 1.0.0. Two opencode trials and two pi trials recorded no version.
- Arm order ran: pi, opencode, terminus-2, then claude-code. The seeded order put claude-code second. Its first smoke read as a false negative and the arm ran last (Amendment 10).
- zcode is `unavailable`: the iMac has no ZCode.app, and the zcode adapter stays out of this repo (Amendment 6).

## Per arm

Rate is passed over graded. The interval is a 95% Wilson interval. A trial that timed out with a 1302 rate-limit error in its agent log is an infrastructure exclusion (Amendment 2). The last rate column scores those trials as failures, as a check on that rule.

| Arm | Trials | Graded | Passed | Rate | 95% interval | Excluded (1302) | Rate, 1302 scored | Other errors among graded | Compute hours | Median trial minutes |
|---|---|---|---|---|---|---|---|---|---|---|
| pi | 89 | 82 | 45 | 54.9% | 44.1% to 65.2% | 7 | 51.7% | AgentTimeoutError 3, NonZeroAgentExitCodeError 3 | 16.8 | 7.5 |
| opencode | 89 | 82 | 53 | 64.6% | 53.8% to 74.1% | 7 | 60.7% | AgentTimeoutError 12, ApiRateLimitError 1, NonZeroAgentExitCodeError 2 | 21.9 | 10.6 |
| terminus-2 | 89 | 76 | 46 | 60.5% | 49.3% to 70.8% | 13 | 53.9% | AgentTimeoutError 20 | 26.3 | 15.5 |
| claude-code | 89 | 62 | 49 | 79.0% | 67.4% to 87.3% | 27 | 57.3% | NonZeroAgentExitCodeError 2 | 25.1 | 13.7 |
| zcode | 0 | 0 | 0 | unavailable | | | | no ZCode.app | | |

## Tokens and dollars

Prices are the manifest's list prices per 1M tokens: 0.15 input, 0.03 cache read, 0.50 output. A trial with no usage reads `unmeasured`, never zero (Amendment 2).

| Arm | Input | Cache read | Output | Dollars | Dollars per pass | Unmeasured trials |
|---|---|---|---|---|---|---|
| pi | 30,294,499 | 28,017,856 | 1,561,361 | 1.96 | 0.044 | 4 |
| opencode | 44,917,880 | 41,429,056 | 1,630,814 | 2.58 | 0.049 | 2 |
| terminus-2 | 31,062,108 | 28,792,896 | 2,644,808 | 2.53 | 0.055 | 0 |
| claude-code | 71,337,522 | 66,417,664 | 2,075,997 | 3.77 | 0.077 | 2 |
| total | | | | 10.84 | | |

## Paired by task

Each row compares two arms on the tasks both graded. The difference is the first arm's pass rate minus the second's, in points. The interval is a 95% bootstrap over tasks (4,000 resamples, seed 272).

| Arm vs arm | Tasks both graded | First only passed | Second only passed | Difference | 95% interval |
|---|---|---|---|---|---|
| claude-code vs terminus-2 | 59 | 10 | 4 | +10.2 | -1.7 to +22.0 |
| opencode vs terminus-2 | 72 | 11 | 6 | +6.9 | -4.2 to +18.1 |
| pi vs terminus-2 | 71 | 7 | 10 | -4.2 | -15.5 to +7.0 |
| opencode vs claude-code | 59 | 3 | 4 | -1.7 | -10.2 to +6.8 |
| pi vs claude-code | 60 | 2 | 10 | -13.3 | -25.0 to -3.3 |

## What the table can and cannot say

- claude-code has the highest raw rate and the most exclusions. 27 of its 89 trials timed out on a 1302 rate limit, against 7 to 13 in each other arm.
- The arm64 pilot found that Claude Code retries a 1302 until the task timeout, where pi stops. So the 1302 rule drops more claude-code trials. Those trials are not a random draw, so its raw rate is the least comparable.
- On the tasks both graded, claude-code and opencode differ by under 2 points, inside the noise. On shared tasks claude-code beats pi by about 13 points, and that interval excludes zero.
- With 1302 trials scored as failures, claude-code falls to 57.3% and sits beside the other arms.

## Exclusions and load

- Amendment 11 load rule: 8 opencode trials ran while the 5-minute load passed 16, from 18:00Z to 19:07Z on 2026-10-02. They were set aside and run again in the same job at 12:00Z to 12:44Z on 2026-10-03. The table counts the re-runs. The set-aside trials are in `aside-amendment-11/` in the run workspace.
- No graded trial in the table ran with the 5-minute load above 16. The highest 1-minute load during any graded trial was pi 14.7, opencode 22.7, terminus-2 19.8, claude-code 24.9.
- 18:29Z on 2026-10-02: unused task images were removed to free disk. Later arms pulled them again during environment setup, before the agent timer (Amendment 11).
- The appendix lists every timeout with the highest 1-minute load during it (Amendment 5).

## Pilot: the arm64 Mac (not comparable, fires no rule)

The first machine emulated every task image. Its claude-code rows ran on the 200,000-token window (Amendment 10). These rows are reported apart.

| Arm | Graded | Passed | Rate | Excluded (1302) |
|---|---|---|---|---|
| pi | 80 | 28 | 35.0% | 9 |
| claude-code | 16 | 12 | 75.0% | 13 |

The pilot's opencode and terminus-2 arms ran no trial.

## Appendix: every timeout, with its load

`1302` marks a timeout with a rate-limit error in the agent log. Those are excluded. The rest are graded failures.

| Arm | Task | 1302 | Highest 1-minute load |
|---|---|---|---|
| pi | caffe-cifar-10 | yes | 6.2 |
| pi | cobol-modernization | no | 4.7 |
| pi | extract-moves-from-video | no | 13.5 |
| pi | gcode-to-text | yes | 5.8 |
| pi | llm-inference-batching-scheduler | yes | 7.0 |
| pi | make-doom-for-mips | yes | 14.7 |
| pi | make-mips-interpreter | yes | 7.9 |
| pi | mteb-leaderboard | yes | 13.5 |
| pi | train-fasttext | yes | 5.7 |
| pi | tune-mjcf | no | 13.5 |
| opencode | caffe-cifar-10 | no | 14.3 |
| opencode | crack-7z-hash | yes | 9.6 |
| opencode | dna-assembly | no | 14.3 |
| opencode | extract-moves-from-video | no | 7.5 |
| opencode | feal-linear-cryptanalysis | no | 19.8 |
| opencode | gcode-to-text | yes | 6.8 |
| opencode | gpt2-codegolf | no | 5.5 |
| opencode | largest-eigenval | no | 7.5 |
| opencode | mailman | no | 8.7 |
| opencode | make-doom-for-mips | yes | 6.5 |
| opencode | make-mips-interpreter | yes | 8.8 |
| opencode | mcmc-sampling-stan | yes | 10.9 |
| opencode | model-extraction-relu-logits | no | 6.4 |
| opencode | path-tracing | yes | 14.3 |
| opencode | rstan-to-pystan | no | 20.7 |
| opencode | schemelike-metacircular-eval | no | 6.0 |
| opencode | torch-pipeline-parallelism | no | 6.0 |
| opencode | train-fasttext | yes | 7.5 |
| opencode | tune-mjcf | no | 7.5 |
| terminus-2 | adaptive-rejection-sampler | no | 9.5 |
| terminus-2 | build-cython-ext | no | 5.1 |
| terminus-2 | build-pmars | no | 19.3 |
| terminus-2 | caffe-cifar-10 | no | 5.1 |
| terminus-2 | chess-best-move | yes | 7.1 |
| terminus-2 | cobol-modernization | no | 6.0 |
| terminus-2 | dna-assembly | yes | 6.6 |
| terminus-2 | extract-elf | no | 9.6 |
| terminus-2 | extract-moves-from-video | yes | 15.1 |
| terminus-2 | feal-differential-cryptanalysis | no | 10.8 |
| terminus-2 | feal-linear-cryptanalysis | no | 12.2 |
| terminus-2 | filter-js-from-html | no | 6.8 |
| terminus-2 | gcode-to-text | no | 8.1 |
| terminus-2 | gpt2-codegolf | no | 11.2 |
| terminus-2 | largest-eigenval | yes | 6.0 |
| terminus-2 | make-doom-for-mips | yes | 7.7 |
| terminus-2 | make-mips-interpreter | yes | 6.3 |
| terminus-2 | model-extraction-relu-logits | no | 6.1 |
| terminus-2 | overfull-hbox | yes | 5.0 |
| terminus-2 | password-recovery | no | 11.1 |
| terminus-2 | path-tracing | yes | 6.6 |
| terminus-2 | path-tracing-reverse | yes | 9.0 |
| terminus-2 | protein-assembly | yes | 6.6 |
| terminus-2 | query-optimize | yes | 7.8 |
| terminus-2 | raman-fitting | yes | 6.1 |
| terminus-2 | regex-chess | no | 7.3 |
| terminus-2 | reshard-c4-data | no | 10.3 |
| terminus-2 | sanitize-git-repo | no | 19.3 |
| terminus-2 | schemelike-metacircular-eval | no | 10.1 |
| terminus-2 | torch-pipeline-parallelism | no | 6.8 |
| terminus-2 | tune-mjcf | no | 19.3 |
| terminus-2 | winning-avg-corewars | yes | 7.3 |
| terminus-2 | write-compressor | no | 7.2 |
| claude-code | adaptive-rejection-sampler | yes | 9.9 |
| claude-code | caffe-cifar-10 | yes | 10.9 |
| claude-code | cancel-async-tasks | yes | 11.9 |
| claude-code | cobol-modernization | yes | 23.9 |
| claude-code | compile-compcert | yes | 14.1 |
| claude-code | dna-assembly | yes | 14.1 |
| claude-code | extract-moves-from-video | yes | 24.9 |
| claude-code | filter-js-from-html | yes | 15.1 |
| claude-code | gcode-to-text | yes | 13.3 |
| claude-code | gpt2-codegolf | yes | 9.9 |
| claude-code | headless-terminal | yes | 15.7 |
| claude-code | largest-eigenval | yes | 11.8 |
| claude-code | make-doom-for-mips | yes | 12.5 |
| claude-code | make-mips-interpreter | yes | 15.3 |
| claude-code | model-extraction-relu-logits | yes | 15.1 |
| claude-code | overfull-hbox | yes | 12.1 |
| claude-code | path-tracing-reverse | yes | 16.1 |
| claude-code | query-optimize | yes | 13.2 |
| claude-code | raman-fitting | yes | 13.5 |
| claude-code | regex-chess | yes | 13.3 |
| claude-code | rstan-to-pystan | yes | 15.7 |
| claude-code | schemelike-metacircular-eval | yes | 23.9 |
| claude-code | torch-pipeline-parallelism | yes | 23.9 |
| claude-code | train-fasttext | yes | 23.9 |
| claude-code | tune-mjcf | yes | 24.9 |
| claude-code | winning-avg-corewars | yes | 15.7 |
| claude-code | write-compressor | yes | 12.7 |
