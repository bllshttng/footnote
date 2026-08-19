# Disposable deletes: why two `rm` spellings coexist

This repo uses bare `rm` in most scripts and `command -p rm ... || /bin/rm ...` in exactly two files. That split is deliberate. `scripts/ci/check-disposable-rm.sh` enforces it in CI: a bare `rm` in either guarded file fails the build, and a bare `rm` anywhere else passes. This document states the rule so the next contributor does not "normalize" one spelling into the other.

## The hazard

Wrapping `rm` to a trash tool is a common safety setup. On such a machine a bare `rm` MOVES the path to the trash instead of unlinking it. For almost every delete in this repo that is tolerable. The path leaves its old location either way, so the calling script's own logic is unaffected. The cost is disk consumption on that host, which is a property of the host's `rm` configuration.

## The criterion

Use the two-rung form where footnote deletes state it created itself AND that state is either:

- **(a) inside a concurrency-critical section.** A delete that silently becomes a move is non-atomic work inside a mutex.
- **(b) potentially large.** Trashing it is a material disk cost, not a rounding error.

Everything else keeps bare `rm`. Two files qualify.

### `scripts/ci/preflight.sh` (criterion a)

Every `rm` in this file sits in, or tears down, the mutex around the one shared preflight worktree. The lock protocol was rebuilt on `mv` because rename is one atomic operation. Its comments record a cross-run corruption race that `rm -rf` + `mkdir` caused. A trash wrapper puts non-atomic work back inside that section. When the trash sits on the same volume, a trash move is a rename. Across volumes it degrades to copy-then-delete. The mutex waits on that copy.

### `hooks/worktree-remove.sh` (criterion b)

A worktree with a built cargo `target/` runs to gigabytes, and git already holds every byte of it. Trashing one turns "reclaim 1.3 GB" into "relocate 1.3 GB and reclaim nothing." The guarded branch is the `[[ ! -e "$WORKTREE_PATH/.git" ]]` one: a leftover directory git itself declined to manage. The registered-worktree path above it is git's own removal and stays untouched.

## The sanctioned spellings

Probed live against a real PATH wrapper on a non-disposable path under `$HOME`:

| Spelling | Result under a PATH wrapper |
|---|---|
| `command -p rm -f` | removed, not trashed |
| `/bin/rm -f` | removed, not trashed |
| `command rm -f` | trashed |
| `rm -f` | trashed |

- `command -p rm` resolves via the default PATH, so a wrapper earlier in the user's PATH cannot intercept it.
- It is also the portable spelling: `/bin/rm` does not exist on NixOS, where `/bin` holds only `sh`.
- `/bin/rm` is the absolute fallback, for a shell whose default PATH is itself shadowed.
- `command rm` is NOT a fix: it bypasses functions and aliases only, not a PATH entry.

One measurement trap is worth recording. `command -pv rm` PRINTS the wrapper's path even though `command -p rm` EXECUTES `/bin/rm`. The lookup output and the execution disagree. Only a behavioral probe (delete a probe file, then check the trash) is trustworthy. This misread produced the wrong original diagnosis on the node that led to this rule. That node recorded `command -p rm -f` failing and leaving the file in place. The failure does not reproduce.

## Why the other ~339 sites stay bare

`rm` resolution is the host's business. The knowledge of which paths are disposable belongs in the user's own wrapper, where the deletion policy lives. One file there replaces hundreds of call sites this project cannot see. After a full sweep, footnote still pollutes the trash of any user whose editors or other tools call `rm`. The wrapper fixes all of it at once. footnote fixing its own 341 sites fixes only footnote. If a future site deletes state that meets (a) or (b), add its file to the gate allowlist in the same PR. State the criterion in the failure message.
