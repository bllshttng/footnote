# Blast-radius router

## Why

`/target` scales ceremony by **size** (`S` = do + PR, `M` = think -> blueprint -> do -> review -> ship, `L` = everything), but size is an operator-supplied proxy for *effort/diff* and knows nothing about *blast radius*. Two failure modes follow: a low-blast feature defaulting to `M` pays for ceremony it does not need, and a small-diff change to a dangerous surface marked `S` ships with no rigor. The motivating case was a 2-line `scripts/lib/config.sh` fix that correctly drew the full `M` pipeline because the surface was control-plane with three copies and a test landmine, but nothing in the size machinery *guaranteed* that: `/target S` would have skipped it.

This feature adds a deterministic blast read at `/target` init that modulates the size profile **before** the immutable manifest is written. It is **fail-closed**: high-blast surfaces can only *raise* ceremony (a non-overridable floor at `M`), low-blast work is downgraded to the fast path only when the operator did not pin a size, and every error degrades to "unchanged" so a classifier bug can never block target init.

## Where it fires

Inside `fno target init` (`cli/src/fno/target_cli.py`), after size resolution and before the bash writer (`hooks/helpers/init-target-state.sh`) renders `target-state.md`. The manifest is immutable, so the modulation lands in the `TARGET_SIZE` value handed to the writer, never after. Gated entirely on `config.target.blast.enabled`; disabled is byte-for-byte the pre-feature behavior.

## The classifier (deterministic)

`fno target blast-check <plan>` (classifier in `cli/src/fno/target/blast.py`, off the LOC-ratchet path) prints `{verdict, matched_paths, reason}` (`--quiet` -> bare token). It reads the plan's `## File Ownership Map` and classifies the touched surface against a blast map with two glob dialects:

| Map part | Source | Dialect |
|---|---|---|
| In-repo control-plane | include entries of `scripts/ci/loc-ratchet-manifest.yaml` (reused, one source of truth) | prefix (`hooks/`) / star (`crates/fno-agents/src/loop*`) / exact |
| General (any repo) | a locked category list: auth, migrations + `*.sql`, infra + secrets, billing | `**`-recursive glob (leading-`**/` matches zero dirs) |
| Per-project extension | `config.target.blast.high_blast_globs` | `**`-recursive glob |

Verdict: any touched path matches -> `high`; all paths known and none match -> `low`; empty / unparseable map -> `unknown`.

## Modulation (Locked Decision 1: floor up, cautious down, fail-safe)

| Verdict | Effect |
|---|---|
| `high` | floor at `M`: `S`->`M`, `M`->`M`, `L`->`L`. Non-overridable downward, even over an explicit `S` (loud announce). Pins the floor so a re-init never regresses. |
| `low` and size **not** pinned | downgrade to `S` (do + PR). Suppressed when `downgrade: false` (safety-only mode). |
| `low` and size pinned | respected; an explicit operator size is never downgraded. |
| `unknown` / any error | no change; fail-safe to the operator/default size. |

The decision is always surfaced (a one-line `blast: ...` announce on stderr), never silent.

## Scope

Plan **and** node inputs (a File Ownership Map exists). `fno target init` resolves a node-id `--input` to its `plan_path` via an exact, format-agnostic graph id match (no fuzzy title guessing; a modifier-laden free-text input never mis-resolves and simply skips), so `/megawalk` and `/megatron` node walks inherit the modulation. Free-text `/target "feature"` keeps its operator/default ceremony until a surface is known. Folder plans read `00-INDEX.md`.

## Config (`config.target.blast.*`, default OFF)

| Key | Default | Effect |
|---|---|---|
| `enabled` | `false` | Whole-feature opt-in. A malformed block fails safe to disabled. |
| `downgrade` | `true` | When false, only the high-blast floor applies (safety-only). |
| `reuse_loc_manifest` | `true` | Include the loc-ratchet control-plane globs in the map. |
| `high_blast_globs` | `[]` | Per-project extension of the general list; a single bad glob is skipped, not raised. |

## Files

| File | Role |
|---|---|
| `cli/src/fno/target/blast.py` | classifier, blast-map loader, ownership-map parser |
| `cli/src/fno/target_cli.py` | `blast-check` verb + `_modulate_size` + init modulation + node resolution |
| `cli/src/fno/config/__init__.py` | `config.target.blast` schema (`BlastConfig`) |
| `skills/target/references/init-state.md` | Step 1c-blast operator documentation |
| `scripts/ci/loc-ratchet-manifest.yaml` | read-only blast-map source (not modified) |

## Rejected alternatives

- **LLM blast read at entry** - non-deterministic; footnote prefers gates over judgment for routing. Possible later as an escalate-only add-on (may raise to high, never lower).
- **Lightweight-by-default, upgrade for high-blast** - bigger token savings but fail-open: anything the classifier misses ships under-ceremonied.
- **A new blast-map config from scratch** - duplicates and drifts from the loc-ratchet-manifest curation; reuse keeps one source of truth.
