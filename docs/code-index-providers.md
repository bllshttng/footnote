# Code index providers

## Is this page for you?

You want a blueprint to ask a local code index before it designs. You also want to add or override a provider of your own. This page is the provider contract. It defines one TOML file per index, every key, the file locations, and how the reader treats a missing, stale or broken index.

Not for: installing or running an indexer. footnote ships no indexer. You bring codegraph, graphify or your own tool, and fno only asks it. Copying an index into a fresh worktree is `scripts/setup/setup-worktree.sh` (`provision_graphify`, `provision_codegraph`). Why an index answer never proves absence: [graph-search.md](graph-search.md).

## The contract

A provider is one TOML file. Every key the detection reader consumes is one line:

```toml
schema = 1
name = "codegraph"                  # ^[a-z0-9][a-z0-9-]*$; a later directory's file with the same name wins
roles = ["symbol"]                  # open vocabulary; v1 uses symbol (symbols, verbs, flags, paths) and semantic (docs, prior art)
detect = ".codegraph"               # repo-relative path; present means the index exists
requires = "codegraph"              # executable on PATH; missing means present but unavailable
ask = ["codegraph", "query", "--json", "-l", "5", "{term}"]
timeout_secs = 10
fresh = ["codegraph", "status"]     # optional freshness probe
fresh_match = "Index is up to date" # optional; stdout must contain it, else fresh = no
refresh = ["codegraph", "sync"]     # documented for users; fno never runs it
```

`roles` declares the questions a provider answers. The planner asks by role and takes the first `ready` provider per role, so two symbol providers do not both get asked. `ask` is one argv. `{term}` is replaced with the question. `fresh` runs the optional probe and matches `fresh_match` in stdout. A match reads `yes`, anything else reads `no`, and no probe reads `unknown`.

A provider that errors, times out or is missing never blocks a plan. The plan records `status: unavailable` or `status: error` and continues. Detection itself never runs `ask`, `fresh` or `refresh`.

## Where the files live

Detection reads three directories in order, and a later file with the same `name` replaces an earlier one:

1. the bundled `code-index/providers/` in the blueprint skill
2. `~/.fno/code-index/providers/`
3. `<repo>/.fno/code-index/providers/`

Drop a file in one of the last two to add or override a provider. No config key, no verb, no restart. The next detection reads it.

`refresh` is documentation for humans. fno never runs it. Keeping the index current is the provider's job and yours. Run `codegraph sync`, `graphify update .`, or install graphify's git hook. A plan states the freshness it read. A stale `absent` verdict is confirmed with a plain search before it is trusted.
