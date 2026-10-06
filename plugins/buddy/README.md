# buddy

Bring back buddy: a terminal companion beside your Claude Code status line. It hatches from an egg, watches your work, and says one line in its own voice after a turn. Every line is one small model call in its personality. It has no canned lines.

It needs Claude Code 2.1.287 or later. You do not need fno, but with the `fno` CLI it also tells you when your fleet ships work or a question waits for you.

## Install

1. Run `/plugin install buddy@footnote`.
2. Type `/buddy` to watch it hatch and see its card. Press any key to close the card.
3. Type `/buddy statusline` to put it beside your status line.

## Commands

`/buddy` and `/bbb` are the same command.

| Command | What it does |
|---|---|
| `/buddy` | Show the card |
| `/buddy pet` | Pet the buddy |
| `/buddy roll` | Roll a new buddy (3 rerolls, refilled daily) |
| `/buddy statusline` | Stand beside your status line |
| `/buddy pane` | Stand in a side pane (fullscreen layout) |
| `/buddy off` / `on` | Mute or unmute |
| `/buddy bye` | Remove the buddy and restore your status line |

Every observation goes to `~/.fno/state/buddy/observations.jsonl`.

The full guide: [docs/guides/buddy.md](../../docs/guides/buddy.md).
