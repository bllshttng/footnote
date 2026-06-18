# Kill Criteria Reference

Abort conditions declared on the plan that target, operator, and do evaluate
at iteration or wave boundaries. When a predicate fires, the engine emits
`<aborted reason="{name}">`, the stop hook treats it symmetrically to
`<promise>` (clean exit, state archive, ledger entry), and the session
terminates without a retry loop.

## Why This Exists

Circuit breakers and checkpoint rollback both assume the plan is recoverable:
rotate approaches, roll back to a known-good state, try again. Neither helps
when the signal is "the plan itself is wrong". Kill criteria are the terminal
guard for that case - they short-circuit burn loops that would otherwise
consume iterations chasing a goal the current plan cannot reach.

Typical firing conditions:

- Iteration counter blows past the ceiling (planning was too optimistic).
- The same test has been failing for N iterations (root cause unclear).
- The session has drifted outside the declared file surface (scope creep).
- A test file was deleted (a common "fix" that destroys signal).

Each of these is a sign the plan is wrong, not that the last attempt was
wrong. Kill criteria name them explicitly so the stop hook can exit clean.

## Data Flow

```
plan frontmatter (full) OR ## Kill Criteria fenced YAML (quick)
   │
   ▼
scripts/lib/kill-criteria.sh :: check_kill_criteria <plan_path>
   │   reads kill_criteria block
   │   reads target-state.md (iteration, verification.consecutive_failures)
   │   reads git working tree (for files_outside, any_test_file_deleted)
   │
   ▼ exit 1 + "KILL_CRITERIA_FIRED <name>|<reason>" on stdout
engine boundary (target: §3f, operator: §2, do: §2)
   │
   ▼ user-facing turn
<aborted reason="{name}">MISSION ABORTED: {reason}</aborted>
   │
   ▼
hooks/target-stop-hook.sh (grep for <aborted>, parse reason)
   │   sets status: ABORTED in target-state.md frontmatter
   │   sets abort_reason: {reason}
   │   archives state to .fno/.aborted-archive/target-state.{ts}.md
   │   exports TARGET_ABORT_REASON
   │   calls run_completion_accounting
   │
   ▼
scripts/metrics/register-task.py
   │   writes ledger.json entry with status: aborted, reason: {reason}
   │
   ▼
emit_approve → clean exit (no retry)
```

Aborted detection runs before promise detection in the stop hook, so a
turn that happens to contain both tags resolves to abort. This is the
safer default: once a kill criterion fires, completing anyway would
overwrite the signal.

## Predicate Vocabulary

Every predicate is a string compared against the engine's known patterns.
Unknown predicates log a WARN and are skipped so a stale validator never
aborts the pipeline.

| Predicate | Example | Fires when |
|-----------|---------|-----------|
| `iteration > N`, `iteration >= N` | `iteration > 15` | Target iteration counter exceeds N |
| `same_test_failing_for > N`, `same_test_failing_for >= N` | `same_test_failing_for >= 3` | `verification.consecutive_failures` in target-state.md hits N |
| `files_outside(plan_path) > N` | `files_outside(plan_path) > 5` | git diff touches more than N files outside the plan folder |
| `any_test_file_deleted` | `any_test_file_deleted` | `git status --porcelain` shows a deleted file matching test path patterns |

The patterns for `any_test_file_deleted` match `__tests__/`, `tests/`,
`spec/`, `*.test.{ts,tsx,js,jsx,py,sh}`, `*.spec.{…}`, `test_*.{py,sh}`,
and `*_test.{py,go}`. See `_kc_eval_test_file_deleted` in
`scripts/lib/kill-criteria.sh` for the exact regex.

## Extending the Vocabulary

Adding a new predicate means three edits, all in-place:

1. Add an evaluator in `scripts/lib/kill-criteria.sh`:

   ```bash
   _kc_eval_my_thing() {
       # Returns 0 if the predicate fires, 1 if not, 2 if malformed.
       local pred="$1"
       [[ "$pred" =~ ^my_thing[[:space:]]*\>[[:space:]]*([0-9]+)[[:space:]]*$ ]] || return 2
       local rhs="${BASH_REMATCH[1]}"
       # ...compute the signal, return 0 if over rhs, else 1
   }
   ```

2. Route it in `_kc_dispatch_predicate` in the same file:

   ```bash
   case "$pred" in
       my_thing[[:space:]]*)  _kc_eval_my_thing "$pred" ;;
       # ...existing cases
   esac
   ```

3. Teach the validator to recognize it by updating `KNOWN_PREDICATES_RE`
   in `skills/blueprint/scripts/validate-plan.sh`. Plans that use the new
   predicate will pass validation without a WARN.

Unknown predicates are a warning, not an error, by design: plans that
reference a predicate added after the validator was last updated will
still run, with a WARN logged once per iteration.

## Backward Compatibility

A plan without a `kill_criteria:` block behaves identically to pre-spec
engines. `check_kill_criteria` returns exit 0 when the block is absent,
and the engine proceeds to its normal iteration. This is the contract
called out as AC4-EDGE in the originating plan: omission is legal, and
no engine-side default is injected.

The stop hook's `<aborted>` branch is only reachable when the engine
emits the tag, which only happens when the evaluator returns non-zero,
which only happens when a predicate fires. Plans that never declared
kill_criteria cannot produce the abort path.

## Relationship to Existing Mechanisms

Kill criteria complement the two existing stop-conditions; they do not
replace them.

| Mechanism | Scope | Action on trigger |
|-----------|-------|-------------------|
| Circuit breaker (`circuit_breaker.*`) | same error signature repeats | rotate approach, keep iterating |
| Checkpoint rollback (`verification.*`) | consecutive validation failures | stash/reset to checkpoint, retry |
| Kill criteria (`kill_criteria:`) | plan-declared "plan is wrong" signal | terminate session clean |

Ordering when more than one could trigger in the same iteration:

1. Kill criteria checked first (at boundary, before execute).
2. Circuit breaker handles same-error repetition during execute.
3. Checkpoint rollback triggers on failed verification after execute.

If kill criteria fires, the engine never reaches the circuit breaker or
checkpoint logic for that iteration - the session exits.

## State Artifacts

| Path | Written by | Purpose |
|------|-----------|---------|
| `.fno/target-state.md` (`status: ABORTED`, `abort_reason: {reason}`) | stop hook | Terminal state visible to next session / target resume |
| `.fno/.aborted-archive/target-state.{ts}.md` | stop hook | Audit trail preserved across sessions |
| `.fno/ledger.json` (`status: aborted`, `reason: {reason}`) | register-task.py via run_completion_accounting | Queryable history |
| Event stream `stop-hook / aborted` | emit_event | JSON `{reason, iteration}` payload |

See `skills/target/references/state-schema.md` for the full status model.
