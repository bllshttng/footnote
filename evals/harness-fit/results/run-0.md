# Run 0 results: Terminal-Bench 2 through Harbor

This is the iMac run, the one that decides (Amendments 6, 7 and 10). The tables count the record after Amendment 12, which gave every rate-limited trial one retry. The first results, before that retry, sit in their own section below. The arm64 pilot sits in its own section and fires no rule.

The numbers come from `run/report_run0.py`. Its `first` mode reproduces the first results exactly, and its `final` mode prints the tables here.

## Setup

- Machine: Intel iMac, 8 physical cores, 128 GiB, native x86_64. Task images are `linux/amd64`, so nothing was emulated.
- Harbor 0.23.0. One job at a time, 4 trials at once (Amendment 6). Each task keeps its own agent timeout at multiplier 1.0 (Amendment 1).
- Model: `glm-5.3-flash` through z.ai at effort high. The claude-code arm asks for `glm-5.3-flash[1m]` (Amendment 10).
- Harness versions inside the task containers: Claude Code 2.1.286, opencode 1.18.34, pi 1.0.0. Two opencode trials and two pi trials recorded no version.
- Arm order ran: pi, opencode, terminus-2, then claude-code. The seeded order put claude-code second. Its first smoke read as a false negative and the arm ran last (Amendment 10).
- Amendment 12 retries ran on 2026-10-04 from 04:58Z to 12:31Z. They ran one arm at a time, in the seeded order: pi, claude-code, opencode, terminus-2.
- zcode is `unavailable`: the iMac has no ZCode.app, and the zcode adapter stays out of this repo (Amendment 6).

## Per arm

Rate is passed over graded. The interval is a 95% Wilson interval. A trial that timed out with a 1302 rate-limit error in its agent log is an infrastructure exclusion (Amendment 2). The last rate column scores those trials as failures, as a check on that rule.

| Arm | Trials | Graded | Passed | Rate | 95% interval | Excluded (1302) | Rate, 1302 scored | Other errors among graded | Compute hours | Median trial minutes |
|---|---|---|---|---|---|---|---|---|---|---|
| pi | 89 | 85 | 46 | 54.1% | 43.6% to 64.3% | 4 | 51.7% | AgentTimeoutError 5, NonZeroAgentExitCodeError 3 | 16.2 | 7.5 |
| opencode | 89 | 85 | 56 | 65.9% | 55.3% to 75.1% | 4 | 62.9% | AgentTimeoutError 12, ApiRateLimitError 1, NonZeroAgentExitCodeError 2 | 21.5 | 10.2 |
| terminus-2 | 89 | 85 | 49 | 57.6% | 47.0% to 67.6% | 4 | 55.1% | AgentTimeoutError 26 | 25.6 | 15.4 |
| claude-code | 89 | 64 | 51 | 79.7% | 68.3% to 87.7% | 25 | 59.6% | NonZeroAgentExitCodeError 2 | 24.8 | 13.7 |
| zcode | 0 | 0 | 0 | unavailable | | | | no ZCode.app | | |

## Amendment 12 retries

Each trial excluded under the 1302 rule in the first results ran once more in its own job. A retry that hit 1302 again stays excluded.

| Arm | Retried | Graded on retry | 1302 again | Highest 1-minute load during a retry | Set-aside spend (USD) |
|---|---|---|---|---|---|
| pi | 7 | 3 | 4 | 12.3 | 0.29 |
| claude-code | 27 | 2 | 25 | 15.1 | 1.35 |
| opencode | 7 | 3 | 4 | 14.7 | 0.44 |
| terminus-2 | 13 | 9 | 4 | 10.0 | 0.57 |

- 25 of claude-code's 27 retries hit 1302 again, against 4 in each other arm. The rate limit follows those tasks on this harness, not the hour.
- The set-aside attempts are in `aside-amendment-12/` in the run workspace, with each one's task, times and spend in `logs/amendment12-setaside.jsonl`.
- The first claude-code launch was stopped during environment setup and run again detached. No agent had started in its 4 trials. Their empty folders are in `aside-amendment-12/claude-code-aborted-relaunch/`.

## Tokens and dollars

Prices are the manifest's list prices per 1M tokens: 0.15 input, 0.03 cache read, 0.50 output. A trial with no usage reads `unmeasured`, never zero (Amendment 2). These tables count the attempts in the final record (Amendment 12). The set-aside spend is in the table above.

| Arm | Input | Cache read | Output | Dollars | Dollars per pass | Unmeasured trials |
|---|---|---|---|---|---|---|
| pi | 31,016,039 | 28,761,984 | 1,558,628 | 1.98 | 0.043 | 4 |
| opencode | 44,058,417 | 40,547,520 | 1,564,059 | 2.53 | 0.045 | 2 |
| terminus-2 | 30,489,747 | 28,244,096 | 2,572,860 | 2.47 | 0.050 | 0 |
| claude-code | 72,254,098 | 67,397,632 | 2,099,009 | 3.80 | 0.075 | 2 |
| total | | | | 10.78 | | |

The study spent 13.43 USD on Run 0 trials in all: 10.78 in the final record and 2.65 on the attempts Amendment 12 set aside.

## Paired by task

Each row compares two arms on the tasks both graded. The difference is the first arm's pass rate minus the second's, in points. The interval is a 95% bootstrap over tasks (4,000 resamples, seed 272).

| Arm vs arm | Tasks both graded | First only passed | Second only passed | Difference | 95% interval |
|---|---|---|---|---|---|
| claude-code vs terminus-2 | 63 | 12 | 4 | +12.7 | +1.6 to +23.8 |
| opencode vs terminus-2 | 83 | 14 | 7 | +8.4 | -2.4 to +19.3 |
| pi vs terminus-2 | 83 | 10 | 13 | -3.6 | -14.5 to +7.2 |
| opencode vs claude-code | 63 | 3 | 4 | -1.6 | -9.5 to +6.3 |
| pi vs claude-code | 64 | 2 | 11 | -14.1 | -25.0 to -3.1 |

## What the table can and cannot say

- The retry evened the other three arms at 85 graded trials each. claude-code still has 25 exclusions, because nearly every rate-limited task hit the limit again.
- The arm64 pilot found that Claude Code retries a 1302 until the task timeout, where pi stops. That fits the retries here: claude-code's excluded tasks timed out on 1302 twice. The excluded trials are not a random draw, so claude-code's raw rate is the least comparable.
- On the tasks both graded, claude-code and opencode differ by under 2 points, inside the noise. On shared tasks claude-code beats pi by about 14 points and Terminus 2 by about 13. Both intervals exclude zero.
- With 1302 trials scored as failures, claude-code falls to 59.6% and sits beside the other arms.

## Exclusions and load

- Amendment 11 load rule: 8 opencode trials ran while the 5-minute load passed 16, from 18:00Z to 19:07Z on 2026-10-02. They were set aside and run again in the same job at 12:00Z to 12:44Z on 2026-10-03. The table counts the re-runs. The set-aside trials are in `aside-amendment-11/` in the run workspace.
- No graded trial and no Amendment 12 retry ran with the 5-minute load above 16. The highest 1-minute load during any graded trial was pi 14.7, opencode 22.7, terminus-2 19.8, claude-code 24.9.
- 18:29Z on 2026-10-02: unused task images were removed to free disk. Later arms pulled them again during environment setup, before the agent timer (Amendment 11).
- The appendix lists every timeout in the final record with the highest 1-minute load during it (Amendment 5).

## First results, before Amendment 12

These are the tables as first written (commit 71151e0a50). `run/report_run0.py first` reproduces them.

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

The first machine emulated every task image. Its claude-code rows ran on the 200,000-token window (Amendment 10). These rows are reported apart.

| Arm | Graded | Passed | Rate | Excluded (1302) |
|---|---|---|---|---|
| pi | 80 | 28 | 35.0% | 9 |
| claude-code | 16 | 12 | 75.0% | 13 |

The pilot's opencode and terminus-2 arms ran no trial.

## Appendix: every timeout, with its load

`1302` marks a timeout with a rate-limit error in the agent log. Those are excluded. The rest are graded failures. This is the final record. A task retried under Amendment 12 shows its retry.

| Arm | Task | 1302 | Highest 1-minute load |
|---|---|---|---|
| pi | caffe-cifar-10 | no | 12.3 |
| pi | cobol-modernization | no | 4.7 |
| pi | extract-moves-from-video | no | 13.5 |
| pi | gcode-to-text | yes | 12.3 |
| pi | llm-inference-batching-scheduler | no | 9.9 |
| pi | make-doom-for-mips | yes | 8.9 |
| pi | make-mips-interpreter | yes | 12.3 |
| pi | train-fasttext | yes | 10.7 |
| pi | tune-mjcf | no | 13.5 |
| opencode | caffe-cifar-10 | no | 14.3 |
| opencode | dna-assembly | no | 14.3 |
| opencode | extract-moves-from-video | no | 7.5 |
| opencode | feal-linear-cryptanalysis | no | 19.8 |
| opencode | gpt2-codegolf | no | 5.5 |
| opencode | largest-eigenval | no | 7.5 |
| opencode | mailman | no | 8.7 |
| opencode | make-doom-for-mips | yes | 11.5 |
| opencode | make-mips-interpreter | yes | 11.5 |
| opencode | model-extraction-relu-logits | no | 6.4 |
| opencode | path-tracing | yes | 14.7 |
| opencode | rstan-to-pystan | no | 20.7 |
| opencode | schemelike-metacircular-eval | no | 6.0 |
| opencode | torch-pipeline-parallelism | no | 6.0 |
| opencode | train-fasttext | yes | 14.7 |
| opencode | tune-mjcf | no | 7.5 |
| terminus-2 | adaptive-rejection-sampler | no | 9.5 |
| terminus-2 | build-cython-ext | no | 5.1 |
| terminus-2 | build-pmars | no | 19.3 |
| terminus-2 | caffe-cifar-10 | no | 5.1 |
| terminus-2 | chess-best-move | no | 8.5 |
| terminus-2 | cobol-modernization | no | 6.0 |
| terminus-2 | dna-assembly | no | 8.5 |
| terminus-2 | extract-elf | no | 9.6 |
| terminus-2 | extract-moves-from-video | no | 9.2 |
| terminus-2 | feal-differential-cryptanalysis | no | 10.8 |
| terminus-2 | feal-linear-cryptanalysis | no | 12.2 |
| terminus-2 | filter-js-from-html | no | 6.8 |
| terminus-2 | gcode-to-text | no | 8.1 |
| terminus-2 | gpt2-codegolf | no | 11.2 |
| terminus-2 | make-doom-for-mips | yes | 8.0 |
| terminus-2 | make-mips-interpreter | yes | 7.5 |
| terminus-2 | model-extraction-relu-logits | no | 6.1 |
| terminus-2 | password-recovery | no | 11.1 |
| terminus-2 | path-tracing-reverse | no | 10.0 |
| terminus-2 | path-tracing | no | 7.5 |
| terminus-2 | protein-assembly | yes | 7.5 |
| terminus-2 | query-optimize | no | 9.2 |
| terminus-2 | raman-fitting | yes | 7.2 |
| terminus-2 | regex-chess | no | 7.3 |
| terminus-2 | reshard-c4-data | no | 10.3 |
| terminus-2 | sanitize-git-repo | no | 19.3 |
| terminus-2 | schemelike-metacircular-eval | no | 10.1 |
| terminus-2 | torch-pipeline-parallelism | no | 6.8 |
| terminus-2 | tune-mjcf | no | 19.3 |
| terminus-2 | write-compressor | no | 7.2 |
| claude-code | adaptive-rejection-sampler | yes | 8.5 |
| claude-code | caffe-cifar-10 | yes | 8.4 |
| claude-code | cobol-modernization | yes | 9.0 |
| claude-code | dna-assembly | yes | 11.3 |
| claude-code | extract-moves-from-video | yes | 11.9 |
| claude-code | filter-js-from-html | yes | 10.2 |
| claude-code | gcode-to-text | yes | 8.3 |
| claude-code | gpt2-codegolf | yes | 8.5 |
| claude-code | headless-terminal | yes | 9.4 |
| claude-code | largest-eigenval | yes | 10.1 |
| claude-code | make-doom-for-mips | yes | 10.2 |
| claude-code | make-mips-interpreter | yes | 11.3 |
| claude-code | model-extraction-relu-logits | yes | 10.1 |
| claude-code | overfull-hbox | yes | 11.3 |
| claude-code | path-tracing-reverse | yes | 8.5 |
| claude-code | query-optimize | yes | 10.2 |
| claude-code | raman-fitting | yes | 10.1 |
| claude-code | regex-chess | yes | 15.1 |
| claude-code | rstan-to-pystan | yes | 15.1 |
| claude-code | schemelike-metacircular-eval | yes | 10.1 |
| claude-code | torch-pipeline-parallelism | yes | 8.8 |
| claude-code | train-fasttext | yes | 10.2 |
| claude-code | tune-mjcf | yes | 10.7 |
| claude-code | winning-avg-corewars | yes | 15.1 |
| claude-code | write-compressor | yes | 15.1 |
