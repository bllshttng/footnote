# Research layer: retrieve + store + ship

`fno research "X"` is the research-pipeline counterpart to `fno target`: where target runs a code task to a green PR, research runs a topic to a cited, eval-checkable brief. **Group 1** is the retrieve + store foundation (below). **Group 2** is ship + grade: the `doc` deliverable, the advisory research-verify profile, and the `fno evals grade` scorer ([jump](#group-2-ship--grade)).

## The one retrieval path

There is a single retrieval backbone, not a fallback chain:

```
ddgs (search) -> self-fetch each URL -> one sources.jsonl row per source
```

`ddgs` (the `duckduckgo-search` CLI) is the floor because it is free, needs no API key, behaves identically on every host CLI (Claude Code / Codex / Gemini), and returns clean URLs. Native-provider websearch enrichment (Claude `web_search`, Gemini grounding) is a Group-2 concern, layered on top only when ddgs breadth is thin - it never replaces the backbone.

Because the backbone **self-fetches**, provenance is clean from round one: the agent controls `url + hash + extract` itself, so no citation-shape normalizer is needed on the primary path.

## Components

| Piece | Location | Role |
|-------|----------|------|
| `fno research "X"` | `cli/src/fno/research/cli.py` | CLI entry point. Validates the topic, runs one retrieve+store round, prints a summary. |
| retrieval engine | `cli/src/fno/research/core.py` | ddgs search, SSRF-guarded self-fetch, `sources.jsonl` read/write, per-topic claim. stdlib-only. |
| `scout` | `agents/scout.md` | The `research` executor subagent. Retrieves through the deterministic backbone (not WebSearch/WebFetch) and treats fetched content as data. |
| `research` executor | `skills/do/references/executor-resolution.md` | Registry row. Reached via `fno research`, **not** `/do waves` surface inference. |

## The evidence store: `sources.jsonl`

One JSON line per source at `~/.fno/notes/research/<slug>.sources.jsonl`. The schema is the Group-2 eval's contract:

```json
{"url": "...", "fetched_at": "ISO-8601", "hash": "sha256", "extract": "text...", "verified": true, "reason": ""}
```

- `verified` is `true` **only** after a successful text fetch produced a content `hash`.
- A non-text (PDF/image), 404, timeout, or SSRF-blocked source is recorded `verified=false` with a `reason` and never aborts the round (Group 2's dead-URL assertion catches it).
- Line-append is the write unit; a single writer is guaranteed per topic by a `fno claim` on `node:research:<slug>`.

## Boundaries (Group 1)

- Empty or one-word topics are refused before a round is spent (exit 2).
- `ddgs` missing or rate-limited fails loud with an install hint (exit 3), never silent-empty.
- A topic already held by another writer refuses without writing (exit 4).
- Zero search results stamp "no sources found" and write an empty file (no crash).

## Security: SSRF boundary

The self-fetch is the first fno code path that fetches **untrusted, attacker-influenced** URLs (search results and their redirects). `_guard_url` rejects non-`http(s)` schemes and any host resolving to a private / loopback / link-local / reserved / multicast / unspecified address, and `_GuardedRedirectHandler` re-validates every redirect hop - so a result page cannot `30x`-redirect the fetch into cloud metadata (`169.254.169.254`) or an internal service. A DNS-rebind TOCTOU remains (the guard resolves, urllib re-resolves at connect); closing it fully means pinning the connection to the vetted IP, deferred until rebinding is a real threat.

Separately, fetched page text is treated as **data, never instructions**: nothing interpolates an extract into a prompt; `scout` is told to treat retrieved content as quoted evidence only (prompt-injection boundary).

---

## Group 2: ship + grade

Group 1 leaves a cache `sources.jsonl`. Group 2 turns it into a *deliverable* and gives research the external truth code-ship always had (PR + CI + bot): a mechanical eval.

### The `doc` deliverable (US3)

By default `fno research "X"` now ships, after retrieval, a `doc`:

```
~/.fno/notes/research/<slug>.sources.jsonl   (cache, Group 1)
        │  ship step (cli/src/fno/research/deliverable.py)
        ▼
<output_dir>/<slug>.md               (the brief)
<output_dir>/<slug>.sources.jsonl    (cited-evidence sidecar)
```

- **Landing path** is `config.research.output_dir`. It is a vault area (not repo-relative, so absolute / `~` are allowed) and is **fail-loud when unset** (exit 5) - the ship step never guesses, the `parking_lot_path` lesson.
- **The brief** carries frontmatter (`topic`, `slug`, `stopped`, `sources`, `found`, `verified`) and a `## Findings` section with one cited claim per verified source: `- <snippet> [S1]`, resolved in a `## Sources` block (`[S1]: <url>`). The sidecar holds the *cited evidence* - the verified rows that back claims.
- **Why it stopped is always stamped** (`stopped: declared` vs `stopped: cap N`): a brief truncated at the round cap says so, never silently (AC4).
- A no-sources round still ships a stamped brief with a `## Findings` section ("no sources found"), not a crash (AC3).
- The terminal is `DoneAdvisory` (the non-PR completion state); a standalone run best-effort emits a `termination` event mirroring the loop.

`--no-deliver` keeps the Group-1 retrieve-only behavior (cache write only, no output_dir needed).

### The eval: research's "CI green" (US5)

`fno evals grade --brief <slug>.md --golden <discovery-*.md>` is the gate - **three mechanical assertions, no model in the loop** (`cli/src/fno/evals/research_grade.py`):

1. **Zero uncited claims** - every claim (a list item under a content section) cites a `[Sn]` that resolves to a URL present as a `sources.jsonl` row.
2. **Zero dead URLs** - every sidecar row is `verified` after a self-fetch, or is a `web.archive.org` (Wayback) URL.
3. **≥1 golden checklist item per section** - the golden `discovery-*.md`'s headings are the checklist; each brief content section must cover one (normalized-substring match). A brief with no content sections covers nothing → red, never vacuously green.

Green only if all three pass (exit 0 green / 1 red / 2 setup-error). The golden set is the hand-rolled `discovery-*.md` files.

### The research-verify profile (US4, advisory)

`/review research <brief.md>` runs a claim-shaped panel - fact-checker / citation-auditor / contradiction-finder / completeness-critic (`skills/review/research-verify.md`) - reusing the sigma panel machine with a swapped roster. It is **advisory**: it annotates the brief; the green/red verdict belongs to `fno evals grade` and this panel never changes it (mirroring sigma-is-advice / PR-CI-bot-is-the-code-gate).

### Scope note

Group 2 implements the deliverable, eval, and verify profile as **Python + skill** concerns layered on the existing seams (config block, review router, `fno evals` namespace). The heavier "alias over `target` with a Rust `finalize.rs` deliverable-strategy registry" the design sketches is not required to satisfy the MVP acceptance criteria and is deferred.
