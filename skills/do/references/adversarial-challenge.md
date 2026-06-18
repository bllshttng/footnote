# Adversarial Challenge

Post-verification phase that challenges the implementation from three
adversarial angles. Opt-in only via `--adversarial` flag because it
costs extra tokens.

## Activation

Only when `--adversarial` flag is present in operator arguments.
This costs extra tokens and should be an explicit opt-in.

## Dispatch

- **Agent:** archer
- **Tools:** `["Read", "Grep", "Glob", "Bash"]`

### Prompt Template

```
You are an adversarial reviewer. Your job is to BREAK the implementation,
not praise it. Be thorough and skeptical.

Plan: {path to 00-INDEX.md}
Changes: run `git diff {base_commit}..HEAD` to see the actual code

Challenge from three angles:

## 1. The Pessimist (What breaks?)
- What happens when the network is down?
- What happens when the database returns unexpected data?
- What happens when two users do this simultaneously?
- What happens when input is empty, null, or malformed?

## 2. The Attacker (What can be exploited?)
- Can this be abused by malicious input?
- Are there injection vectors (SQL, XSS, command)?
- Are there authorization bypasses?
- Can rate limiting be circumvented?

## 3. The Auditor (Does this actually satisfy the AC?)
- Read each acceptance criterion literally
- Does the implementation satisfy the LETTER, not just the spirit?
- Are there edge cases in the AC that aren't handled?
- Are there implicit requirements the AC assumes but doesn't state?

Report format:
## Adversarial Challenge Report
### Critical (must fix)
- [finding]: [severity] - [what to fix]
### Warning (should fix)
- [finding]: [severity] - [suggestion]
### Observation (informational)
- [finding]: [context]
### Verdict: PASS | FAIL (critical findings = FAIL)
```

## Fix Loop

If verdict is FAIL (critical findings exist):

1. Dispatch a fix worker for each critical finding (one at a time)
2. After fixes, re-run the adversarial challenge
3. Maximum 3 fix iterations
4. If still failing after 3 iterations: report to user with remaining findings

If verdict is PASS: report completion with any warnings noted.

## Coordinator State

Update `coordinator_phase` in target-state.md:
- Set to `adversarial` before dispatching the adversarial worker
- Set to `complete` when challenge finishes (pass or max iterations)

On resume: if `coordinator_phase: adversarial`, re-run the adversarial
challenge (it reads current git state).
