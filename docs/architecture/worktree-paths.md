# Canonical worktree paths

## Summary

Every git worktree created by footnote lives at exactly one shape:

```
~/.fno/worktrees/{project_id}-{slug}/
```

Examples:

- `~/.fno/worktrees/abi-provider-rotation-codex-ab-1234abcd/`
- `~/.fno/worktrees/web-onboarding-cleanup/`
- `~/.fno/worktrees/acme-web-citation-extractor/`

This replaces two earlier locations that previously coexisted:

- `<repo>/.claude/worktrees/{slug}/` (in-project, gitignored, leaked into `git status` as `??` because the parent isn't ignored)
- `~/conductor/workspaces/{proj}/{slug}/` (per-project subfolder under a different shape, used by the conductor mechanism)

A second-pass review flagged the `.claude/` prefix as Claude-specific implementation leaking into the CLI-agnostic `_shared.create_worktree` helper. This document records the design that closed the finding.

## Why flat with a project prefix

| Property | Flat (`abi-foo`) | Nested (`fno/foo`) |
|---|---|---|
| Single canonical home across all projects | yes | yes |
| Cleanup sweep | `ls ~/.fno/worktrees/abi-*` | `ls ~/.fno/worktrees/fno/` (with empty-dir trail) |
| Provenance at a glance | yes (prefix is right there) | yes (one level deeper) |
| `cd` ergonomics | one segment | two segments |
| Empty-project-dir clutter | none | leftover empty `fno/` dirs after sweep |
| `git worktree list` as cross-project dashboard | one-shot | needs traversal |

Flat wins on operator ergonomics; the prefix carries the provenance.

## Layers

```
   adapter callers (ClaudeCodeAdapter / CodexCliAdapter / ...)
                │
                ▼
   fno.adapters._shared.create_worktree
   (CLI-agnostic primitive used by every adapter)
                │
                ▼
   fno.worktree_paths   <-- single source of truth
   ├── resolve_project_id     (settings.yaml -> git remote -> repo basename)
   ├── worktree_path           (canonical full path)
   ├── legacy_worktree_path    (old shape; detection only)
   └── worktree_base           (~/.fno/worktrees/)
```

The runtime layer (`fno.runtime.worktree`) is a separate caller of the same helpers; it adds the `.fno` symlink wiring and `list`/`remove` verbs that the adapter primitive doesn't need.

## `project_id` resolution chain

`resolve_project_id(repo_root)` walks three sources, in order, and returns the first match:

1. `project.id` in `<repo_root>/.fno/settings.yaml`
2. Basename of `git remote get-url origin` (with `.git` suffix stripped)
3. `repo_root.name` (final fallback)

The chosen id is validated against `^[A-Za-z0-9][A-Za-z0-9._-]*$` with an explicit `..` rejection. An id that fails validation raises `ValueError` rather than silently producing a bad path. Defense-in-depth against the path-traversal class of bug previously flagged in review.

A typical project declares `project.id` explicitly:

```yaml
# .fno/settings.yaml
project:
  id: fno
  name: footnote    # long form, display only
```

Projects that haven't declared an id fall through to the git remote basename automatically, so day-one adoption is zero-config for any repo with an `origin` remote.

## Back-compat (AC7)

`legacy_worktree_path(name, repo_root)` returns `<repo_root>/.claude/worktrees/{name}/`. Both `_shared.create_worktree` and `runtime.create_worktree` probe this path before calling `git worktree add`; if it exists, they short-circuit with `status: "already-exists"` and return the legacy path so an operator's in-flight legacy worktree keeps working.

`runtime.list_worktrees` accepts worktrees under either the canonical base or the legacy base through the transition window, so `fno runtime worktree --action list` keeps surfacing legacy entries until they are torn down.

`runtime.remove_worktree` picks the canonical path when it exists, otherwise falls back to the legacy path, so `--action remove --name foo` keeps working regardless of which shape the worktree lives under.

## Migration

Option A: leave old worktrees alone. New `create` calls land at the canonical path; legacy worktrees finish their lifecycle in `.claude/worktrees/`. Zero risk to active work.

An `fno worktrees migrate` sweep tool is a separate follow-up (the original brief's AC4); not required to ship the path-standardisation change.

## Validation

Both `project_id` and `name` are validated by the same helper before any filesystem operation:

```python
_SAFE_COMPONENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")

def _validate_component(value, *, kind):
    if not value: raise ValueError(...)
    if ".." in value: raise ValueError(...)   # belt
    if not _SAFE_COMPONENT.match(value): ...  # braces
```

The `..` substring check and the regex check are independent so neither can be bypassed by the other. Path traversal (`../escape`) is rejected before any `Path` construction happens.

## Related references

- Origin: second-pass review of the Codex CLI adapter
