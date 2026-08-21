# fno project environments: which half is isolated

`fno project init <id>` gives a checkout its own fno state root. It isolates fno's data and it does not isolate the harness substrate. This page says where that line falls, because a receipt that blurs it has already cost real sessions.

## Two layers, one of them ours

**Layer 1 is fno's own data.** One key moves all of it. A project-local `<repo>/.fno/config.toml` setting `state_dir` deep-merges over the per-user global, winning that key while inheriting every other default, and every `paths.*` derives from `state_dir` (`cli/src/fno/paths.py`). The graph, the ledger, the briefs, the agent registry (`agents_registry_path`, whose fallback is `state_dir() / "agents" / "registry.json"`) and the mail bus all move together.

**Layer 2 is the harness substrate.** One claude daemon namespace for the whole box, plus codex app-servers owned by ChatGPT.app and a VS Code extension. fno never spawned those processes and cannot stop them. There is nothing for `state_dir` to move.

So an isolated environment is isolated about data and not about identity. A session started inside one shares the machine's daemon namespace with every other session on that machine, and an ambient identity marker crosses the boundary as if it were not there.

## Why the receipt says it out loud

Three failures on one machine in one day, all on the layer-2 vector:

- Claude sessions carried a real `CODEX_SESSION_ID` that nothing in the fleet had spawned. One of them was refused a crown grant because of it.
- A claim recorded a pid belonging to a desktop app's own codex process. Because that pid stayed alive, the claim read `state=live` for eleven hours past its expiry and fenced every merge in the meantime.
- A model override was inherited from a long-lived daemon, so a session ran on a model nobody had selected for it.

None of these is exotic. Each is an environment variable outliving the process that set it, which is what environment variables do. A receipt reading "isolated environment" would have been read as ruling all three out. `fno project init` prints what moved and what did not, so the next person reads a true sentence instead of a reassuring one.

The remedy for layer 2 is provenance, not isolation: `resolve_owned_identity` (`cli/src/fno/harness_identity.py`) refuses to guess when two harness families disagree, and every durable stamp site routes through it.

## The measured candidate order

Measured, not read off the schema: `config_read_candidates` returns

```
<repo>/.fno/config.toml
<repo>/.fno/settings.yaml
~/.fno/config.toml
~/.fno/settings.yaml
```

so a project-local `config.toml` wins per key and deep-merges over the global (`cli/src/fno/config_io.py`, `_prefer_toml`).

`FNO_CONFIG` is the wrong lever for this and `fno project init` does not use it. It is the ONLY candidate when set, so it discards the operator's global defaults rather than overriding one key.

## Where the root lives

`~/.fno/projects/<id>/`. Absolute, already inside the state root `fno config doctor` reports, and it needs no new rule. An in-repo root (`.fnostate/`) would need a gitignore entry and can be committed by accident, which puts a demo graph in a public history. A sibling of the repo has no discovery path.

## Its own mail bus, by default and on purpose

The bus derives from `state_dir`, so an isolated environment gets its own. That is right for a demo and wrong when the operator wants to message a demo worker from a real session. The receipt states it rather than leaving it inherited. There is no `--share-mail` escape until someone needs one.

## The two refusals

`fno project init` refuses rather than half-isolating:

- **A different `state_dir` already pinned** in this repo's `config.toml`. It prints the existing value and the file path and writes nothing. Overwriting would silently move a live environment's graph, ledger and mail bus.
- **`config.paths.agents_registry_path` set.** That explicit override is honored ahead of the `state_dir` fallback, so the environment would get its own graph and share the agent roster. The refusal names the key.

## Appendix: does `CLAUDE_CONFIG_DIR` give a distinct claude daemon namespace?

Not part of an fno project environment. This is an open question that got measured, recorded so the next person does not re-derive it, and it applies to an edge case: operators who keep two logged-in Claude Code accounts side by side and switch between them. Some people do run that way. It is not a step in setting up a project environment, and nothing below is something `fno project init` does.

**Yes.** Measured 2026-08-21.

The rule, which is what travels: the segment is the first eight hex characters of `sha256` over the absolute config-dir path. Verify it on any machine without launching anything:

```bash
ls /tmp/cc-daemon-$(id -u)/
python3 -c 'import hashlib,os,sys; print(hashlib.sha256(os.path.abspath(sys.argv[1]).encode()).hexdigest()[:8])' ~/.claude
```

The listed directory is that digest. Point `CLAUDE_CONFIG_DIR` at a second directory, start a background session (`claude --bg '<task>'`), and a second directory appears whose name is that second path's digest. That is the measurement: on the box this was run on, one entry before and two after, each matching its own digest.

So layer-2 isolation is partially achievable for claude: a distinct `CLAUDE_CONFIG_DIR` gets a distinct daemon namespace, distinct `bg-spare` processes, and a distinct control socket.

Three limits, so nobody reads that as more than it is.

`--print` never starts the daemon at all. A headless one-shot under a second config dir leaves the listing unchanged, which reads as a negative result and is not one. The instrument for this question is `claude --bg`, whose `bg-spare` processes allocate the directory.

The split is per config dir, not per fno environment. One config dir shared by five project environments still shares one daemon namespace, so this buys nothing on the ordinary path.

A second config dir is an account boundary, not a scratch directory: it is a second login to keep alive. That is a deliberate setup an operator chooses for their own reasons, and it stays theirs to turn on. `fno project init` does not set `CLAUDE_CONFIG_DIR`, does not read it, and does not claim what it would buy.

This changes nothing for codex under either outcome. Those processes belong to ChatGPT.app and a VS Code extension and are not fno's to namespace.
