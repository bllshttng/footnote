# Buddy: a companion beside your status line

Claude Code once shipped a small companion, `/buddy`, and removed it in v2.1.97. The fno plugin brings it back as a Claude Code mod. A buddy stands at the right end of your status line, fidgets, and says one line after each turn.

It needs Claude Code 2.1.287 or later, the first version that loads mods.

## What you see

- **The sprite.** It is one of 18 species, with a rarity, an eye and maybe a hat. Five stats shape its voice: debugging, patience, chaos, wisdom and snark.
- **A quick line** the moment a turn of 5 seconds or more ends. The buddy's highest stat picks it.
- **A model line** a moment later. One Haiku call reads the last exchange and answers in the buddy's voice. It runs at most once every 10 seconds, and only while the buddy is on screen.
- **Fleet news.** With `fno-agents` on your PATH, the buddy reads the activity feed every 2 minutes. It reads only while the buddy is on screen. It names a node that shipped a PR, a node that finished, and a question waiting on you. These lines use no model call.

- **Fleet counts.** Beside its name the buddy shows live workers, questions waiting on you, and your open PRs. They come from `fno agents gate-status`, `fno inbox outstanding --json` and `gh pr list`. Those verbs take several seconds, so one read every 5 minutes serves every session.

## Where it stands

Run `/buddy statusline` once. It saves your current `statusLine` setting to `state/buddy/inner.json` in the fno state folder (`~/.fno/` by default) and points `statusLine` at a small wrapper, `state/buddy/statusline.py` in that folder, with `refreshInterval: 1`. The wrapper runs your own status line unchanged on the left and draws the buddy flush right. The status area grows to 4-6 rows while the buddy is there. With no status line of your own, the left side shows the model, the folder, context use and cost.

`/buddy statusline off` puts your saved setting back exactly. If you run `/statusline` again later, the buddy says so at the next session start and waits for you to run `/buddy statusline` again. It never wraps the new command on its own.

A row of your status line that is too wide to share pushes the buddy down a row. If 6 rows still cannot hold it, the buddy shrinks to a one-line face on the last row.

Without the wrapper, the buddy opens a narrow pane docked on the right in the fullscreen layout: the sprite stands at the bottom, its words and the fleet counts above it. Claude Code opens an unasked pane only at 144 columns or wider. Below that, or on the main screen layout, the buddy is a one-line face above the prompt.

## Commands

| Command | What it does |
| :- | :- |
| `/buddy` | Shows the card: species, rarity, personality and stats |
| `/buddy pet` | Pets it |
| `/buddy roll` | Hatches a new buddy in place of the old one |
| `/buddy statusline` | Draws the buddy beside your status line |
| `/buddy statusline off` | Restores your status line as it was |
| `/buddy off` | Hides it and stops every model call and feed read |
| `/buddy on` | Brings it back |

Press `p` while the docked pane has focus to pet it.

## Your old buddy

If you hatched a buddy before Claude Code removed it, its name and personality are still in `~/.claude.json`. The first session with the mod reads them and says hello again. The old roll used a hash a mod cannot reproduce. If the personality text names a species, the buddy takes that species. The other traits are rolled again.

## Cost and storage

Each model line is one Haiku call on your own plan, capped at 60 output tokens. `/buddy off` turns them off for every session. The buddy keeps its soul (seed, name, personality) in the mod store under `~/.claude/plugins/store/`, never in `~/.claude.json`.

## Where the code lives

The mod is `hooks/buddy/`, named by the `modules` key of `hooks/hooks.json`. `claude plugin test` runs `hooks/buddy/buddy.test.ts`. The repo also holds an opencode test file, so run the mod tests from a copy that holds only `.claude-plugin/plugin.json`, `hooks/hooks.json` and `hooks/buddy/`.
