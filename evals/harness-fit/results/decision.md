# Decision: harness fit for glm-5.3-flash

Rule 1 fires. opencode ties Claude Code on accepted changes and costs 39% less per accepted change. The recommendation is a GLM routing change toward opencode. It is a recommendation for the user. This study edits no production lane.

The rules apply in order (README, "Decision rules"). Rule 1 is checked first and fires, so rules 2 and 3 are not reached. Every number below is from the iMac rows in `run-0.md` and `run-1.md`, after Amendment 12 gave every rate-limited Run 0 trial one retry. The first results, before that retry, reached the same decision; they sit in their own section below. The arm64 pilot fires no rule.

## Rule 1: opencode or zcode against Claude Code

The rule: "If opencode or zcode beats Claude Code on accepted changes, recommend a GLM routing change. A tie counts only with 30% lower cost per accepted change."

zcode is `unavailable` in both runs, so only opencode is tested.

### Accepted changes: a tie

- Run 0 pairs the 63 tasks both arms graded. opencode minus Claude Code is -1.6 points, 95% interval -9.5 to +6.3. opencode alone passed 3 of them, Claude Code alone 4.
- Run 1, paired over the 10 tasks: opencode minus Claude Code is +3.3 points, 95% interval -10.0 to +20.0. opencode accepted 2 of 30, Claude Code 1 of 30.
- Both intervals hold zero, and both differences sit inside the 15-point noise band. Neither arm beats the other.

### Cost per accepted change: opencode is 39% lower

- Run 0, all spend in the final record over passes: opencode $2.53 for 56 passes is $0.0451 per pass. Claude Code $3.80 for 51 passes is $0.0745 per pass. opencode is 39% lower.
- Both arms have more than 20 graded trials (opencode 85, Claude Code 64). Each has 2 trials with no usage.
- Run 1 points the same way: $0.66 against $10.17 per accepted change. Stalls carry no usage in Run 1, so those figures are floors. They cannot carry the rule.

So the tie counts, and Rule 1 fires on Run 0's numbers.

## How firm the cost margin is

The 30% line holds on three of four ways to count spend. It fails on the fourth.

| Basis | opencode per pass | Claude Code per pass | opencode lower by |
|---|---|---|---|
| All spend in the final record (the preregistered metric, Amendment 12) | $0.0451 | $0.0745 | 39% |
| All spend, with the attempts Amendment 12 set aside | $0.0529 | $0.1010 | 48% |
| The 63 tasks both arms graded | $0.0301 | $0.0492 | 39% |
| Graded trials only | $0.0395 | $0.0493 | 20% |

The gap comes from spend on rate-limited trials. Claude Code's 25 trials that hit 1302 on their retry spent money and graded nothing; opencode had 4. Counting graded trials only, opencode is 20% cheaper, under the 30% line. So the margin rests on how each harness handles a rate limit, and the retry made that clearer, not weaker: 25 of Claude Code's 27 rate-limited tasks hit the limit again, against 4 of 7 for opencode.

## Caveats

- Claude Code still has 25 of 89 Run 0 trials excluded for rate limits after the retry, against 4 in every other arm. Those trials are not a random draw.
- Claude Code's original arm ran last, from 05:36Z to 12:00Z on 2026-10-03, so its hours of the day differ from the other arms. The Amendment 12 retries ran for every arm on 2026-10-04, one arm at a time.
- The study ran 5 GLM sessions at once on one shared z.ai account during the original arms: 4 Run 0 trials and 1 Run 1 attempt. The retries ran 4 at once with Run 1 finished. Rate limits follow the account, not the arm.
- The arm64 pilot found that Claude Code retries a rate-limited request until the task times out, where pi stops. The repeated 1302 timeouts here fit that, but this study did not measure the retry behaviour directly.
- Run 1's bank is hard for both harnesses: 3 accepted changes in 60 scored attempts. Run 1 agrees with Run 0 but adds little power.
- zcode was never measured, and pi had no Run 1 lane. Rule 1 says nothing about either.
- Prices are list prices for glm-5.3-flash (manifest). One machine, one model, one pass of Terminal-Bench 2 plus one retry of its rate-limited trials.

## First results, before Amendment 12

The first decision (commit 71151e0a50) also fired Rule 1. On 59 shared tasks opencode minus Claude Code was -1.7 points, 95% interval -10.2 to +6.8. The cost margin was 37% on all spend ($0.0487 against $0.0769 per pass), 39% on shared tasks and 18% on graded trials only. The retry moved each arm's rate by at most 3 points and the opencode against Claude Code comparison by under 1 point. It changed no conclusion.

## Recommendation

Route GLM coding work through opencode lanes in `config.routing.models`, at the same model and effort. Claude Code on GLM solves about as many tasks, at higher cost per solved task, and loses about a quarter of its Terminal-Bench trials to rate limits on this account. The user decides. No production config was changed.

A cheaper follow-up can test the margin without a new study: give Claude Code a shorter rate-limit retry budget, then re-run its Run 0 arm. If its cost per pass falls within 30% of opencode's, the routing case weakens.
