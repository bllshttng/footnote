# Research layer: retrieve + store (Group 1)

`fno research "X"` is the research-pipeline counterpart to `fno target`: where target runs a code task to a green PR, research runs a topic to a cited, eval-checkable evidence store. This doc covers **Group 1** - the retrieve + store foundation. The `doc` deliverable terminal, the advisory verify profile, and the `fno evals` research golden-task are **Group 2** (blocked on this group).

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
