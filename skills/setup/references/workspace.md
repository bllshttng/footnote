# Workspace Skill

Provides awareness of related projects for cross-project coordination.

## Workspace Config Location

Check in order:
1. `.fno/settings.yaml` (current project - rare, for project-specific overrides)
2. `~/.fno/settings.yaml` (global - primary location)

## Settings Schema

settings.yaml has two top-level sections: `work` (project topology) and `config` (execution defaults). See `workspace/references/config-schema.md` for canonical defaults.

### Schema

Projects are always organized under named workspaces in `work.workspaces.{name}.projects`. Even single-product setups use a workspace - the `/setup` wizard creates one named after your project. This avoids dual-mode branching in skills.

```yaml
# ~/.fno/settings.yaml (global)

work:
  # Multi-workspace schema: organize projects into logical workspaces
  workspaces:
    myplatform:
      description: SaaS platform with web frontend and API backend
      projects:
        - name: frontend
          path: ~/code/myplatform/frontend
          type: frontend
          stack:
            - react
            - typescript
            - tailwind
          default: true
          package_manager: pnpm
          test_command: pnpm test
          build_command: pnpm build

        - name: api
          path: ~/code/myplatform/api
          type: backend
          stack:
            - python
            - fastapi
            - postgres
          package_manager: uv
          test_command: pytest
          deploy_command: cdk deploy

        - name: notification-service
          path: ~/code/myplatform/notification-service
          type: service
          stack:
            - python
            - fastapi
          package_manager: uv

        - name: worker
          path: ~/code/myplatform/worker
          type: service
          stack:
            - python
            - celery
          package_manager: uv

  # Cross-project coordination patterns
  patterns:
    branch_naming: "feature/{feature-name}"
    pr_linking: true  # Link PRs across repos
    # Default execution order by project type.
    # Order 1 completes (implement + PR) before order 2 starts.
    execution_order:
      backend: 1
      service: 1
      frontend: 2
      docs: 2
      fullstack: 1
      plugin: 1

  # Worktree configuration (used by the git-worktrees skill)
  worktree:
    base: .claude/worktrees        # relative to each project root (matches Claude Code native)
    shared_branch_name: true       # same branch name across all projects
    symlink_directories:           # maps to Claude Code's worktree.symlinkDirectories
      - node_modules
      - .venv

  # Worktree resolution
  # Full path: {project.path}/{work.worktree.base}/{feature-slug}
  # Example: ~/code/myplatform/frontend/.claude/worktrees/auth

  # Shared resources (optional - only include what applies)
  shared:
    github_org: myplatform

  # Testing context (read by browser-testing and archer agents)
  testing:
    frontend:
      auth:
        # Dev login - bypasses normal auth for testing
        dev_login:
          url: localhost:3000/dev-login
          description: "Direct admin login - bypasses normal auth flow"
          roles: [admin, user]

        # Email OTP auth for regular users
        email_otp:
          method: email_otp
          description: "Users verify via email one-time password"
          test_emails:
            - "test-user@example.com"
            - "test-admin@example.com"
          otp_retrieval:
            script: "scripts/get-otp.sh {email}"
            playwright: "await getLatestOTP('test-user@example.com')"
            env_var: TEST_OTP  # If fixed OTP for test env

      # Things agents should know when testing this project
      gotchas:
        - "File uploads require S3 mock in local tests"
        - "Timestamps are UTC, displayed in user's timezone"
        - "OAuth actions require token - use dev_login to bypass"

config:
  budget_cap: 25
  no_external: false
  no_docs: false
  no_verify: false
  notifications:
    enabled: true

  # External reviewer configuration
  external_reviewer: none            # gemini | coderabbit | claude | codex | none
  external_reviewer_bot: ""          # GitHub bot login for polling

  # Commit style
  commit_style: conventional         # conventional | angular | none
  commit_scopes: [feat, fix, refactor, docs, test, chore]

  # Documentation preferences
  docs:
    how_to_guides: false             # Generate end-user how-to guides
    how_to_path: docs/howto          # Where how-to guides live
    architecture_docs: true          # Generate architecture docs
    roles: []                        # User roles for how-to guides

  # Model profile
  profile: balanced                  # quality | balanced | budget
```

### Local Override (.fno/settings.yaml)

```yaml
# .fno/settings.yaml (project-local, overrides global config)
config:
  expertise: frontend
  max_iterations: 20
  no_external: true   # override global default
```

## Project Types

| Type | Description | Example | Default Order |
|------|-------------|---------|---------------|
| `frontend` | Web UI, React/Vue/etc | frontend | 2 |
| `backend` | API, database, serverless | api | 1 |
| `service` | Standalone microservice | notification-service | 1 |
| `docs` | Documentation site (Mintlify, Docusaurus, etc) | docs | 2 |
| `shared` | Shared libraries | common-types | 1 |
| `infra` | Infrastructure as code | terraform | 1 |
| `fullstack` | Combined frontend + backend | monolith | 1 |
| `plugin` | Tool/editor plugin | tools | 1 |

## Workspace Auto-Detection

Abilities detects which workspace the current project belongs to by matching `pwd` against project paths in settings.yaml:

```
pwd = ~/code/myplatform/api
-> matches work.workspaces.myplatform.projects[1] (api)
-> workspace = "myplatform", project = "api", type = "backend"
```

This determines:
- Which **related projects** to consider when dispatching foreign work (spawn-into-project)
- Which **testing context** to inject for the current project
- Which **worktree base** to use when creating feature branches
- Which **project roots** to resolve when spawning a `/target` worker into a peer project

If `pwd` doesn't match any project path, workspace features are disabled and tools operates in single-project mode.

## Using Work Config

### In Skills

```markdown
# At start of skill that needs cross-project awareness
If file exists "~/.fno/settings.yaml":
  Load work config (work.workspaces.{name}.projects)
  Set project_paths variable
  Enable cross-project features
```

### In Target (spawn-into-project)

A `/target` session works only in its own project. When a wave belongs to a
peer project, the session resolves that project's root from `work.workspaces`
and spawns a worker into it (`fno agents spawn --cwd <root> "/target <node>"`)
rather than editing the peer repo. Each project ships its own PR; there is no
matched-worktree / linked-PR pipeline.

### In Audit

When analyzing features:
```
1. Load settings.yaml
2. Scan ALL project paths for implementations
3. Create plans that reference correct repos
4. Tag plans with affected projects
```

## Worktree Resolution

Worktrees are created at `{project.path}/{work.worktree.base}/{feature-slug}`,
where `feature-slug` is derived from the branch name (e.g., `feature/auth` ->
`auth`). Each project's worktree is independent; there is no matched-worktree
coordination across repos.

## Add Project (`--add-project` / `--add-projects`)

Quick commands to register projects in settings.yaml. Also available via `/setup --add-project`.

```bash
/workspace --add-project [path]        # Add single project (default: cwd)
/workspace --add-project ~/code/myapp  # Add specific path
/workspace --add-projects              # Batch mode - keep adding until 'done'
```

### Process

**1. Resolve path and auto-detect:**

```bash
PROJECT_PATH="${1:-$(pwd)}"
PROJECT_PATH=$(cd "$PROJECT_PATH" && pwd)  # resolve to absolute
PROJECT_NAME=$(basename $(git -C "$PROJECT_PATH" rev-parse --show-toplevel 2>/dev/null) 2>/dev/null || basename "$PROJECT_PATH")

# Detect stack
HAS_PACKAGE_JSON=$([[ -f "$PROJECT_PATH/package.json" ]] && echo true || echo false)
HAS_PYPROJECT=$([[ -f "$PROJECT_PATH/pyproject.toml" ]] && echo true || echo false)
HAS_CARGO=$([[ -f "$PROJECT_PATH/Cargo.toml" ]] && echo true || echo false)
HAS_GO_MOD=$([[ -f "$PROJECT_PATH/go.mod" ]] && echo true || echo false)

# Infer type + stack
if $HAS_PACKAGE_JSON; then
    HAS_REACT=$(grep -q '"react"' "$PROJECT_PATH/package.json" 2>/dev/null && echo true || echo false)
    HAS_MINTLIFY=$(grep -q '"mintlify"' "$PROJECT_PATH/package.json" 2>/dev/null && echo true || echo false)
    HAS_DOCUSAURUS=$(grep -q '"@docusaurus' "$PROJECT_PATH/package.json" 2>/dev/null && echo true || echo false)
    HAS_NEXTRA=$(grep -q '"nextra"' "$PROJECT_PATH/package.json" 2>/dev/null && echo true || echo false)
    HAS_VITEPRESS=$(grep -q '"vitepress"' "$PROJECT_PATH/package.json" 2>/dev/null && echo true || echo false)
    HAS_MKDOCS=$([[ -f "$PROJECT_PATH/mkdocs.yml" ]] && echo true || echo false)
    if $HAS_MINTLIFY; then TYPE="docs"; STACK="mintlify, typescript"
    elif $HAS_DOCUSAURUS; then TYPE="docs"; STACK="docusaurus, typescript"
    elif $HAS_NEXTRA; then TYPE="docs"; STACK="nextra, typescript"
    elif $HAS_VITEPRESS; then TYPE="docs"; STACK="vitepress, typescript"
    elif $HAS_MKDOCS; then TYPE="docs"; STACK="mkdocs, typescript"
    elif $HAS_REACT; then TYPE="frontend"; STACK="react, typescript"
    else TYPE="fullstack"; STACK="node, typescript"; fi
    PKG_MGR=$([[ -f "$PROJECT_PATH/pnpm-lock.yaml" ]] && echo pnpm || ([[ -f "$PROJECT_PATH/yarn.lock" ]] && echo yarn || echo npm))
elif $HAS_PYPROJECT; then
    TYPE="backend"; STACK="python"; PKG_MGR="uv"
elif $HAS_CARGO; then
    TYPE="backend"; STACK="rust"; PKG_MGR="cargo"
elif $HAS_GO_MOD; then
    TYPE="backend"; STACK="go"; PKG_MGR="go"
elif [[ -f "$PROJECT_PATH/mint.json" ]]; then
    # Mintlify without package.json (uses npx)
    TYPE="docs"; STACK="mintlify"; PKG_MGR="npx"
elif [[ -f "$PROJECT_PATH/mkdocs.yml" ]]; then
    TYPE="docs"; STACK="mkdocs"; PKG_MGR="pip"
else
    TYPE="unknown"; STACK=""; PKG_MGR=""
fi
```

**2. Determine workspace:**

If `work.workspaces` has only one workspace, auto-select it. If multiple, infer from path prefix (e.g., `~/code/myplatform/*` -> myplatform). If ambiguous, ask:

```
AskUserQuestion: "Which workspace does this project belong to?"
  Options: [{workspace names from settings.yaml}, "Create new workspace"]
```

**3. Check for duplicates:**

Read projects from the target workspace. If a project with the same `name` or `path` already exists, report it and exit.

**4. Summary confirmation (one prompt):**

```
AskUserQuestion: "Adding to workspace '{WORKSPACE_NAME}':

  Name:  {PROJECT_NAME}
  Path:  {PROJECT_PATH}
  Type:  {TYPE}
  Stack: [{STACK}]
  Pkg:   {PKG_MGR}
  Order: {N} ({TYPE} -> order {N})

Confirm?"
  Options: ["Yes", "Change type", "Change name", "Change workspace", "Cancel"]
```

If user picks a "Change" option -> ask the specific follow-up, then re-show summary. Most of the time auto-detection is correct and users just pick "Yes".

**5. Write to settings.yaml & confirm:**

Append to the target workspace's projects list and report:

```
Added {PROJECT_NAME} ({TYPE}) to workspace '{WORKSPACE_NAME}'
  Execution order: {N} ({TYPE} -> order {N})
  Total projects in workspace: {count}
```

### Batch Mode (`--add-projects`)

Loop mode for registering multiple projects:

```
AskUserQuestion: "Enter project path (or 'done'):"
  Placeholder: "~/code/myapp/api"
```

For each path:
1. Auto-detect name/type/stack/pkg
2. Auto-select workspace (infer from path, or ask if ambiguous)
3. Show summary confirmation (same as single mode)
4. On "Yes" -> append and ask for next path
5. On "done" -> show final summary of all added projects

```
Added 3 projects:
  api         (backend)  -> myplatform [order 1]
  webapp      (frontend) -> myplatform [order 2]
  docs        (docs)     -> myplatform [order 2]
```

### Optional Flags

| Flag | Effect |
|------|--------|
| `--name NAME` | Override auto-detected name |
| `--type TYPE` | Override auto-detected type |
| `--order N` | Override execution_order default |
| `--workspace NAME` | Skip workspace detection, add to this workspace |
| `--worktree-base PATH` | Set custom worktree base |

## Remove Project (`--remove-project`)

```bash
/workspace --remove-project [name]     # Remove by name
/workspace --remove-project            # Interactive - pick from list
```

### Process

**1. If no name given, show project list:**

```
AskUserQuestion: "Which project to remove?

  myplatform:
    1. frontend            (frontend)
    2. api                 (backend)
    3. notification-service (service)
  side-project:
    4. app                 (fullstack)
  tools:
    5. cli-tool            (plugin)

Enter number or name:"
```

**2. Confirm removal:**

```
AskUserQuestion: "Remove 'notification-service' from workspace 'myplatform'?

  This only removes it from settings.yaml - no files are deleted."
  Options: ["Yes, remove", "Cancel"]
```

**3. Remove from settings.yaml & confirm:**

Remove the project entry from the workspace's projects list. If the workspace has no projects left, remove the workspace too.

```
Removed notification-service from workspace 'myplatform'
  Remaining projects in myplatform: 2
```

---

## Remove Workspace (`--remove-workspace`)

```bash
/workspace --remove-workspace [name]   # Remove by name
/workspace --remove-workspace          # Interactive - pick from list
```

### Process

**1. If no name given, show workspace list:**

```
AskUserQuestion: "Which workspace to remove?

  1. myplatform  (3 projects: frontend, api, notification-service)
  2. side-project    (1 project: app)
  3. tools   (1 project: cli-tool)

Enter number or name:"
```

**2. Confirm removal:**

```
AskUserQuestion: "Remove workspace 'side-project' and its 1 project?

  Projects that will be unregistered:
    - app (fullstack)

  This only removes from settings.yaml - no files are deleted."
  Options: ["Yes, remove workspace", "Cancel"]
```

**3. Remove from settings.yaml & confirm:**

```
Removed workspace 'side-project' (1 project unregistered)
  Remaining workspaces: myplatform, footnote
```

---

## Initialization Process

When `/workspace --init`:

1. **Detect current project:**
```bash
project_name=$(basename $(pwd))
# Detect stack from package.json, pyproject.toml, etc.
```

2. **Ask for workspace name:**
```
What's the name of this workspace? (e.g., MyPlatform)
```

3. **Ask for related projects:**
```
Add related projects (enter path or 'done'):
> ~/code/myplatform/api
Detected: Python backend (fastapi, postgres)
> done
```

4. **Write config:**
```yaml
# .claude/settings.yaml created
```

## Environment Variables

For projects with env vars:
```yaml
- name: api
  env_var: API_PROJECT_PATH
```

Skills can then use:
```bash
cd $API_PROJECT_PATH
```

## Testing Context Schema

The `testing` section provides project-specific testing context that agents read when running E2E tests or browser automation.

### Schema

```yaml
testing:
  {project-name}:
    auth:
      # Authentication shortcuts for testing
      dev_login:
        url: string           # Direct login URL (bypasses auth)
        description: string   # What this login method does
        roles: [string]       # Roles available via this method

      {custom_auth_method}:   # e.g., email_otp, magic_link, sso
        method: email_otp|phone_otp|magic_link|sso
        description: string
        test_emails: [string]   # Test accounts for auth
        otp_retrieval:
          script: string      # Shell script to get OTP
          playwright: string  # Playwright helper function
          env_var: string     # Env var with fixed OTP

    gotchas: [string]         # Project-specific testing gotchas
```

### How Agents Use Testing Context

1. **browser-testing skill**: Reads testing.{project}.auth to know available login methods
2. **target agent**: Injects gotchas into test context when running E2E
3. **target orchestrator**: Passes OTP retrieval method when browser_testing phase runs

### Example: Testing User Sign-In Flow

```typescript
// Agent reads from settings.yaml:
// testing.frontend.auth.email_otp.test_emails[0]
// testing.frontend.auth.email_otp.otp_retrieval.playwright

test('user can sign in with email OTP', async ({ page }) => {
  await page.goto('/sign-in');
  await page.fill('[name="email"]', 'test-user@example.com');
  await page.click('button[type="submit"]');

  // Get OTP using configured helper
  const otp = await getLatestOTP('test-user@example.com');
  await page.fill('[name="otp"]', otp);
  await page.click('button[type="submit"]');

  await expect(page).toHaveURL('/dashboard');
});
```

## Key Principles

- **Single source of truth** - settings.yaml defines all projects
- **Auto-detection** - Infer stack from project files
- **Consistent naming** - Same branch names across repos
- **Linked PRs** - Reference related PRs automatically
- **Shared config** - GitHub org, shared resources in one place

## Red Flags

**Never:**
- Hardcode project paths in skills
- Assume project structure without checking workspace
- Create mismatched branch names across repos
- Forget to link PRs

**Always:**
- Check for settings.yaml first
- Use env_var if defined
- Match branch names across projects
- Update all related repos together
