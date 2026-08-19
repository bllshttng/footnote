# Recording scripts

## Why these exist

A recording script is the exact terminal run, expected output, and edit list for one lesson. The maintainer recording the asset reads one file from top to bottom instead of improvising against a live installation.

## The medium table

| lesson | title | medium | file | status |
|--------|-------|--------|------|--------|
| L01 | Install and prove it | cast | `L01-install-and-prove-it.md` | scripted |
| L02 | Your first shipped PR | video | `L02-your-first-shipped-pr.md` | scripted |
| L03 | How done is decided | video | `L03-how-done-is-decided.md` | scripted |
| L04 | Design before code | video | `L04-design-before-code.md` | scripted |
| L06 | Execute a plan | cast | `L06-execute-a-plan.md` | scripted |
| L07 | Review before you ship | video | `L07-review-before-you-ship.md` | scripted |
| L08 | The PR lifecycle | cast | `L08-the-pr-lifecycle.md` | scripted |
| L10 | Capture and shape work | cast | `L10-capture-and-shape-work.md` | scripted |
| L12 | Worktrees | cast | `L12-worktrees.md` | scripted |
| L13 | Spawn a peer agent | video | `L13-spawn-a-peer-agent.md` | scripted |
| L14 | Make agents talk | cast | `L14-make-agents-talk.md` | scripted |
| L15 | Mux | cast | `L15-mux.md` | scripted |

## Setup state

Run this block before every recording except L01 and L03. L01 records installation itself. L03 continues from L02 without replacing its target worktree.

```run
export DEMO_ROOT=/Users/Shared/footnote-recording-demo
set -euo pipefail
if [ -e "$DEMO_ROOT" ]; then mv "$DEMO_ROOT" "${DEMO_ROOT}.previous.$(date +%Y%m%d%H%M%S)-$$"; fi
mkdir -p "$DEMO_ROOT/state" "$DEMO_ROOT/plans"
touch "$DEMO_ROOT/state/.path-migration-done"
cat > "$DEMO_ROOT/config.toml" <<'TOML'
state_dir = "/Users/Shared/footnote-recording-demo/state"
plans_dir = "/Users/Shared/footnote-recording-demo/plans"

[backlog]
id_prefix = "demo"
id_hex_width = 4
TOML
export FNO_CONFIG="$DEMO_ROOT/config.toml"
test -d "$DEMO_ROOT/repo/.git" || git clone --quiet https://github.com/bllshttng/footnote.git "$DEMO_ROOT/repo"
git -C "$DEMO_ROOT/repo" checkout --quiet main
git -C "$DEMO_ROOT/repo" pull --ff-only --quiet
test "$(git -C "$DEMO_ROOT/repo" rev-parse --abbrev-ref HEAD)" = main
test "$(git -C "$DEMO_ROOT/repo" rev-parse HEAD)" = "$(git -C "$DEMO_ROOT/repo" rev-parse origin/main)"
cd "$DEMO_ROOT/repo"
fno config doctor | sed '/post-merge:/d'
```

```expected
[doctor] settings source: /Users/Shared/footnote-recording-demo/config.toml
[doctor] schema_version: 1
[doctor] state_dir: /Users/Shared/footnote-recording-demo/state

[doctor] OK; no suspicious paths detected.
```

The setup moves any prior demo root to a timestamped sibling before creating the new one. The `state_dir` line is the go-ahead. If it is under `$HOME`, stop. If `pwd` is not `/Users/Shared/footnote-recording-demo/repo`, stop. An incorrect config or working directory can put real node IDs, account names, and maintainer vault paths on camera.

## Terminal and capture

Record at 120 columns by 36 rows with an 18-point monospace font and the literal prompt `$ `. Disable notifications and clear scrollback before each take. Casts publish to asciinema.org. Narrated videos publish as GitHub Releases assets so every README link resolves from a fresh clone without Git LFS.

## The fence contract

Each numbered beat has one `run` fence containing the exact keystrokes, one command per line. When a command can run cheaply here, the next block is an `expected` fence copied from a real end-to-end run. For provider-billed, destructive, or session-dependent commands, use the literal line `[capture-at-record]`.

The recording-script test resolves every `fno`, `fno-py`, and `/fno:` command against the live command surface. It also rejects a run fence without its output block and rejects a scripted lesson whose file or medium-table row is missing.

## Recording order

Record L02 first because it replaces the public README's unkept screencast promise. Record L03, L04, L07, and L13 next, then record the seven casts in lesson order.
