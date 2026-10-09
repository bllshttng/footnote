# Buddy: a companion beside your status line

Buddy is a small terminal companion. It stands at the right end of your status line, moves a little, and says one line after each turn. It is a Claude Code mod, claude-mod-buddy, in its own plugin, `buddy`, in the footnote marketplace. You do not need fno to use it. `/buddy` and `/bbb` (bring back buddy) are the same command.

It needs Claude Code 2.1.287 or later. That is the first version that loads mods.

## Why we brought it back

Claude Code shipped `/buddy` until v2.1.97. On 2026-04-09, users found that the command was gone. The community issue reports that no changelog line named the removal. [anthropics/claude-code#45596](https://github.com/anthropics/claude-code/issues/45596), "Bring Back Buddy", collects 8 of them. People did not ask for less buddy. They asked for a buddy that they can change, that works in more places, and that knows more about their work.

A mod is the right way to do this. A mod runs inside Claude Code, with no fork and no patch to the CLI. A mod can draw on the screen, register a slash command, and read files. It stops when you turn the plugin off. The buddy also knows your fno fleet, so it can tell you when work ships and when a question waits for you.

## Start

1. Install the plugin: `/plugin install buddy@footnote`. The buddy hatches at the next session start.
2. Type `/buddy` to see its card. The first time, you watch it hatch from an egg. In a wide fullscreen terminal, and in Desktop, the card opens in a pane on the right. Press any key to close it. In other terminals, Claude Code would put that pane above the prompt, so the card shows in the transcript instead.
3. Type `/buddy statusline` to put it beside your status line. This is the best place for the buddy.
4. If you want the buddy in a side pane, type `/buddy pane`.

## What you see

- **The sprite.** The buddy is one of 18 species. It has a rarity, eyes, and sometimes a hat. Five stats set its voice: debugging, patience, chaos, wisdom, and snark.
- **Rarity colors.** In the dark theme, common is gray, uncommon green, rare blue, epic purple, and legendary gold. The card, the status line, and the Desktop sprite all draw the color of your Claude Code theme. A custom theme draws the dark theme's colors in the status line.
- **Idle talk.** When nothing happens for 2 minutes, the buddy says something of its own. It is one model call, in its personality, about what the session is doing. The buddy has no canned lines.
- **Reactions.** After a turn, one model call reads the last exchange and answers in the voice of the buddy. Ordinary turns wait 30 seconds between reactions, as the original did. A turn that says the buddy's name, fails tests, hits an error, or lands a diff over 80 lines gets a reaction at once. Petting and hatching get one too. The last three lines go along, so the buddy does not repeat itself.
- **Observations.** Every observation the buddy makes goes to `~/.fno/state/buddy/observations.jsonl`, one JSON row each with the time, the name, the reason, and the line. Read it with `tail ~/.fno/state/buddy/observations.jsonl`.
- **Fleet news.** When `fno-agents` is on your PATH, the buddy reads the fleet activity feed every 2 minutes. It tells you when a node ships a PR, when a node finishes, and when a question waits for you. It says the news in its own voice with one model call. If that call fails, it says the plain fact.
- **The fno CLI is optional.** Fleet news and fleet counts need the `fno` CLI. Without it, the buddy still talks about your own session.
- **Fleet counts.** Beside its name, the buddy shows live workers, questions that wait for you, and your open PRs. One read every 5 minutes serves every session.

## Only where you can see it

Each live session has its own buddy, and all of them share one soul. If no person can see the answer, a buddy makes no model call. Hidden sessions stay quiet.

- **In an fno mux pane,** the mux writes the panes on screen to `~/.fno/mux/<session>.visible.json`. A buddy whose pane is not in that list makes no model call. Its sessions on another tab or in the sideline cost nothing.
- **Anywhere else,** the buddy counts as seen for 10 minutes after you type in that session's prompt box.
- **One voice for the machine.** Idle talk and fleet news happen once. After the 2-minute gap, the first seen session says the idle line. The first seen session to read a fleet event tells it. Two panes side by side do not say the same news twice.
- **Reactions** stay with each session, because each one is about that session's own turn.

## Where it stands

The buddy has three places. It uses the first place that is available.

1. **Beside your status line.** This is the default after you type `/buddy statusline`. Your own status line stays on the left. The buddy stands at the right edge with the same sprite as the original: 4 or 5 rows of art (the top row holds the hat), then a row for its name. Its words show to its left in a thought bubble: a rounded outline that starts 30 columns wide and grows up to 60 columns for a long line, with a trail of dots toward the buddy. While the bubble shows, it can cover the ends of your rows; they come back when it fades. If your rows are too wide to share even without a bubble, the buddy cuts the ends of the rows beside it. Below 60 columns it shows a one-line face. The mode line of Claude Code shows under the last row, so a buddy taller than your status line adds rows.
2. **A narrow pane on the right.** Type `/buddy pane` to put the buddy here. The sprite stands at the bottom, and its words are above it. Claude Code shows this pane only in the fullscreen layout, at 110 columns or more. If you never typed `/buddy statusline`, the buddy opens this pane by itself at 144 columns or more.
3. **One line above the prompt.** If the first two places are not available, the buddy shows a one-line face above the prompt.

In the Claude Desktop app, the buddy stands right above the text input, at the right edge: the full sprite, its name below, and its thought bubble to its left. If that band has too few rows, it shows the one-line face. Desktop has no status line, so `/buddy statusline` changes nothing there, even when it is on in your terminal. The card and its hatch work in both apps.

The length of your status line does not move the buddy. The buddy always aligns to the right edge of the terminal. If a row of your status line is too wide to share, the buddy first moves down a row. If no row count up to 6 fits, it keeps the full sprite and cuts your rows. If your status line already uses all 6 rows, it changes to the one-line face.

### Your status line, and how to undo it

`/buddy statusline` saves your current `statusLine` setting. Then it points `statusLine` at a small wrapper. The wrapper runs your status line first, then draws the buddy beside it. If you have no status line, the left side shows the model, the folder, the context use, and the cost.

The wrapper reruns every second, so the buddy animates. Most of those runs cost almost nothing: a small bash script prints the last output again when the session, the width, and the buddy's frame have not changed, and Python starts only when one of them has, or on every 25th run in a row. The buddy moves only for 1 minute after something happens: you type, a turn ends, it speaks, or you pet it. Then it holds still until the next thing, so an idle session stays on that cheap path. While it moves, it changes pose every 2 seconds. In the fno mux, a pane that is not on screen never moves. Your own status line does not rerun every second. It reruns when the session changes, or every 30 seconds at most. To choose that time, give your status line its own `refreshInterval`. If you set `refreshInterval` on the wrapper itself, the next session moves that number to your status line, if it has none of its own, and sets the wrapper back to 1 second.

`/buddy pane` puts your saved setting back, exactly as it was, and moves the buddy to the side pane. `/buddy restore` does the same thing. If the buddy cannot read the saved copy, it does not change your settings.

If you run `/statusline` again later, the buddy tells you at the next session start. It does not wrap the new status line automatically. Type `/buddy statusline` to put the buddy beside it again.

## The reroll game

`/buddy roll` hatches a new buddy in place of the old one. Each roll costs one reroll. You cannot undo a roll. All your live sessions share one buddy, so each session shows the new buddy within 2 seconds.

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
