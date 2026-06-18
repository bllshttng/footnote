# `fno pr` internalization

The `fno pr {merge,verify,rebase}` verbs used to be thin Typer wrappers that
resolved `resolve_repo_root() / "scripts/lib/pr-*.sh"` and `subprocess.run`'d
the bash. That worked in a clone but degraded on a bare `pip install fno` (the
scripts are not on disk). This change ports the four scripts to in-package
Python that shells to `gh` / `git`, so the verbs run from the installed wheel
with no repo-root dependency.

This is a **port, not a rewrite**: the logic moved to Python, shells to the same
`gh`/`git` the bash used, and is pinned by characterization tests. The merge
guards and the rebase delegation handshake are safety-critical and must not
drift.

## Layout

```
cli/src/fno/pr/cli.py       Typer verbs; dispatch in-package (no repo-root script)
cli/src/fno/pr/_proc.py     run() + ToolMissing: capture gh/git output, flag a missing binary
cli/src/fno/pr/_merge.py    <- pr-merge.sh        (JSON line + merge guards + worktree recovery)
cli/src/fno/pr/_verify.py   <- verify-pr-merged.sh + verify-review-replies.sh
cli/src/fno/pr/_rebase.py   <- rebase-resolve.sh  (exit-code protocol incl. 42)
```

The auto_merge config is read through the new typed `config.auto_merge`
(`AutoMergeBlock`) instead of re-parsing `settings.yaml` in a subprocess, so the
4-tier precedence + caching live in one place. Validation mirrors the old
`config.sh` helpers exactly (invalid `merge_strategy` -> `merge`,
`conflict_resolution` -> `opus`, `remediation` -> `attempt`).

## Preserved contracts

| Verb | Contract held verbatim |
|---|---|
| `pr merge` | JSON line `{pr, outcome, reason, strategy, invoker}`; exit 0 merged\|queued, 1 failed, 2 skipped, 127 gh-missing; the canonical merge guards (`config.auto_merge` gating + invoker allowlist); the worktree server-side-recovery fallback (a worktree-local post-merge step can error after the server-side merge already landed; the fallback consults GitHub before classifying so a merged PR is never misreported as failed); post-merge sentinels (`.memory-pass-pending`, `.triage-pending`), the `session_satisfied{source:pr_merge}` event, and the per-PR artifact consolidation, all best-effort. |
| `pr verify --kind merged` | merge-state audit; the single bounded remediation (one auto-merge attempt + one 30s poll, never a retry loop); the `record_merge` frontmatter write (atomic `os.replace`, idempotent `merged_prs`); the audit events. Exit 0/1/2. |
| `pr verify --kind reviews` | the qualifying-reply gate-flip that closes the external-review forgery hole (PR-author reply strictly after the reviewer's latest review, @-mention OR within 24h); one `transcript_audit_failed` event per missing reviewer. Exit 0/1/2. |
| `pr rebase` | two-phase exit-code protocol 0 clean / 1 failed\|refused\|fetch_failed / 2 dirty / 3 protected / 42 needs_resolver; guardrails (migration/secret/lock/gitconfig/mass-conflict); phase-B `--continue` resume from git's native in-progress rebase state. |

## The one inherent limit

`pr rebase` runs its git mechanics pip-only, but the **exit-42 -> needs_resolver
agent delegation stays a caller (skill) responsibility**: the conflict-resolver
agent is invoked via the Task tool by the orchestrating skill, which a bare
`pip install` (no skills) does not have. So pip-only `pr rebase` works through
clean / dirty / refused / needs_resolver and emits the needs_resolver signal for
a caller to handle; with no caller it degrades cleanly (it never trusts a
half-rebase or invents an agent). This limit is inherent to the delegation
design, not a porting gap.

## Drift guard

The four `scripts/lib/pr-*.sh` are deleted and their
`scripts/lint/.clone-only-scripts.txt` entries removed, so `fno lint
shellout-drift` passes without flagging the pr paths. `_merge.py`'s best-effort
per-PR artifact consolidation still shells to the canonical
`scripts/lib/consolidate-artifacts.sh`, but resolves its path from the plugin
root via a direct environment read (`CLAUDE_PLUGIN_ROOT` / `FNO_REPO_ROOT`),
not the shared `resolve_repo_root` resolver. That keeps it an in-repo-only
maintainer side-effect that is out of scope for the drift guard, and it
degrades to a silent no-op on a bare install where the script is absent.
