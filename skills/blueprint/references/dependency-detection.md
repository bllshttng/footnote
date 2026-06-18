# Detecting Dependencies (for Execution Strategy)

## Explicit Dependencies (from frontmatter)

| Pattern | Example | Meaning |
|---------|---------|---------|
| `depends_on:` | `depends_on: [01]` | Direct reference |
| `after:` | `after: database setup` | Sequential |
| `requires:` | `requires: auth module` | Prerequisite |
| `blocks:` | `blocks: [03, 04]` | Reverse dependency |

## Implicit Dependencies (from file analysis)

| Pattern | Dependency |
|---------|------------|
| Same file in multiple phases | Sequential required |
| `creates: X` / `reads: X` | Schema before usage |
| `implements: X` / `tests: X` | Implementation before tests |
| `endpoint: X` / `calls: X` | API before consumer |
| Migration 001 → 002 | Migrations in order |

## Independence Signals (can run parallel)

Tasks are independent when:
- No shared files between tasks
- No shared database tables/schemas
- No shared API endpoints
- No explicit dependency markers
- Different feature areas
- Same phase prefix (02a, 02b, 02c)

## Quick Reference: Execution Strategy

| Situation | Execution Mode | Wave Mode |
|-----------|----------------|-----------|
| Linear dependency chain | sequential | all sequential |
| Independent features | parallel | all parallel |
| Foundation + parallel work | mixed | wave 1 seq, wave 2+ parallel |
| File conflicts in wave | mixed | that wave sequential |

## Execution Mode Selection

**Sequential signals (wave mode: sequential):**
- Phase N depends on Phase N-1
- Same file modified in multiple tasks
- Database migration ordering
- Test depends on implementation

**Parallel signals (wave mode: parallel):**
- No shared files between tasks
- No explicit depends_on
- Different feature areas
- Same phase prefix (02a, 02b)

**Algorithm:**
1. Build dependency graph from phase files
2. Topological sort to identify waves
3. Within each wave, check for file conflicts
4. If conflicts exist, wave is sequential
5. If no conflicts, wave can be parallel
