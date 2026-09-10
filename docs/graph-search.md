# Graph search

Optional. Nothing in footnote depends on it, and a checkout without it works the same.

The tool is `graphify`. It builds a knowledge graph of the repo and answers a where-does-this-live question with a scoped subgraph instead of a repo-wide grep. Use it when a question spans several files and you do not yet know which ones.

## Is it here

Only when `graphify-out/graph.json` exists in the repo root. If it does not, skip this page and use `rg` / Grep.

The index is generated and local, so a dirty index is expected after a hook run or an incremental update, and it is never a reason to skip the tool. Keep `graphify-out/` out of commits: `graph.json` runs to tens of megabytes, and one `graphify-out/` under `crates/` makes the rust build report a dirty tree.

## The four verbs

```
graphify query "<question>"     # scoped subgraph for a question
graphify path "<A>" "<B>"       # shortest path between two symbols
graphify explain "<concept>"    # one node and its neighbors, in plain language
graphify update .               # re-extract after changing code (AST only, no API cost)
```

`graphify-out/GRAPH_REPORT.md` is a whole-repo architecture summary. Read it only for a broad pass. For a specific question the three read verbs return far less text.

## What it does not replace

The search convention in [AGENTS.md](../AGENTS.md#conventions) still governs. A load-bearing sweep, one whose zero you intend to trust, uses `RIPGREP_CONFIG_PATH= rg -uu` and never a graph query, because the graph is a snapshot and can be stale by exactly the change you are looking for.
