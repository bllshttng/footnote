# Fix

One verb repairs a broken state.
`/fno:fix` routes between fast repair and methodical diagnosis based on how much you already know.

## Two modes

| Mode | Command | Use when |
|------|---------|----------|
| fix (default) | `/fno:fix` | you know roughly what broke and want it green fast |
| investigate | `/fno:fix investigate` | the cause is unknown and you must find it first |

## fix: the fast loop

The default runs one fix per iteration and reverts on regression.
Give it a failing guard:

```
/fno:fix --guard "fno doctor test tests/test_auth.py" --category test
```

Each iteration changes code and runs the guard.
It keeps the change on improvement and reverts it on regression.
A change that breaks something else is reverted automatically.
When a test or build is red and the fix is close, this is the right mode.

## investigate: the hypothesis loop

When you cannot name the broken code yet, use investigate.
It runs the scientific method: state a hypothesis, write a failing reproduction, then confirm or reject it.

```
/fno:fix investigate
```

It is slower and deeper.
It produces a documented root cause before it changes anything.
Two cases call for it.
A fix loop that keeps reverting is one.
A symptom and a cause in different files is the other.

## How to pick

The rule is what you know about the cause.
Know it: `fix`.
Do not know it: `investigate`.
A `fix` that loops without converging is the signal to switch to `investigate`.

## Test first, always

Both modes lean on `fno doctor test`, not bare `pytest`. `fno doctor test` pins the worktree PYTHONPATH, bypasses the wrapper tee, and returns the real exit code. A bare `pytest` in a worktree can import the wrong package and report a false green.

```bash
fno doctor test tests/                    # run a path
fno doctor test tests/test_auth.py        # run one file
```

## Config problems, not code bugs

A run that breaks on a bad setting is not a code bug.
Read and fix the value directly:

```bash
fno config get config.auto_merge.enabled
fno config set config.auto_merge.enabled true
fno config unset config.auto_merge.enabled
fno config doctor                   # what resolved, and any suspicious values
```

## Related

- [Troubleshooting](../troubleshooting.md) covers the stuck-run cases this verb does not fix.
- [Getting started](../getting-started.md) covers install and first-run failures.
