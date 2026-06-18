# Cross-Project Pipeline — Subagent Prompt Templates

These templates are used by the cross-project-pipeline skill when dispatching subagents at each step. Each template is self-contained — subagents start with fresh context.

## Setup Step Prompt Template

```
You are setting up a worktree for project "{PROJECT_NAME}" as part of a cross-project feature.

PROJECT_PATH: {absolute_path}
FEATURE_SLUG: {feature_slug}
SETUP_COMMAND: {setup_command}
TEST_COMMAND: {test_command}

## Your job:
1. Run: bash {plugin_root}/scripts/cross-project-setup.sh \
       "{PROJECT_PATH}" "{feature_slug}" "{SETUP_COMMAND}" "{TEST_COMMAND}"
2. Parse the JSON it emits on stdout: {status, worktree_path, branch, test_exit}
3. Report result.

The script delegates to scripts/lib/worktree-manager.sh, which honors
per-project worktree_base from settings.yaml and caches deps install
on lockfile hash. See skills/_shared/worktree.md for the decision matrix.

## Return:
STATUS: OK|FAILED
WORKTREE_PATH: {absolute worktree path}
TEST_COUNT: {number of passing tests}
ERROR: {if FAILED}
```

## Implement Step Prompt Template

```
You are implementing tasks for project "{PROJECT_NAME}" in a cross-project feature.

WORKTREE_PATH: {worktree_path}
PROJECT_TYPE: {type}
MEMORY_PATH: {main_project_memory_path}

## Tasks assigned to you:
{task_content_pasted_in_full}

## Context:
{scene_setting_from_plan}

## Rules:
1. Work ONLY in your worktree ({WORKTREE_PATH})
2. Follow TDD: write test → verify fails → implement → verify passes
3. Atomic commits per logical change
4. Self-review before returning
5. Save learnings to MEMORY_PATH (not local project memory)

## When blocked:
Stop and return BLOCKED with specific reason and what would unblock you.

## Return:
STATUS: SUCCESS|DONE_WITH_CONCERNS|FAILED|BLOCKED
COMMITS: [{hash, message}]
CONCERNS: [if DONE_WITH_CONCERNS]
ERROR: [if FAILED/BLOCKED]
```

## Finalize Step Prompt Template

Uses the `cross-project-finalizer` agent definition directly. Additional context passed in the prompt:

```
FEATURE_NAME: {feature_name}
BRANCH_NAME: feature/{feature_slug}
PROJECT_TYPE: {type}
RELATED_PROJECTS: {list of other project names}
MEMORY_PATH: {main_project_memory_path}
```

The finalizer agent handles: atomic commits → code review → push → `gh pr create`.
