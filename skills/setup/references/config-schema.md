# Config Schema — All Keys with Defaults

Skills read these via `get_config()` from `scripts/lib/config.sh`.
If a key is absent, the default is used. No hardcoding in skills.

```yaml
# project block (top-level, NOT under config)
project:
  id: fno                                # short stable identifier for worktree path
                                         # prefixes (~/.fno/worktrees/{id}-{slug}/).
                                         # Resolved via:
                                         #   1. project.id in this file
                                         #   2. basename of `git remote get-url origin`
                                         #   3. basename of repo_root
                                         # Validated against ^[A-Za-z0-9][A-Za-z0-9._-]*$.
                                         # Never reused across projects (ab-3180b3f4).

config:
  # Plan paths (prefer plansDirectory in .claude/settings.json)
  plans:
    focused_path: ""                     # where /do plans save (no default — read from .claude/settings.json plansDirectory)
    full_path: ""                        # where /do waves plans save (no default — read from .claude/settings.json plansDirectory)

  # Documentation
  docs:
    how_to_guides: false                 # disabled by default
    how_to_path: docs/howto              # where how-to guides save
    architecture_path: docs/architecture # where architecture docs save
    test_plan_path: docs/test-plans      # where test plans save
    roles: []                            # user roles for how-to guides (e.g., [admin, user])

  # Linear integration (absent = disabled)
  # linear:
  #   enabled: true
  #   team: TEAM                         # Linear team prefix (e.g., RR, FN)
  #   workspace: my-workspace            # Linear workspace slug for URLs

  # External review
  external_reviewer: none                # gemini | coderabbit | claude | codex | none
  external_reviewer_bot: ""              # GitHub bot login for polling

  # Execution
  budget_cap: 25
  commit_style: conventional
  profile: balanced                      # quality | balanced | budget

  # Schema sources - inside-out naming approach
  # Set these once during /setup. Agents read them directly instead of grepping.
  # This is the #1 way to prevent field name mismatches (facility_id vs facilityNumber).
  schema_sources:
    types: ""                        # frontend type definitions (e.g., src/types/)
    db: ""                           # database schema source of truth:
                                     #   supabase: src/types/supabase.ts (from `supabase gen types typescript`)
                                     #   prisma: prisma/schema.prisma
                                     #   drizzle: src/db/schema.ts
                                     #   raw SQL: supabase/migrations/
    api: ""                          # API routes/controllers (e.g., src/app/api/)
    naming_boundary: ""              # where snake_case DB columns map to camelCase frontend:
                                     #   e.g., src/lib/supabase/transforms.ts
                                     #   or: "supabase-client-auto" (if using .from().select())

work:
  # Always use workspaces — even single-product setups get one workspace.
  # /setup wizard creates the first workspace named after your project.
  workspaces: {}                         # named workspaces with projects

  # Worktree defaults
  worktree:
    base: ~/.fno/worktrees         # canonical flat layout; full path is
                                         # ~/.fno/worktrees/{project.id}-{slug}/
                                         # (Plan ab-3180b3f4). Old projects may still
                                         # carry base: .claude/worktrees for the
                                         # transition window.
    shared_branch_name: true
    auto_install: true                   # Default true. Set false to skip the
                                         # WorktreeCreate hook's auto-detected
                                         # dep install (pnpm install / uv sync /
                                         # etc.). Useful at scale when many
                                         # worktrees of the same project each
                                         # materialize their own .venv and bloat
                                         # the uv cache (45GB+ observed). An
                                         # explicit setup_command still runs.

  # Project-type execution ordering (informs blocked_by chaining when a
  # multi-repo feature is decomposed into per-project backlog nodes)
  patterns:
    execution_order:
      backend: 1
      service: 1
      frontend: 2
      docs: 2
      fullstack: 1
      plugin: 1
```

## Key Conventions

- **Absent = disabled**: If `config.linear` is not present, Linear is off. No need for explicit `enabled: false`.
- **Empty list = skip**: If `config.docs.roles` is `[]`, role-based how-to generation is skipped.
- **Lookup order**: `.claude/settings.local.json` → `.claude/settings.json` → `.fno/settings.yaml` → `~/.fno/settings.yaml`
- **Claude Code settings.json mapping**: `"plansDirectory"` → `plans.focused_path` and `plans.full_path`
- **Skills reference config keys** using the dotted path (e.g., `config.plans.full_path`), resolved via `get_config "plans.full_path" ""`. No default — unconfigured = ask user.
