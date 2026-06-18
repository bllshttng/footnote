# Cross-Project Coordination Reference

Automatic backend dependency detection and parallel execution with contract-first development.

## Project Mapping

Create `.fno/project-map.yaml` to define project relationships:

```yaml
# .fno/project-map.yaml
projects:
  webapp:
    path: {frontend.path}
    type: frontend
    depends_on: api

  api:
    path: {backend.path}
    type: backend

worktree_base: .claude/worktrees
```

## Backend Detection

Keywords that trigger backend work detection:

```bash
BACKEND_KEYWORDS="migration|api endpoint|database|schema|supabase function|rpc"

for PLAN_FILE in $PLAN_FILES; do
  PLAN_NAME=$(basename "$PLAN_FILE" .md)

  if grep -qiE "$BACKEND_KEYWORDS" "$PLAN_FILE"; then
    echo "⚠️  $PLAN_NAME requires backend work"
    NEEDS_BACKEND[$PLAN_NAME]=true
  else
    echo "✓  $PLAN_NAME is frontend-only"
    NEEDS_BACKEND[$PLAN_NAME]=false
  fi
done
```

## Auto-Create Backend Worktrees

For each plan with `needs_backend: true`, delegate to the shared
worktree manager. Path resolution and branch naming flow through
`scripts/lib/worktree-manager.sh` so per-project `worktree_base`
from `settings.yaml` is honored. See
[worktree.md](worktree.md) for the
manual-vs-ephemeral decision matrix.

```bash
for PLAN_NAME in "${!NEEDS_BACKEND[@]}"; do
  if [[ "${NEEDS_BACKEND[$PLAN_NAME]}" == "true" ]]; then
    BACKEND_NAME=$(basename "$BACKEND_PATH")

    cd "$BACKEND_PATH"
    git fetch origin

    RESULT=$(bash "$FNO_PLUGIN_ROOT/scripts/lib/worktree-manager.sh" \
        create "$BACKEND_NAME" "$PLAN_NAME" --mode=manual)
    BACKEND_WORKTREE=$(echo "$RESULT" | python3 -c \
        'import json,sys; print(json.load(sys.stdin)["path"])')
    EXISTED=$(echo "$RESULT" | python3 -c \
        'import json,sys; print(json.load(sys.stdin)["existing"])')

    if [[ "$EXISTED" == "True" ]]; then
      echo "✓ Backend worktree already exists, reusing: $BACKEND_WORKTREE"
    else
      # Run setup (lockfile-hash cached, env-files copied)
      bash "$FNO_PLUGIN_ROOT/scripts/lib/worktree-manager.sh" \
          setup "$BACKEND_WORKTREE" >&2 || true
      echo "✓ Created backend worktree: $BACKEND_WORKTREE"
    fi
  fi
done
```

## Auto-Spawn Backend Agents

After creating worktrees, spawn agents with MCP context:

```bash
BACKEND_PROMPT="You are working in: $BACKEND_WORKTREE

**Documentation Access:**
You have access to context7 MCP tools for documentation lookup.
For Supabase patterns, use:
- mcp__context7__resolve-library-id with libraryName='supabase'
- mcp__context7__query-docs with the resolved ID

**Task:**
Create Supabase migrations for the following schema:

$EXPECTED_SCHEMA

**Steps:**
1. Use context7 to lookup current RLS policy patterns if needed
2. cd to the backend worktree
3. Run: npx supabase migration new \"migration_name\"
4. Edit the generated migration file following Supabase best practices
5. Commit with message: \"feat(db): description (ticket)\"
6. Push to origin"

Task(
  description="Create backend migration for $PLAN_NAME",
  prompt="$BACKEND_PROMPT",
  subagent_type="fno:target"
)
```

## Contract-First Development (Parallel Execution)

Frontend is NOT blocked on backend merge. Build against mocks/contracts:

```
Execution Flow (Parallel):
┌─────────────────────────────────────────────────────────────┐
│  01-user-dashboard (no backend)  → Execute immediately      │
│  02-settings-page (no backend)   → Execute in parallel      │
│  03-api-integration (needs backend):                        │
│    BACKEND (worktree A):                                    │
│      1. Create backend worktree                             │
│      2. Execute backend tasks (migrations, endpoints)       │
│      3. Create backend PR → awaiting human merge            │
│                                                             │
│    FRONTEND (worktree B, IN PARALLEL):                      │
│      1. Extract expected schema from plan                   │
│      2. Check local DB for existing tables                  │
│      3. Generate mocks for NEW tables/endpoints             │
│      4. Build frontend against mocks                        │
│      5. Tests pass against mocks                            │
│      6. Create frontend PR (with integration TODO)          │
└─────────────────────────────────────────────────────────────┘
```

## Schema Detection for Mocks

```bash
# Read .env.local for database connection
source .env.local

# For existing tables: query local DB schema
psql $DATABASE_URL -c "\d table_name" > existing_schema.txt

# For NEW tables mentioned in plan: extract expected schema
grep -iE "(create table|schema|columns|migration)" "$PLAN_FILE" > expected_schema.txt
```

## Mock Generation

```typescript
// Generated: __mocks__/api/newEndpoint.ts
// Based on plan: "POST /api/widgets returns { id, name, status }"

export const mockCreateWidget = {
  id: 'mock-id-123',
  name: 'Test Widget',
  status: 'active',
  created_at: new Date().toISOString(),
};

// TODO: Replace with real API when backend PR #42 merges
export async function createWidget(data: CreateWidgetInput) {
  if (process.env.USE_MOCKS === 'true') {
    return mockCreateWidget;
  }
  return fetch('/api/widgets', { method: 'POST', body: JSON.stringify(data) });
}
```

## State Tracking

```yaml
- name: 03-api-integration
  needs_backend: true
  # Backend
  backend_pr: 42
  backend_merged: false
  # Frontend
  frontend_pr: 43
  using_mocks: true
  mocks_path: __mocks__/api/
  integration_todo: "Swap mocks for real API when PR #42 merges"
```

## Post-Merge Integration

When user merges backend PR and runs `--resume`:

```bash
# Check if backend PR merged
MERGED=$(gh pr view 42 --json merged --jq '.merged')

if [[ "$MERGED" == "true" ]]; then
  echo "✓ Backend PR #42 merged"
  # Update state: backend_merged: true, using_mocks: false
  # Create follow-up task: "Remove mocks, test against real API"
fi
```
