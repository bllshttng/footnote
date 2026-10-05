# Run 0 results: Terminal-Bench 2 through Harbor

This is the iMac run, the one that decides (Amendments 6, 7 and 10). The tables count the record Amendment 13 defines. A trial is a 1302 exclusion only when its agent log carries z.ai's rate-limit error. Two earlier sets of tables used a substring test that misread ordinary timeouts as rate limits. They sit in their own sections below, labelled. The arm64 pilot sits in its own section and fires no rule.

The numbers come from `run/report_run0.py`. Its `final` mode prints the tables here. With `HARNESS_FIT_LEGACY_1302=1`, its `first` and `retried` modes reproduce both earlier sets exactly.

## Setup

- Machine: Intel iMac, 8 physical cores, 128 GiB, native x86_64. Task images are `linux/amd64`, so nothing was emulated.
- Harbor 0.23.0. One job at a time, 4 trials at once (Amendment 6). Each task keeps its own agent timeout at multiplier 1.0 (Amendment 1).
- Model: `glm-5.3-flash` through z.ai at effort high. The claude-code arm asks for `glm-5.3-flash[1m]` (Amendment 10).
- Harness versions inside the task containers: Claude Code 2.1.286, opencode 1.18.34, pi 1.0.0. Two opencode trials and two pi trials recorded no version.
- Arm order ran: pi, opencode, terminus-2, then claude-code. The seeded order put claude-code second. Its first smoke read as a false negative and the arm ran last (Amendment 10).
- Retries of rate-limited trials ran on 2026-10-04 from 04:58Z to 12:31Z (Amendment 12) and on 2026-10-05 from 16:33Z to 17:51Z (Amendment 13).
- zcode is `unavailable`: the iMac has no ZCode.app, and the zcode adapter stays out of this repo (Amendment 6).

## Per arm

Rate is passed over graded. The interval is a 95% Wilson interval. A trial that timed out with z.ai's rate-limit error in its agent log is an infrastructure exclusion (Amendments 2 and 13). The last rate column scores those trials as failures, as a check on that rule.

| Arm | Trials | Graded | Passed | Rate | 95% interval | Excluded (1302) | Rate, 1302 scored | Other errors among graded | Compute hours | Median trial minutes |
|---|---|---|---|---|---|---|---|---|---|---|
| pi | 89 | 89 | 45 | 50.6% | 40.4% to 60.7% | 0 | 50.6% | AgentTimeoutError 10, NonZeroAgentExitCodeError 3 | 16.8 | 7.5 |
| opencode | 89 | 88 | 56 | 63.6% | 53.2% to 72.9% | 1 | 62.9% | AgentTimeoutError 16, ApiRateLimitError 1, NonZeroAgentExitCodeError 2 | 21.6 | 10.2 |
| terminus-2 | 89 | 89 | 48 | 53.9% | 43.6% to 63.9% | 0 | 53.9% | AgentTimeoutError 33 | 26.3 | 15.5 |
| claude-code | 89 | 89 | 51 | 57.3% | 46.9% to 67.1% | 0 | 57.3% | AgentTimeoutError 27, NonZeroAgentExitCodeError 2 | 25.1 | 13.7 |
| zcode | 0 | 0 | 0 | unavailable | | | | no ZCode.app | | |

## What Amendment 13 changed

The scorer read any `1302` in an agent log as a rate limit. Claude Code's thinking-token counter passes 1302 in most long sessions. uuids, line numbers, file sizes and timestamps carry the same characters.

| Arm | Flagged by the substring test (first results) | Really rate-limited | Flagged but ordinary timeouts | Rate-limited but missed |
|---|---|---|---|---|
| pi | 7 | 3 | 4 | 0 |
| opencode | 7 | 5 | 2 | 7 |
| terminus-2 | 13 | 0 | 13 | 0 |
| claude-code | 27 | 2 | 25 | 0 |

- A really rate-limited trial carries `Rate limit reached for requests`, the message z.ai sends with code 1302.
- opencode prints that message with no code, so the substring test missed 7 of its rate-limited timeouts.
- An original attempt stands unless it really hit the limit (Amendment 4). So the 44 mis-flagged trials count as the graded timeouts they were.

## Retries of rate-limited trials

Each really rate-limited trial ran once more in its own job, with the same settings and 4 trials at once. A retry that hit the limit again stays excluded.

| Arm | Retried | Graded on retry | Rate limit again |
|---|---|---|---|
| pi | 3 | 3 | 0 |
| opencode | 12 | 11 | 1 |
| terminus-2 | 0 | 0 | 0 |
| claude-code | 2 | 2 | 0 |

- 10 retries ran under Amendment 12. The 7 opencode trials the substring test missed ran under Amendment 13.
- Amendment 12 also retried the 44 mis-flagged trials. Those 44 retries are discarded, because their originals stand.
- The set-aside attempts are in `aside-amendment-12/` in the run workspace, with each one's task, times and spend in `logs/amendment12-setaside.jsonl`.
- The first Amendment 12 claude-code launch was stopped during environment setup and run again detached. No agent had started in its 4 trials. Their empty folders are in `aside-amendment-12/claude-code-aborted-relaunch/`.

## Tokens and dollars

Prices are the manifest's list prices per 1M tokens: 0.15 input, 0.03 cache read, 0.50 output. A trial with no usage reads `unmeasured`, never zero (Amendment 2). These tables count the attempts in the record.

| Arm | Input | Cache read | Output | Dollars | Dollars per pass | Unmeasured trials |
|---|---|---|---|---|---|---|
| pi | 30,110,668 | 27,855,808 | 1,552,095 | 1.95 | 0.043 | 4 |
| opencode | 45,320,188 | 41,900,096 | 1,618,410 | 2.58 | 0.046 | 2 |
| terminus-2 | 31,062,108 | 28,792,896 | 2,644,808 | 2.53 | 0.053 | 0 |
| claude-code | 70,335,568 | 65,344,128 | 2,085,842 | 3.75 | 0.074 | 2 |
| total | | | | 10.81 | | |

The study spent 13.66 USD on Run 0 trials in all. 10.81 is in the record. 2.85 went on attempts outside it: rate-limited originals, and the discarded retries of mis-flagged trials.

## Paired by task

Each row compares two arms on the tasks both graded. The difference is the first arm's pass rate minus the second's, in points. The interval is a 95% bootstrap over tasks (4,000 resamples, seed 272).

| Arm vs arm | Tasks both graded | First only passed | Second only passed | Difference | 95% interval |
|---|---|---|---|---|---|
| claude-code vs terminus-2 | 89 | 14 | 11 | +3.4 | -7.9 to +14.6 |
| opencode vs terminus-2 | 88 | 15 | 7 | +9.1 | -1.1 to +19.3 |
| pi vs terminus-2 | 89 | 11 | 14 | -3.4 | -14.6 to +7.9 |
| opencode vs claude-code | 88 | 12 | 7 | +5.7 | -3.4 to +15.9 |
| pi vs claude-code | 89 | 7 | 13 | -6.7 | -16.9 to +2.2 |

## What the table can and cannot say

- Every arm is now graded on 88 or 89 of the same 89 tasks.
- claude-code's earlier lead came from the substring test. It excluded 25 claude-code timeouts as rate limits, and timeouts are failures. Graded, claude-code sits beside the other arms.
- No pair differs beyond noise. Every interval holds zero.
- claude-code timed out on 27 tasks, against 10 to 33 in the other arms. Its median trial ran 13.7 minutes.

## Exclusions and load

- Amendment 11 load rule: 8 opencode trials ran while the 5-minute load passed 16, from 18:00Z to 19:07Z on 2026-10-02. They were set aside and run again in the same job at 12:00Z to 12:44Z on 2026-10-03. The table counts the re-runs. The set-aside trials are in `aside-amendment-11/` in the run workspace.
- No trial in the record ran with the 5-minute load above 16. The highest 1-minute load during any graded trial was pi 14.7, opencode 22.7, terminus-2 19.8, claude-code 24.9.
- 18:29Z on 2026-10-02: unused task images were removed to free disk. Later arms pulled them again during environment setup, before the agent timer (Amendment 11).
- The appendix lists every timeout in the record with the highest 1-minute load during it (Amendment 5).

## Second results, after Amendment 12 (substring test)

These tables were published at commit 2a139fc2b8. They read any `1302` as a rate limit. `HARNESS_FIT_LEGACY_1302=1 run/report_run0.py retried` reproduces them.

| Arm | Trials | Graded | Passed | Rate | 95% interval | Excluded (1302) | Rate, 1302 scored | Dollars | Dollars per pass |
|---|---|---|---|---|---|---|---|---|---|
| pi | 89 | 85 | 46 | 54.1% | 43.6% to 64.3% | 4 | 51.7% | 1.98 | 0.043 |
| opencode | 89 | 85 | 56 | 65.9% | 55.3% to 75.1% | 4 | 62.9% | 2.53 | 0.045 |
| terminus-2 | 89 | 85 | 49 | 57.6% | 47.0% to 67.6% | 4 | 55.1% | 2.47 | 0.050 |
| claude-code | 89 | 64 | 51 | 79.7% | 68.3% to 87.7% | 25 | 59.6% | 3.80 | 0.075 |

| Arm vs arm | Tasks both graded | First only passed | Second only passed | Difference | 95% interval |
|---|---|---|---|---|---|
| claude-code vs terminus-2 | 63 | 12 | 4 | +12.7 | +1.6 to +23.8 |
| opencode vs terminus-2 | 83 | 14 | 7 | +8.4 | -2.4 to +19.3 |
| pi vs terminus-2 | 83 | 10 | 13 | -3.6 | -14.5 to +7.2 |
| opencode vs claude-code | 63 | 3 | 4 | -1.6 | -9.5 to +6.3 |
| pi vs claude-code | 64 | 2 | 11 | -14.1 | -25.0 to -3.1 |

## First results, before Amendment 12 (substring test)

These are the tables as first written (commit 71151e0a50). They read any `1302` as a rate limit. `HARNESS_FIT_LEGACY_1302=1 run/report_run0.py first` reproduces them.

| Arm | Trials | Graded | Passed | Rate | 95% interval | Excluded (1302) | Rate, 1302 scored | Dollars | Dollars per pass |
|---|---|---|---|---|---|---|---|---|---|
| pi | 89 | 82 | 45 | 54.9% | 44.1% to 65.2% | 7 | 51.7% | 1.96 | 0.044 |
| opencode | 89 | 82 | 53 | 64.6% | 53.8% to 74.1% | 7 | 60.7% | 2.58 | 0.049 |
| terminus-2 | 89 | 76 | 46 | 60.5% | 49.3% to 70.8% | 13 | 53.9% | 2.53 | 0.055 |
| claude-code | 89 | 62 | 49 | 79.0% | 67.4% to 87.3% | 27 | 57.3% | 3.77 | 0.077 |

| Arm vs arm | Tasks both graded | First only passed | Second only passed | Difference | 95% interval |
|---|---|---|---|---|---|
| claude-code vs terminus-2 | 59 | 10 | 4 | +10.2 | -1.7 to +22.0 |
| opencode vs terminus-2 | 72 | 11 | 6 | +6.9 | -4.2 to +18.1 |
| pi vs terminus-2 | 71 | 7 | 10 | -4.2 | -15.5 to +7.0 |
| opencode vs claude-code | 59 | 3 | 4 | -1.7 | -10.2 to +6.8 |
| pi vs claude-code | 60 | 2 | 10 | -13.3 | -25.0 to -3.3 |

## Pilot: the arm64 Mac (not comparable, fires no rule)

The first machine emulated every task image. Its claude-code rows ran on the 200,000-token window (Amendment 10). These rows are reported apart. Their exclusions used the substring test and were not recounted.

| Arm | Graded | Passed | Rate | Excluded (1302) |
|---|---|---|---|---|
| pi | 80 | 28 | 35.0% | 9 |
| claude-code | 16 | 12 | 75.0% | 13 |

The pilot's opencode and terminus-2 arms ran no trial.

## Appendix: every timeout, with its load

`1302` marks a timeout with z.ai's rate-limit error in the agent log. Those are excluded. The rest are graded failures. This is the record Amendment 13 defines.

| Arm | Task | 1302 | Highest 1-minute load |
|---|---|---|---|
| pi | caffe-cifar-10 | no | 12.3 |
| pi | cobol-modernization | no | 4.7 |
| pi | extract-moves-from-video | no | 13.5 |
| pi | gcode-to-text | no | 5.8 |
| pi | llm-inference-batching-scheduler | no | 9.9 |
| pi | make-doom-for-mips | no | 8.9 |
| pi | make-mips-interpreter | no | 7.9 |
| pi | mteb-leaderboard | no | 13.5 |
| pi | train-fasttext | no | 5.7 |
| pi | tune-mjcf | no | 13.5 |
| opencode | caffe-cifar-10 | no | 18.6 |
| opencode | crack-7z-hash | no | 9.6 |
| opencode | dna-assembly | no | 14.3 |
| opencode | extract-moves-from-video | yes | 18.6 |
| opencode | feal-linear-cryptanalysis | no | 19.8 |
| opencode | gpt2-codegolf | no | 11.7 |
| opencode | largest-eigenval | no | 7.5 |
| opencode | mailman | no | 18.6 |
| opencode | make-doom-for-mips | no | 11.5 |
| opencode | make-mips-interpreter | no | 11.5 |
| opencode | model-extraction-relu-logits | no | 6.4 |
| opencode | path-tracing | no | 14.3 |
| opencode | rstan-to-pystan | no | 18.6 |
| opencode | schemelike-metacircular-eval | no | 11.7 |
| opencode | torch-pipeline-parallelism | no | 6.0 |
| opencode | train-fasttext | no | 14.7 |
| opencode | tune-mjcf | no | 14.3 |
| terminus-2 | adaptive-rejection-sampler | no | 9.5 |
| terminus-2 | build-cython-ext | no | 5.1 |
| terminus-2 | build-pmars | no | 19.3 |
| terminus-2 | caffe-cifar-10 | no | 5.1 |
| terminus-2 | chess-best-move | no | 7.1 |
| terminus-2 | cobol-modernization | no | 6.0 |
| terminus-2 | dna-assembly | no | 6.6 |
| terminus-2 | extract-elf | no | 9.6 |
| terminus-2 | extract-moves-from-video | no | 15.1 |
| terminus-2 | feal-differential-cryptanalysis | no | 10.8 |
| terminus-2 | feal-linear-cryptanalysis | no | 12.2 |
| terminus-2 | filter-js-from-html | no | 6.8 |
| terminus-2 | gcode-to-text | no | 8.1 |
| terminus-2 | gpt2-codegolf | no | 11.2 |
| terminus-2 | largest-eigenval | no | 6.0 |
| terminus-2 | make-doom-for-mips | no | 7.7 |
| terminus-2 | make-mips-interpreter | no | 6.3 |
| terminus-2 | model-extraction-relu-logits | no | 6.1 |
| terminus-2 | overfull-hbox | no | 5.0 |
| terminus-2 | password-recovery | no | 11.1 |
| terminus-2 | path-tracing | no | 6.6 |
| terminus-2 | path-tracing-reverse | no | 9.0 |
| terminus-2 | protein-assembly | no | 6.6 |
| terminus-2 | query-optimize | no | 7.8 |
| terminus-2 | raman-fitting | no | 6.1 |
| terminus-2 | regex-chess | no | 7.3 |
| terminus-2 | reshard-c4-data | no | 10.3 |
| terminus-2 | sanitize-git-repo | no | 19.3 |
| terminus-2 | schemelike-metacircular-eval | no | 10.1 |
| terminus-2 | torch-pipeline-parallelism | no | 6.8 |
| terminus-2 | tune-mjcf | no | 19.3 |
| terminus-2 | winning-avg-corewars | no | 7.3 |
| terminus-2 | write-compressor | no | 7.2 |
| claude-code | adaptive-rejection-sampler | no | 8.5 |
| claude-code | caffe-cifar-10 | no | 10.9 |
| claude-code | cancel-async-tasks | no | 11.9 |
| claude-code | cobol-modernization | no | 23.9 |
| claude-code | compile-compcert | no | 14.1 |
| claude-code | dna-assembly | no | 14.1 |
| claude-code | extract-moves-from-video | no | 24.9 |
| claude-code | filter-js-from-html | no | 15.1 |
| claude-code | gcode-to-text | no | 13.3 |
| claude-code | gpt2-codegolf | no | 9.9 |
| claude-code | headless-terminal | no | 15.7 |
| claude-code | largest-eigenval | no | 11.8 |
| claude-code | make-doom-for-mips | no | 12.5 |
| claude-code | make-mips-interpreter | no | 15.3 |
| claude-code | model-extraction-relu-logits | no | 15.1 |
| claude-code | overfull-hbox | no | 12.1 |
| claude-code | path-tracing-reverse | no | 8.5 |
| claude-code | query-optimize | no | 13.2 |
| claude-code | raman-fitting | no | 13.5 |
| claude-code | regex-chess | no | 13.3 |
| claude-code | rstan-to-pystan | no | 15.7 |
| claude-code | schemelike-metacircular-eval | no | 23.9 |
| claude-code | torch-pipeline-parallelism | no | 23.9 |
| claude-code | train-fasttext | no | 23.9 |
| claude-code | tune-mjcf | no | 24.9 |
| claude-code | winning-avg-corewars | no | 15.7 |
| claude-code | write-compressor | no | 12.7 |
