# fno project environments: which half is isolated

`fno project init <id>` gives a checkout its own fno state root. It isolates fno's data. It does not isolate the harness substrate. This page says where that line falls, because a receipt that blurs it has already cost real sessions.

## Slicing is not isolating

Two different asks get confused here. They need opposite mechanisms.

**Slice**: the same graph, less noise. A filter. Reversible, no new state.

**Isolate**: a different graph. A new state root. Not a smaller view of the same data.

Most people asking for "a clean instance" want the slice. The graph is one store and every node carries a `project` tag. Filtering by that tag is the whole fix, and it works on the real graph with no new state root.

`config.mux.board_scope` picks the slice:

| Value | Board shows |
|---|---|
| `repo` (default) | this checkout's `project.id`, plus unscoped cards |
| `workspace:<name>` | every project `work.workspaces.<name>` declares, plus unscoped |
| `all` | every project (the historical board) |

`repo` needs no configuration. `project.id` resolves through git, so every worktree layout of a checkout answers with the same project.

`workspace:<name>` reuses the project set you already declared. A workspace named `main` holding `web`, `backend` and `marketing` scopes the board to those three:

```toml
[[work.workspaces.main.projects]]
name = "web"
path = "~/code/web"

[[work.workspaces.main.projects]]
name = "backend"
path = "~/code/backend"

[[work.workspaces.main.projects]]
name = "marketing"
path = "~/code/marketing"
```

```toml
[mux]
board_scope = "workspace:main"
```

A scope that cannot resolve does not narrow the board. It falls back to every project. That is the historical board, and what an operator who configured nothing already has. Narrowing there is worse. Every node in the graph carries a project tag. An unresolved scope then leaves a board of the few unscoped cards, with no visible cause. So the fallback is loud, not silent. `fno mux doctor` reports it as a `warn` with the remedy. The server logs the same line at startup. Both name the board they produce, not the one they wanted.

Cards with no project are never hidden under any scope. A node with no project is not another project's work. Filtering it out makes work disappear rather than filter it.

When the mux server starts, it resolves the scope once. `fno mux doctor` reports what it resolved. When it refused, that report carries a remedy. To change it, run `fno mux kill-server` and reattach.

The rest of this page is about the other half.

## Two layers, one of them ours

**Layer 1 is fno's own data.** One key moves all of it. A project-local `<repo>/.fno/config.toml` setting `state_dir` deep-merges over the per-user global, winning that key while inheriting every other default. Every `paths.*` derives from `state_dir` (`cli/src/fno/paths.py`). The graph, the ledger, the briefs, the agent registry (`agents_registry_path`, whose fallback is `state_dir() / "agents" / "registry.json"`) and the mail bus all move together.

**Layer 2 is the harness substrate.** One claude daemon namespace for the whole box, plus codex app-servers owned by ChatGPT.app and a VS Code extension. fno never spawned those processes and cannot stop them. There is nothing for `state_dir` to move.

So an isolated environment is isolated about data and not about identity. A session started inside one shares the machine's daemon namespace with every other session on that machine. An ambient identity marker crosses the boundary untouched.

## Why the receipt says it out loud

Three failures on one machine in one day, all on the layer-2 vector:

- Claude sessions carried a real `CODEX_SESSION_ID` that nothing in the fleet had spawned. One of them was refused a crown grant because of it.
- A claim recorded a pid belonging to a desktop app's own codex process. That pid stayed alive, so the claim read `state=live` for eleven hours past its expiry. It fenced every merge in the meantime.
- A model override was inherited from a long-lived daemon. A session then ran on a model nobody had selected for it.

None of these is exotic. Each is an environment variable outliving the process that set it, which is what environment variables do. A receipt reading "isolated environment" rules all three out to anyone who reads it. `fno project init` prints what moved and what did not, so the next person reads a true sentence instead of a reassuring one.

The remedy for layer 2 is provenance, not isolation. `resolve_owned_identity` (`cli/src/fno/harness_identity.py`) proves ownership from the process tree instead of picking by precedence order. When it cannot prove one, it refuses rather than guessing. Durable stamp sites reach it through `resolve_self_identity` (`cli/src/fno/claims/self_identity.py`). That function supplies the process-tree prover and nothing else. The registry collider stays at the one init-time verb that owns a registry row. Hoisting it into the shared resolver broke that: a session then refused its own row whenever the walk failed.

The discriminator is the raw primitive's own docstring. If the resolved harness or session id ends up WRITTEN to a durable record, it is a stamp and uses the owned path. Those records: a claim, a mail record, an event, an agent-state row, a registry row, a crown grant, a decision record, a graph session record. A caller that only reads to display or branch keeps the precedence primitive.

That rule was documented long before anything enforced it, which is how the caller set drifted from two obeying it to dozens not. `scripts/ci/check-identity-stamp-sites.sh` is the enforcement. Every remaining precedence-primitive caller is listed in a baseline with the reason it is read-only. A new one fails CI until someone makes that call and writes it down.

## The measured candidate order

Measured, not read off the schema: `config_read_candidates` returns

```
<repo>/.fno/config.toml
<repo>/.fno/settings.yaml
~/.fno/config.toml
~/.fno/settings.yaml
```

so a project-local `config.toml` wins per key and deep-merges over the global (`cli/src/fno/config_io.py`, `_prefer_toml`).

`FNO_CONFIG` is the wrong lever for this, and `fno project init` does not use it. When set, it is the ONLY candidate. It discards the operator's global defaults rather than overriding one key.

## Where the root lives

`~/.fno/projects/<id>/`. Absolute, already inside the state root `fno config doctor` reports, and it needs no new rule. An in-repo root (`.fnostate/`) needs a gitignore entry and can be committed by accident, which puts a demo graph in a public history. A sibling of the repo has no discovery path.

## Its own mail bus, by default and on purpose

The bus derives from `state_dir`, so an isolated environment gets its own. That is right for a demo. When the operator wants to message a demo worker from a real session, it is wrong. The receipt states it rather than leaving it inherited. There is no `--share-mail` escape until someone needs one.

## The two refusals

`fno project init` refuses rather than half-isolating:

- **A different `state_dir` already pinned** in this repo's `config.toml`. It prints the existing value and the file path, and writes nothing. Overwriting silently moves a live environment's graph, ledger and mail bus.
- **`config.paths.agents_registry_path` set.** That explicit override is honored ahead of the `state_dir` fallback. The environment then gets its own graph and shares the agent roster. The refusal names the key.

## Appendix: does `CLAUDE_CONFIG_DIR` give a distinct claude daemon namespace?

Not part of an fno project environment. This is an open question that got measured, recorded here so the next person does not re-derive it. It applies to one edge case: operators who keep two logged-in Claude Code accounts side by side and switch between them. Some people do run that way. It is not a step in setting up a project environment, and nothing below is something `fno project init` does.

**Yes.** Measured 2026-08-21.

The rule is what travels. The segment is the first eight hex characters of `sha256` over the absolute config-dir path. Verify it on any machine without launching anything:

```bash
ls /tmp/cc-daemon-$(id -u)/
python3 -c 'import hashlib,os,sys; print(hashlib.sha256(os.path.abspath(sys.argv[1]).encode()).hexdigest()[:8])' ~/.claude
```

The listed directory is that digest. Point `CLAUDE_CONFIG_DIR` at a second directory and start a background session (`claude --bg '<task>'`). A second directory appears, named for that second path's digest. That is the measurement: on the box this ran on, one entry before and two after, each matching its own digest.

So layer-2 isolation is partially achievable for claude. A distinct `CLAUDE_CONFIG_DIR` gets a distinct daemon namespace, distinct `bg-spare` processes, and a distinct control socket.

Three limits, so nobody reads that as more than it is.

`--print` never starts the daemon at all. A headless one-shot under a second config dir leaves the listing unchanged, which reads as a negative result and is not one. The instrument for this question is `claude --bg`, whose `bg-spare` processes allocate the directory.

The split is per config dir, not per fno environment. One config dir shared by five project environments still shares one daemon namespace, so this buys nothing on the ordinary path.

A second config dir is an account boundary, not a scratch directory. It is a second login to keep alive. That is a deliberate setup an operator chooses for their own reasons, and it stays theirs to turn on. `fno project init` does not set `CLAUDE_CONFIG_DIR`, does not read it, and claims nothing about it.

This changes nothing for codex under either outcome. Those processes belong to ChatGPT.app and a VS Code extension. They are not fno's to namespace.
