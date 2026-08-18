# Mux selector resolution

The contract between the Python name minter and the Rust pane resolver. Two languages, no shared type: this file is the type. The registry row carries no node field, so the agent NAME is the only carrier of which node a worker serves - which makes the name format load-bearing in both directions.

## The name format

A node-driven spawn mints `t-<node-id>-<slug>-<model>` (prefix `t` for target workers):

- `<node-id>`: the `--node` value, normalized to the canonical id when a graph read resolves a slug.
- `<slug>`: `--slug` when given, else the node's graph slug, else omitted.
- `<model>`: `--model`/`-m` (or the model half of `--route <vendor>,<model>`), lowercased with every non-alphanumeric stripped: `glm-5.2` becomes `glm52`. Omitted when no model is visible on argv.

The mint routes through `agent_name` in `cli/src/fno/agents/naming.py` - the single owner of the 64-character daemon budget, where only the slug gives way. Any mint failure (graph read, budget, contract) falls back to the adjective-noun mint; a spawn never dies on a naming lookup. A caller-supplied `--name` always wins and no mint fires.

## The resolution tiers

`resolve_selector` in `crates/fno/src/mux_cli.rs` resolves one selector to one registry row. First non-empty tier wins:

1. **Exact**: the row's `name`, `session_id`, or `harness_session_id` equals the selector.
2. **Prefix**: `session_id` or `harness_session_id` starts with the selector.
3. **Substring over `name`, case-insensitive**: the tier that makes a node id and a slug resolve, because the name carries them.

## The refusal rule

Ambiguity inside the winning tier refuses; it never falls through to a looser tier and never picks a winner. Two rows are the SAME candidate when their effective identity (`session_id`, else `harness_session_id`) matches - a row listed twice is not an ambiguity. The refusal prints one line per candidate (name, pane ref, seconds since last activity) and exits `21` (`EXIT_AMBIGUOUS`), distinct from no-match's `16` (`EXIT_NOT_FOUND`), so a script can tell a typo from a family.

## The three doors

One resolver, three callers - a resolver wired into only one door would leave the others integer-only while the fix read as landed:

| Door | Behavior |
|------|----------|
| `fno mux view <selector>` | Resolve, then focus the hosting session's pane. `--url` prints the web-bridge URL for that pane instead; `--fzf` opens the interactive picker. |
| `fno mux pane focus <target>` | All digits: the legacy pane-id door, unchanged. Anything else: the same resolution as `view`. `--fzf` (no argument) opens the picker. |
| `fno mux where <fno_id>` | The same tiers; reports the location rather than focusing. |

A row that resolves but hosts no pane (`mux == None`) is never an error to swallow and never an attach: attaching creates a pane, and the server's inherited fd limit makes that a side effect that can wedge a wave. The doors print the follow command (`fno agents peek <name> --follow`) and exit `17` (`EXIT_NOT_PANE_HOSTED`).

Selector focus ignores `FNO_SESSION` - that names the session you sit in, not the one the target pane lives in. An explicit `--session` still overrides, as it does for `where`.

## The web-bridge state file

`fno mux serve --web` prints its URL and token once at bind. It also writes `~/.fno/mux/web-<session>.json` (`{"bind", "port", "token"}`, mode 0600) at bind and removes it on exit. `mux view <selector> --url` reads it, probes the TCP port before trusting it (a file left by a killed bridge reads as "no bridge"), and prints `http://<host>:<port>/?t=<token>&pane=<pane>`. The served page reads `pane` from its query string and pins the first paint to that pane, clearing the want once honoured so the operator can still switch panes by hand.
