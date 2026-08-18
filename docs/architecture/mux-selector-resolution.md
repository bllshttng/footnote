# Mux selector resolution

The contract between the Python name minter and the Rust pane resolver. Two languages, no shared type: this file is the type. The registry row carries no node field. The agent NAME is the only carrier of which node a worker serves. That makes the name format load-bearing in both directions.

## The name format

A node-driven spawn mints `t-<node-id>-<slug>-<model>`. The prefix `t` marks target workers.

- `<node-id>`: the `--node` value. A graph read normalizes a slug input to the canonical id.
- `<slug>`: given `--slug`, use it. Else use the node's graph slug. Else omit it.
- `<model>`: `--model`/`-m`, or the model half of `--route <vendor>,<model>`. Lowercase it and strip every non-alphanumeric: `glm-5.2` becomes `glm52`. When no model is visible on argv, omit the tag.

The mint routes through `agent_name` in `cli/src/fno/agents/naming.py`. That function is the single owner of the 64-character daemon budget, and only the slug gives way. Any mint failure (graph read, budget, contract) falls back to the adjective-noun mint. A spawn never dies on a naming lookup. A caller-supplied `--name` always wins and no mint fires.

## The resolution tiers

`resolve_selector` in `crates/fno/src/mux_cli.rs` resolves one selector to one registry row. The first non-empty tier wins.

1. **Exact**: the row's `name`, `session_id`, or `harness_session_id` equals the selector.
2. **Prefix**: `session_id` or `harness_session_id` starts with the selector.
3. **Substring over `name`, case-insensitive**: the tier where a node id and a slug resolve. The name carries them.

## The refusal rule

Ambiguity inside the winning tier refuses. It never falls through to a looser tier and never picks a winner. Rows whose effective identity (`session_id`, else `harness_session_id`) matches are the SAME candidate. A row listed twice is not an ambiguity.

The refusal prints one line per candidate: name, pane ref, seconds since last activity. It exits `21` (`EXIT_AMBIGUOUS`). No-match exits `16` (`EXIT_NOT_FOUND`). A script can tell a typo from a family.

## The three doors

One resolver, three callers. A resolver wired into only one door leaves the others integer-only while the fix reads as landed.

| Door | Behavior |
|------|----------|
| `fno mux view <selector>` | Resolve, then focus the hosting session's pane. `--url` prints the web-bridge URL for that pane instead. `--fzf` opens the interactive picker. |
| `fno mux pane focus <target>` | All digits: the legacy pane-id door, unchanged. Anything else: the same resolution as `view`. `--fzf` (no argument) opens the picker. |
| `fno mux where <fno_id>` | The same tiers. It reports the location rather than focusing. |

A row that resolves but hosts no pane (`mux == None`) never attaches. Attaching creates a pane, and the server's inherited fd limit makes that a wave-wedging side effect. The doors print the follow command (`fno agents peek <name> --follow`) and exit `17` (`EXIT_NOT_PANE_HOSTED`).

Selector focus ignores `FNO_SESSION`. That variable names the session you sit in, not the one the target pane lives in. An explicit `--session` still overrides, as it does for `where`.

## The web-bridge state file

`fno mux serve --web` prints its URL and token once at bind. It also writes `~/.fno/mux/web-<session>.json` at bind: `{"bind", "port", "token", "pid"}`, mode 0600. The bridge removes the file on exit, including Ctrl-C via graceful shutdown. Removal is pid-guarded: a newer bridge for the same session owns the file, and an older bridge's exit must not delete it.

`mux view <selector> --url` reads that file and probes the TCP port before trusting it. The probe resolves any bind spelling, `localhost` and `::1` included. A file left by a killed bridge reads as "no bridge". The command prints `http://<host>:<port>/?t=<token>&pane=<pane>` with a bracketed IPv6 host. The served page reads `pane` from its query string and pins the first paint to that pane. Other panes' frames keep waiting while the wanted pane is outstanding. It clears the want once honoured, so the operator can still switch panes by hand.
