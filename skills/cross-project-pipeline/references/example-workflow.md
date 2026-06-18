# Cross-Project Pipeline — Example Workflow

## Scenario

Add authentication — backend API endpoint + frontend sign-in UI.

## 1. Configuration (settings.yaml)

```yaml
# ~/.fno/settings.yaml
work:
  workspaces:
    my-workspace:
      description: My platform with API and web frontend
      projects:
        - name: api
          path: ~/code/my-project/api
          type: backend
          stack: [python, supabase, aws-lambda]
          package_manager: uv
          test_command: pytest
          setup_command: uv sync

        - name: webapp
          path: ~/code/my-project/webapp
          type: frontend
          stack: [tanstack-start, react, typescript]
          package_manager: pnpm
          test_command: pnpm test
          setup_command: pnpm install

  worktree:
    base: .claude/worktrees
    shared_branch_name: true

  cross_project:
    memory_target: main
    pr_linking: true
    finalize_model: sonnet

  patterns:
    execution_order:
      backend: 1
      frontend: 2
```

## 2. Invocation

```bash
/target cross-project "add auth to API and frontend"
```

## 3. Plan Generation

The `/blueprint cross-project` generates an INDEX with:

```yaml
execution_mode: mixed
scope: cross-project
projects:
  api:
    order: 1              # schema + endpoints must exist before frontend
    tasks: [1.1, 2.1]
  webapp:
    order: 2              # starts after api has PR
    tasks: [1.2, 2.2, 3.1]

waves:
  - wave: 1
    mode: parallel
    tasks: [1.1, 1.2]
    reason: "Database schema + frontend scaffolding are independent"
  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2]
    reason: "Backend endpoint + frontend API client are independent"
  - wave: 3
    mode: sequential
    tasks: [3.1]
    reason: "Integration test requires both backend and frontend"
```

## 4. Execution Trace

### Step 1: Setup (parallel)

```
[pipeline] → git worktree add .claude/worktrees/auth -b feature/auth
             uv sync → pytest (12 passing)
             STATUS: OK

[web]      → git worktree add .claude/worktrees/auth -b feature/auth
             pnpm install → pnpm test (47 passing)
             STATUS: OK
```

### Step 2: Implement (parallel)

```
[pipeline] → target agent
             Tasks: 1.1 (schema), 2.1 (endpoint)
             TDD: write test → implement → verify
             RESULT: SUCCESS, 3 commits

[web]      → target agent
             Tasks: 1.2 (scaffolding), 2.2 (client), 3.1 (integration)
             TDD: write test → implement → verify
             RESULT: SUCCESS, 4 commits
```

### Step 3: Finalize (parallel)

```
[pipeline] → cross-project-finalizer
             sigma-review → fix 1 issue → push → gh pr create
             PR #124: feat(auth): add authentication endpoint

[web]      → cross-project-finalizer
             sigma-review → push → gh pr create
             PR #789: feat(auth): add sign-in UI
```

### Step 4: Link (main session)

```
gh pr edit #124 --body "$(add Related PRs table with #789)"
gh pr edit #789 --body "$(add Related PRs table with #124)"

target-state update:
  projects:
    api:
      status: COMPLETE
      pr_url: https://github.com/org/api-repo/pull/124
      pr_number: 124
    webapp:
      status: COMPLETE
      pr_url: https://github.com/org/webapp-repo/pull/789
      pr_number: 789
```

## 5. Result

```
<promise>MISSION COMPLETE: all 2 project PRs created and linked. PRs: #124 (pipeline), #789 (web)</promise>
```

Both PRs have "Related PRs" sections linking to each other. Memory files are in the main project's memory path.
