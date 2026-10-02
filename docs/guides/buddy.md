# Buddy: a companion above the prompt

Claude Code once shipped a small companion, `/buddy`, and removed it in v2.1.97. The fno plugin brings it back as a Claude Code mod. A buddy sits in the band above the prompt, fidgets, and says one line after each turn.

It needs Claude Code 2.1.287 or later, the first version that loads mods.

## What you see

- **The sprite.** It is one of 18 species, with a rarity, an eye and maybe a hat. Five stats shape its voice: debugging, patience, chaos, wisdom and snark.
- **A quick line** the moment a turn of 5 seconds or more ends. The buddy's highest stat picks it.
- **A model line** a moment later. One Haiku call reads the last exchange and answers in the buddy's voice. It runs at most once every 10 seconds, and only while the band is on screen.
- **Fleet news.** With `fno-agents` on your PATH, the buddy reads the activity feed every 2 minutes. It reads only while the band is on screen. It names a node that shipped a PR, a node that finished, and a question waiting on you. These lines use no model call.

In a band shorter than 6 rows, or narrower than 40 columns, the buddy shrinks to a one-line face.

## Commands

| Command | What it does |
| :- | :- |
| `/buddy` | Shows the card: species, rarity, personality and stats |
| `/buddy pet` | Pets it |
| `/buddy roll` | Hatches a new buddy in place of the old one |
| `/buddy off` | Hides it and stops every model call and feed read |
| `/buddy on` | Brings it back |

Press `p` while the band has focus to pet it.

## Your old buddy

If you hatched a buddy before Claude Code removed it, its name and personality are still in `~/.claude.json`. The first session with the mod reads them and says hello again. The old roll used a hash a mod cannot reproduce. If the personality text names a species, the buddy takes that species. The other traits are rolled again.

## Cost and storage

Each model line is one Haiku call on your own plan, capped at 60 output tokens. `/buddy off` turns them off for every session. The buddy keeps its soul (seed, name, personality) in the mod store under `~/.claude/plugins/store/`, never in `~/.claude.json`.

## Where the code lives

The mod is `hooks/buddy/`, named by the `modules` key of `hooks/hooks.json`. `claude plugin test` runs `hooks/buddy/buddy.test.ts`. The repo also holds an opencode test file, so run the mod tests from a copy that holds only `.claude-plugin/plugin.json`, `hooks/hooks.json` and `hooks/buddy/`.
