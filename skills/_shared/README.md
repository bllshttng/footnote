# skills/_shared/

This folder is a **dev-only build input**. The files here are canonical
sources for content that the bundler copies into other skills via
`skill-bundles.yaml`. They are NOT shipped as a skill in their own right
(notice the absence of a `SKILL.md`).

Consumers receive bundled copies at `skills/<consumer>/references/<name>.md`
(frontmatter stripped) via `scripts/generate-skill-bundles.sh`. The
freshness CI gate (`scripts/lint/check-skill-bundles-fresh.sh`) keeps
consumer bundles in sync with these canonicals.

## Why this exists

Driver skills (`/target`, `/megawalk`, `/megatron`) need to share content
without runtime cross-skill delegation. A path escape like
`../../_shared/X.md` works inside the footnote repo but breaks the moment
the skill folder is lifted into Codex, Gemini, or any other markdown-aware
runtime. Bundling at build time makes each consumer's folder self-contained
on disk at checkout while keeping a single source of truth.

## When to edit

1. Edit the canonical file in this folder.
2. Run `bash scripts/generate-skill-bundles.sh` (or rely on the pre-commit
   hook at `scripts/hooks/pre-commit-skill-bundles.sh` if you've installed it).
3. Commit both the canonical and the regenerated bundles together.
4. CI's freshness gate will fail any PR that ships a canonical edit without
   the matching bundle update.

## Why not delete this folder

We considered moving the canonicals to `dev/canonicals/` for a clearer name.
Decided against it: the existing layout works, and renaming for symmetry
adds churn without changing the runtime behavior. The `README.md` you are
reading is enough to set expectations.

See `internal/fno/plans/2026-05-11-skill-encapsulation-refactor/`
for the refactor history.
