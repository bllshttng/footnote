# Decision: harness fit for glm-5.3-flash

Rule 1 fires. opencode ties Claude Code on accepted changes and costs 37% less per accepted change. The recommendation is a GLM routing change toward opencode. It is a recommendation for the user. This study edits no production lane.

The rules apply in order (README, "Decision rules"). Rule 1 is checked first and fires, so rules 2 and 3 are not reached. Every number below is from the iMac rows in `run-0.md` and `run-1.md`. The arm64 pilot fires no rule.

## Rule 1: opencode or zcode against Claude Code

The rule: "If opencode or zcode beats Claude Code on accepted changes, recommend a GLM routing change. A tie counts only with 30% lower cost per accepted change."

zcode is `unavailable` in both runs, so only opencode is tested.

### Accepted changes: a tie

- Run 0 pairs the 59 tasks both arms graded. opencode minus Claude Code is -1.7 points, 95% interval -10.2 to +6.8. opencode alone passed 3 of them, Claude Code alone 4.
- Run 1, paired over the 10 tasks: opencode minus Claude Code is +3.3 points, 95% interval -10.0 to +20.0. opencode accepted 2 of 30, Claude Code 1 of 30.
- Both intervals hold zero, and both differences sit inside the 15-point noise band. Neither arm beats the other.

### Cost per accepted change: opencode is 37% lower

- Run 0, all spend over passes: opencode $2.58 for 53 passes is $0.0487 per pass. Claude Code $3.77 for 49 passes is $0.0769 per pass. opencode is 37% lower.
- Both arms have more than 20 graded trials (opencode 82, Claude Code 62). Each has 2 trials with no usage.
- Run 1 points the same way: $0.66 against $10.17 per accepted change. Stalls carry no usage in Run 1, so those figures are floors. They cannot carry the rule.

So the tie counts, and Rule 1 fires on Run 0's numbers.

## How firm the cost margin is

The 30% line holds on two of three ways to count spend. It fails on the third.

| Basis | opencode per pass | Claude Code per pass | opencode lower by |
|---|---|---|---|
| All spend, every trial (the preregistered metric) | $0.0487 | $0.0769 | 37% |
| The 59 tasks both arms graded | $0.0306 | $0.0503 | 39% |
| Graded trials only | $0.0404 | $0.0494 | 18% |

Most of the gap is spend on rate-limited trials. 27 Claude Code trials timed out on a 1302 rate limit and spent $1.35 between them. opencode's 7 such trials spent $0.44. Those dollars were really spent, so the preregistered metric counts them. The rule fires on that metric, but the margin rests on how each harness handles a rate limit.

## Caveats

- Claude Code lost 27 of 89 Run 0 trials to rate limits, against 7 for opencode. It ran last, from 05:36Z to 12:00Z on 2026-10-03, so its hours of the day differ from the other arms.
- The study ran 5 GLM sessions at once on one shared z.ai account. That was 4 Run 0 trials and 1 Run 1 attempt. Rate limits follow the account, not the arm.
- Run 1's bank is hard for both harnesses: 3 accepted changes in 60 scored attempts. Run 1 agrees with Run 0 but adds little power.
- zcode was never measured, and pi had no Run 1 lane. Rule 1 says nothing about either.
- Prices are list prices for glm-5.3-flash (manifest). One machine, one model, one pass of Terminal-Bench 2.

## Recommendation

Route GLM coding work through opencode lanes in `config.routing.models`, at the same model and effort. Claude Code on GLM solves about as many tasks, at higher cost per solved task. That is mostly because it keeps retrying rate-limited requests until the task times out. The user decides. No production config was changed.

A cheaper follow-up can test the margin without a new study: give Claude Code a shorter rate-limit retry budget, then re-run its Run 0 arm. If its cost per pass falls within 30% of opencode's, the routing case weakens.
