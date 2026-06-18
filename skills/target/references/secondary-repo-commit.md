# Secondary Repo Inline Commit (NON-NEGOTIABLE)

**Load when:** a plan task requires touching files in a repo other than the primary one (e.g., a frontend plan that needs a backend migration). Cross-project coordination at folder/repo scale uses `cross-project` instead.

When a plan task requires work in a secondary repo, you **MUST branch, commit, and PR it inline** — before `cd`-ing back to the main repo.

## FORBIDDEN

- Leaving files uncommitted in a secondary repo
- Committing to `main` — always create a feature branch first

## Pattern

```bash
cd {backend.path}
git checkout -b feature/{same-slug-as-main-branch}   # branch FIRST
# ... create files ...
git add supabase/migrations/YYYYMMDD_*.sql            # only your files
git commit -m "feat(schema): add facility_responses tables"
git push -u origin feature/{same-slug-as-main-branch}
gh pr create --title "feat(schema): ..." --body "Related: {main-repo} PR pending"
cd -
```

## Rules

- Branch BEFORE writing — never work on main in secondary repos
- Branch name matches the main repo's for traceability
- Only `git add` specific files — never `git add .`
- Log secondary PR URL in target-state: `secondary_prs:`
- After main PR is created (Phase 6), cross-link both PRs

## When NOT to use this

If >3 files or meaningful parallel work across repos, use `cross-project` instead. See [cross-project.md](cross-project.md).

## External Reviewer Resolution

**Critical for Phase 7:** `pr_number` MUST be set. Before invoking check-pr, read reviewer config:

```bash
source scripts/lib/config.sh
REVIEWER_TYPE=$(get_config "external_reviewer" "gemini")
```

If `REVIEWER_TYPE` is `"none"`:
- Update target-state.md: set `external_review_passed: skipped`, `no_external: true` (provenance: settings.yaml)
- Log: "External review disabled in settings — skipping"

Otherwise:
- Invoke `fno:pr check {pr_number}` (the skill reads its own reviewer config)

ENFORCE: if `external_review_passed` is false and reviewer is not "none", MUST run this phase.
