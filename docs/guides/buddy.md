# Buddy: a companion beside your status line

Buddy is a small terminal companion. It stands at the right end of your status line, moves a little, and says one line after each turn. It is a Claude Code mod in its own plugin, `buddy`, in the footnote marketplace. You do not need fno to use it. `/buddy` and `/bbb` (bring back buddy) are the same command.

It needs Claude Code 2.1.287 or later. That is the first version that loads mods.

## Why we brought it back

Claude Code shipped `/buddy` until v2.1.97. On 2026-04-09, users found that the command was gone. The community issue reports that no changelog line named the removal. [anthropics/claude-code#45596](https://github.com/anthropics/claude-code/issues/45596), "Bring Back Buddy", collects 8 of them. People did not ask for less buddy. They asked for a buddy that they can change, that works in more places, and that knows more about their work.

A mod is the right way to do this. A mod runs inside Claude Code, with no fork and no patch to the CLI. A mod can draw on the screen, register a slash command, and read files. It stops when you turn the plugin off. The buddy also knows your fno fleet, so it can tell you when work ships and when a question waits for you.

## Start

1. Install the plugin: `/plugin install buddy@footnote`. The buddy hatches at the next session start.
2. Type `/buddy` to see its card.
3. Type `/buddy statusline` to put it beside your status line. This is the best place for the buddy.
4. If you want the buddy in a side pane, type `/buddy pane`.

## What you see

- **The sprite.** The buddy is one of 18 species. It has a rarity, eyes, and sometimes a hat. Five stats set its voice: debugging, patience, chaos, wisdom, and snark.
- **Idle talk.** When nothing happens for 2 minutes, the buddy says something of its own. It is one model call, in its personality, about what the session is doing. The buddy has no canned lines.
- **A model line.** When a turn of 5 seconds or more ends, one model call reads the last exchange and answers in the voice of the buddy. This call runs at most once each 10 seconds, and only while the buddy is on screen.
- **Fleet news.** When `fno-agents` is on your PATH, the buddy reads the fleet activity feed every 2 minutes. It tells you when a node ships a PR, when a node finishes, and when a question waits for you. It says the news in its own voice with one model call. If that call fails, it says the plain fact.
- **The fno CLI is optional.** Fleet news and fleet counts need the `fno` CLI. Without it, the buddy still talks about your own session.
- **Fleet counts.** Beside its name, the buddy shows live workers, questions that wait for you, and your open PRs. One read every 5 minutes serves every session.

## Where it stands

The buddy has three places. It uses the first place that is available.

1. **Beside your status line.** This is the default after you type `/buddy statusline`. Your own status line stays on the left, unchanged. The buddy stands at the right edge with the same sprite as the original: 4 or 5 rows of art (the top row holds the hat), then a row for its name. Its words wrap to its left, up to 30 columns wide, on the rows beside the art. Below 100 columns it shows a one-line face, as the original did. The mode line of Claude Code shows under the last row, so a buddy taller than your status line adds rows.
2. **A narrow pane on the right.** Type `/buddy pane` to put the buddy here. The sprite stands at the bottom, and its words are above it. Claude Code shows this pane only in the fullscreen layout, at 110 columns or more. If you never typed `/buddy statusline`, the buddy opens this pane by itself at 144 columns or more.
3. **One line above the prompt.** If the first two places are not available, the buddy shows a one-line face above the prompt.

The length of your status line does not move the buddy. The buddy always aligns to the right edge of the terminal. If a row of your status line is too wide to share, the buddy moves down one row. If 6 rows cannot hold the buddy, it changes to the one-line face.

### Your status line, and how to undo it

`/buddy statusline` saves your current `statusLine` setting. Then it points `statusLine` at a small wrapper. The wrapper runs your status line first, then draws the buddy beside it. If you have no status line, the left side shows the model, the folder, the context use, and the cost.

`/buddy pane` puts your saved setting back, exactly as it was, and moves the buddy to the side pane. `/buddy restore` does the same thing. If the buddy cannot read the saved copy, it does not change your settings.

If you run `/statusline` again later, the buddy tells you at the next session start. It does not wrap the new status line automatically. Type `/buddy statusline` to put the buddy beside it again.

## The reroll game

`/buddy roll` hatches a new buddy in place of the old one. Each roll costs one reroll. You cannot undo a roll.

You get rerolls in two ways:

- **One reroll each day.** The day follows the clock of the Claude Code process, which can be UTC.
- **One reroll for every 2 PRs that your fleet ships.** The buddy counts shipped PRs from the fleet feed. Each PR counts one time, even when many sessions read the same feed.

You can keep 3 rerolls at most. A reroll that you earn with a full bank is lost. The card shows your bank, for example `rerolls: 2/3`. When the bank is empty, `/buddy roll` tells you how to get the next reroll, and your buddy stays.

## Commands

| Command | What it does |
| :- | :- |
| `/buddy` | Shows the card: species, rarity, personality, stats, and rerolls |
| `/buddy pet` | Pets the buddy |
| `/buddy roll` | Spends one reroll to hatch a new buddy |
| `/buddy statusline` | Puts the buddy beside your status line |
| `/buddy pane` | Puts your status line back as it was and moves the buddy to a side pane. `/buddy restore` is the same command. |
| `/buddy off` | Hides the buddy, closes its pane, and stops every model call and feed read |
| `/buddy on` | Shows the buddy again |
| `/buddy bye` | Puts your status line back, closes the pane, and turns the buddy off in every session. It keeps the soul, so `/buddy on` brings the same buddy back. |

While the buddy is off, it makes no model call, no feed read, and no fleet read. At session start it only reads its saved state.

`/bbb` takes the same words: `/bbb roll`, `/bbb pet`, and the others. In the docked pane, press `p` to pet the buddy.

## Your old buddy

If you hatched a buddy before Claude Code removed it, its name and personality are still in `~/.claude.json`. The first session with the mod reads them and says hello again. The mod cannot make the old hash again. If the personality text names a species, the buddy gets that species. The mod rolls the other traits again.

## Cost and models

Each model line is one call on your own plan, with a limit of 80 output tokens. `/buddy off` stops these calls for every session.

The buddy asks for the `haiku` model through the API client of your session. On an Anthropic session, that is Claude Haiku. If your session uses a different endpoint, the `haiku` name goes to that endpoint. For example, a session with `ANTHROPIC_DEFAULT_HAIKU_MODEL` set to a GLM model gets GLM lines.

## Storage

- The soul of the buddy (seed, name, personality) and the reroll bank are in the mod store under `~/.claude/plugins/store/`.
- The status line files are in `state/buddy/` in the fno state folder (`~/.fno/` by default). Without fno, they are in `~/.local/state/buddy/`. They are the wrapper, your saved `statusLine`, and one frame file for each session. The wrapper erases a frame file one day after its last write.

## Where the code is

The plugin is `plugins/buddy/` in the footnote repo. `plugins/buddy/hooks/hooks.json` names the mod, `register.ts`. Run its tests with `claude plugin test plugins/buddy`.

## If you used the buddy inside fno

Before this change, the fno plugin loaded the buddy. Now fno does not load it. Install `buddy@footnote` to get it back. The soul and the reroll bank are in the mod store, so the same buddy comes back.
