# Footnote on OpenCode

Footnote installs into opencode as a first-class harness: its own commands, agents, skills and hooks, with no third-party orchestrator in the loop. This guide covers what the installer writes, what it asks before touching, and what it refuses to touch. `fno doctor` answers the one-line "what state am I in" question at every step.

## Before you start

Three reads tell you where you stand:

1. `opencode --version` - the installed opencode. The installer renders agent files for the contract it reports (see [OpenCode 2.x](#opencode-2x-differences)).
2. `npm ls -g oh-my-openagent oh-my-opencode` - whether oh-my-openagent (omo) is installed as a package. Footnote never uninstalls it. The installer only asks to stop loading it.
3. `fno doctor` - the current install state, one line per finding.

## Install

```bash
fno config plugin install opencode
```

The installer does things in this order:

1. Resolves a footnote source tree: env hints, then the `~/.fno/install/plugin-root` pointer, then the repository around the current directory.
2. Reads `opencode --version` once and classifies the contract (1.x or 2.x).
3. Writes the file surface into the config dir (below).
4. Audits every config file opencode reads plugins from, and prints one summary line per finding. Then it asks once: `Disable these plugin entries? [Y/n]`. A yes disables each oh-my-openagent and stranger entry with a timestamped backup and a printed undo line. A no, a missing terminal, or `--json` writes nothing and prints the re-run command.

Flags:

- `--dry-run` - prints the audit summary and the omo config report, writes nothing, and skips the file install. Pure preview.
- `--yes` - answers the disable prompt without asking. Combinable with `--dry-run` for a scripted preview.
- `--json` - one JSON receipt on stdout, prose side lines on stderr. It never prompts and never edits config files. The receipt carries `plugin_array` for a caller that wants to decide itself.

## What opencode reads, in what order

Later keys win. Objects merge. When it audits, footnote reads all of these, and edits only the global and pointer files:

1. Remote `.well-known/opencode` (opencode 1.x).
2. The global dir, from `XDG_CONFIG_HOME` or `~/.config/opencode` by default: `config.json`, then `opencode.json`, then `opencode.jsonc`, then `tui.json` (TUI plugins).
3. `$OPENCODE_CONFIG`, a single file.
4. `~/.opencode/opencode.json[c]`, present or not.
5. Project configs, walking up from the working directory to the git root: `opencode.json[c]` in each directory, plus `.opencode/opencode.json[c]`.
6. `$OPENCODE_CONFIG_DIR`, an extra directory loaded last.

Local plugins auto-load from `~/.config/opencode/plugins/` and `.opencode/plugins/`. npm is optional, and footnote does not use it.

## The plugin array entry fno writes: none

Footnote loads as a local plugin file, `~/.config/opencode/plugins/footnote.js`. No npm package, no `plugin` array entry. This is deliberate. Both unscoped npm names exist and belong to other authors: `fno` is volkovasystems' Function wrapper, `footnote` is an annotation library. A config listing a bare `"fno"` or `"footnote"` spec makes opencode install that stranger's package into every session. The installer flags it and offers to remove it.

## What fno writes, and why

| File | Why |
|---|---|
| `plugins/footnote.js` | The bridge: footnote's hook host. It runs hooks.json through opencode's plugin events (guards on tool calls, prompt injections, compaction context, the identity stamp), and drives the target loop's completion gate on idle. |
| `commands/fno:<verb>.md` | One per shipped verb, so `/fno:target` and friends exist in every project. |
| `agents/fno:<name>.md` | One per shipped agent, permissions rendered for the contract opencode reports. |
| `agents/fno.md` | The `fno` primary agent: footnote's orchestrator, Tab-selectable beside opencode's build and plan. |
| `skills/<name>/**` | The skill trees the commands and agents load. |
| `~/.fno/opencode-install-<hash>.json` | The manifest: every written path and its content hash. The basis for honest upgrades and uninstall. |
| `<file>.fno-backup-<timestamp>` | Any config file the disable step edits, backed up first, plus the replaced legacy bridge. |

`fno:<verb>` and `fno:<name>` are namespaced on purpose. They coexist with everything else opencode loads. A bare `target` command name is a collision with other plugins.

## Idempotent re-install

Run the install again at any time. Files whose bytes match the manifest are skipped untouched, with no mtime churn. Files footnote shipped but no longer writes are removed. If their bytes still match the hash recorded at install, they go. If they do not, the file is yours, and it stays. A file you edited is kept and named. A pre-manifest `footnote.js` whose first line is the shipped bridge header is footnote's own legacy install: backed up, replaced, and named in the receipt. Any other file you wrote is kept, named, and reported as `partial`.

## What fno refuses, and why

- A file it did not write, whose bytes differ: it is yours. Footnote keeps it and names it. The 42-directory hazard - one installer assuming it owns a whole config tree - is the failure this prevents.
- A project config file: project `opencode.json[c]` files are reported, never edited. What applies to every project belongs in the global config. Footnote edits only there, plus `tui.json` and `$OPENCODE_CONFIG`.
- A config file whose edit breaks parsing: the disable step re-parses before writing and refuses the write, naming the file.
- Uninstalling any package: the disable step stops omo from loading. It never runs `npm uninstall`. The receipt prints omo's own remove command as information only.
- Anything under `~/.omo`: omo's own config is read, read-only, for the model carry-over hint. Footnote never writes there.

## oh-my-openagent, 4.19.x and 5.x

omo 5.x moved its config to `~/.omo/omo.jsonc`. A migration run leaves the legacy `oh-my-openagent.json` in a `~/.omo/migration-backup-<timestamp>-opencode-config/` directory. A 4.19.x plugin installed after that move runs on its defaults. Once the omo plugin entry is disabled, none of this matters. The package stops loading and its config stays where it is. The install reports it with an INFO line ("inert while no omo plugin loads"). `~/.omo/codegraph` indexes are untouched, and the `codegraph` CLI is independent of omo.

The `plugin` array is not the only place omo registers: `tui.json` can list it as a TUI plugin. The audit covers `tui.json` too.

## OpenCode 2.x differences

The installed opencode decides what the installer renders, and the manifest records which contract it rendered for:

- The plugin key is `plugins` (plural). Entries can be `{"package": ..., "options": ...}` objects, and a `-<id>` entry disables a plugin. `opencode plugin add|list|remove` manages package plugins.
- Agent permissions render as a `permissions` rule list (`action`/`resource`/`effect`). Version 1.x renders a `permission` map.
- The shell tool is `shell` and delegation is `subagent`. Version 1.x names them `bash` and `task`.
- Version 2.x has no `shell.env` seam, so the session identity stamp is 1.x-only. The bridge names this on stderr under 2.x.

An opencode upgrade crossing 2.0.0 after an install reads `stale` in `fno doctor`: "agents were rendered for opencode 1.x, opencode now reports 2.x". The fix is one re-install, which re-renders the agent files.

## What you get: agents and models

- One `fno` primary agent (the orchestrator), Tab-selectable in every project.
- Eighteen `fno:<name>` subagents: archer, scout, architect, verifier, code-reviewer, and the rest. Restrictions render as opencode permissions. A tool allowlist becomes a deny-all record with the named allows, so an allowlisted agent never installs unrestricted.

Models are opencode's own mechanism: assign one per agent in `opencode.json`, and it decides.

```json
{
  "agent": {
    "fno:archer": { "model": "zai-coding-plan/glm-5.3" }
  }
}
```

Agent files ship no bare model aliases, so an `agent.<name>.model` entry governs. An unassigned agent runs on the caller's model. When omo's per-agent assignments exist in `~/.omo/omo.jsonc`, the install summary prints a suggested `agent` block mapping them onto footnote's agents. Sisyphus maps to `fno`, hephaestus to `fno:archer`, oracle to `fno:architect`, and so on. It also names the omo agents nothing carries: atlas, multimodal-looker, frontend-ui-ux-engineer, document-writer. It prints, and you copy what you want.

## Hooks: what runs where

The bridge runs footnote's hooks.json, the same scripts claude and codex run, through opencode's plugin events. Session start and prompt injections feed the system prompt. Guards run before tools and can deny. Post-tool hooks run fail-open. Compaction context rides `experimental.session.compacting`. The idle event drives the target loop's completion gate, the same `fno-agents loop-check` claude uses.

Gap rows, named honestly: claude's SessionEnd, StopFailure, Notification, SubagentStart and SubagentStop have no opencode equivalent the bridge can drive today. Either opencode has no matching event, or the script reads a claude transcript. Everything else has a mapping, and `docs/architecture/hook-budget-audit.md` counts them.

## Whoami and intel

`fno whoami` inside opencode reads `harness: opencode`: the bridge stamps `OPENCODE_SESSION_ID` into every shell environment, and the harness resolver reads it. When opencode is the nearest harness ancestor, a session spawned from a claude process still reads opencode. The process-tree walk decides, not the environment.

`fno-agents intel -H opencode` folds opencode sessions into the fleet report. The default scope is the current project. Add `--all-projects` to read the whole machine.

## One doctor line per state

- Not installed: `INFO opencode: footnote is not installed; run fno config plugin install opencode`.
- Legacy bridge only: `WARN opencode: only the pre-manifest stop bridge is installed (...)`. The install replaces it and keeps a backup.
- Installed: `OK opencode: footnote <v> installed: <n> commands, <n> agents, <n> skills, all loaded`.
- Stale, from a newer source or a changed contract: `WARN opencode: installed at footnote <v>, source is <v>; re-run ...`.
- Partial: `WARN opencode: installed but not loaded: <names>`, plus kept user files by path.
- Drifted: `WARN opencode: <path> changed since install (digest differs); re-run the install to restore it, or keep your edit`.
- omo still loaded: `WARN opencode: oh-my-openagent is still in the plugin array of <file> ...`.
- Stranger spec: `WARN opencode: <file> lists "fno", an unrelated npm package (not footnote) ...`.
- omo config left: `INFO opencode: ~/.omo/omo.jsonc keeps oh-my-openagent's settings; inert while no omo plugin loads, footnote never edits it`.

## Uninstall

```bash
fno-agents plugin-install opencode --uninstall
```

Removes exactly the manifest's paths, and only those whose bytes still match the recorded hash. A file you edited is kept and named. Each disable edit printed an undo line at the moment it ran. `undo: cp '<backup>' '<file>'` restores any config file from its timestamped backup.
