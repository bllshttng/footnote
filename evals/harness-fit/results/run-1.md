# Run 1 results: the replay bank

This is the iMac run, the one that decides (Amendments 6, 7 and 10). The arm64 pilot sits in its own section below and fires no rule.

## Setup

- 10 replay tasks from this repo's own merged PRs (Amendment 3), 3 attempts per task per lane, a 45-minute budget per attempt, one attempt at a time.
- Lanes run through `fno doctor evals run --lane --cohort --repeat 3` with this branch's CLI and fno-agents. The claude lane asks for `glm-5.3-flash[1m]` with an 800,000-token compact window (Amendment 10). Host harness versions: Claude Code 2.1.286, opencode 1.18.33.
- The scores come from `evals/harness-fit/run/score_run1.py` over the eval history. It applies the preregistered stall, rate-limit, identity and exclusion rules.
- pi and zcode are `unavailable` on every attempt. The headless substrate has no pi lane on this branch, and the iMac has no ZCode.app.

## Per lane

An accepted change means the task's hidden tests pass. A stall is a worker that ran out its 45-minute budget: it is scored, with no accepted change (Amendment 4). The interval is a 95% Wilson interval.

| Lane | Attempts | Scored | Accepted | Rate | 95% interval | Stalls | Rate without stalls | Rate, rate limits scored | Excluded by reason | Median wall minutes |
|---|---|---|---|---|---|---|---|---|---|---|
| claude | 31 | 30 | 1 | 3.3% | 0.6% to 16.7% | 6 | 4.2% | 3.3% | unavailable 1 | 15.7 |
| opencode | 33 | 30 | 2 | 6.7% | 1.8% to 21.3% | 12 | 11.1% | 6.1% | rate-limit 3 | 6.4 |
| pi | 30 | 0 | 0 | unavailable | | | | | unavailable 30 | |
| zcode | 30 | 0 | 0 | unavailable | | | | | unavailable 30 | |

## Tokens and dollars

Prices are the manifest's list prices. Usage comes from each attempt's own transcript. An attempt with no readable usage reads `unmeasured` and adds nothing, so a lane's dollars are a floor.

| Lane | Input | Cache read | Cache write | Output | Dollars (floor) | Unmeasured attempts | Dollars per accepted change (floor) |
|---|---|---|---|---|---|---|---|
| claude | 11,974,039 | 247,327,872 | 0 | 1,911,844 | 10.17 | 6 of 30 scored | 10.17 |
| opencode | 1,448,066 | 32,437,184 | 0 | 267,450 | 1.32 | 12 of 30 scored | 0.66 |

12 of opencode's 30 scored attempts and 6 of claude's 30 recorded no usage. In each lane those are exactly the stalls. Run 1's dollar figures cannot rank the two lanes on cost.

## Per task

Accepted over scored attempts.

| Task | claude | opencode |
|---|---|---|
| replay-x-1018 | 0/3 | 0/3 |
| replay-x-5978 | 0/3 | 0/3 |
| replay-x-5baf | 0/3 | 0/3 |
| replay-x-70e1 | 0/3 | 0/3 |
| replay-x-71e9 | 0/3 | 0/3 |
| replay-x-861c | 0/3 | 0/3 |
| replay-x-a23c | 0/3 | 2/3 |
| replay-x-a8f3 | 0/3 | 0/3 |
| replay-x-c911 | 1/3 | 0/3 |
| replay-x-f188 | 0/3 | 0/3 |

Paired over the 10 tasks, opencode's accepted rate minus claude's is +3.3 points, 95% bootstrap interval -10.0 to +20.0 (4,000 resamples, seed 272).

## What the table can and cannot say

- The bank is hard for both lanes. claude accepted 1 change in 30 scored attempts and opencode 2 in 30. The two intervals overlap almost entirely.
- The difference is inside the noise, so Run 1 alone fires no rule. It agrees with Run 0, where the two harnesses tie on shared tasks.
- opencode stalled twice as often as claude: 12 against 6. Its median attempt was still shorter: 6.4 minutes against 15.7.
- A stalled attempt reads no usage, so stalls hide cost. The cost columns understate opencode more than claude.

## Exclusions and set-asides

- Amendment 11 load rule: two opencode attempts on replay-x-861c ran while the 5-minute load passed 16. They were set aside and the top-up ran them again. No row in the table ran above that line.
- One claude attempt never started: its worker exited 143 before any work. It reads `unavailable` and the top-up refilled its slot.
- Set aside earlier: 29 never-started claude attempts from the 11:51Z installer incident, and one claude attempt on the 200,000-token window (Amendments 10 and 11). The rows are in the run workspace under `logs/aborted-start-4` and `logs/pre-amendment-10`.

## Pilot: the arm64 Mac (not comparable, fires no rule)

The pilot's claude lane ran on the 200,000-token window, and many of its claude attempts were refused by the provider cap.

| Lane | Attempts | Scored | Accepted | Stalls | Excluded by reason |
|---|---|---|---|---|---|
| claude | 30 | 6 | 0 | 0 | unavailable 24 |
| opencode | 31 | 22 | 2 | 11 | rate-limit 3, unavailable 6 |
| pi | 30 | 0 | 0 | 0 | unavailable 30 |
