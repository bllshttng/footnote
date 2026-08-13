# House style for agent-authored text

Agent text is re-read by every recipient on every turn. It must read once. A machine checks six rules at the tool boundary. The same rules run on mail, PR bodies, comments, and changed markdown.

## The house style

Six rules, from the operator ruling of 2026-08-13, verbatim.

- use short sentences
- use the active voice
- give each word one meaning
- cut the clutter
- keep the writing warm and human - a person wrote it, not a manual
- adhere to ASD-STE100

The scope is every message, PR body, comment, doc, and anything you explain. ASD-STE100 is ASD Simplified Technical English, Issue 9.

## Warmth and STE100 pull against each other on purpose

STE100 is a controlled language written for aircraft maintenance manuals. It exists to remove voice. "Warm and human" asks for the opposite. Both rules are the operator ruling and both are intended.

Read them this way. STE100 governs the sentence. Warmth governs the choice of what to say.

Short active sentences with one meaning per word are the mechanics. A person who names a real cost, admits a mistake, and says the thing that matters is the warmth.

So a warm document is not one with longer sentences. It is one that tells the reader something true and useful in short ones.

A worker who reads both rules without this section picks one and drops the other. That is the whole reason this section exists.

## What the checker enforces

The house style above is the standard a person writes to. The list below is the part a machine can decide, so the two lists are different things and both count six. The checker is the floor, never the whole style. A draft that passes it can still bury the point, and no gate catches that.

1. A list-item sentence is 20 words or fewer. Every other sentence is 25 words or fewer.
2. No semicolon. Write two sentences.
3. No "should", "would", "may", "might", or "could". Write "can", "will", or "must".
4. No contractions. Write "do not", not "don't".
5. If a sentence carries "if" or "when", that word starts the sentence.
6. A paragraph is one physical line. A newline starts the next block.

## Rule 6 reverses the sentence-per-line convention

This repo used to mandate one full sentence per physical line. The operator ruling reverses that. The author inserts no newline inside a paragraph. The terminal or the renderer wraps it.

Two defects are refused together. One sentence per line puts a newline after every period. A hard wrap at 65 columns puts one every 65 columns. Neither belongs inside a paragraph.

Each of these starts the next block, so each one is a legal newline:

- a blank line
- a list marker
- a heading
- a table row
- a fence
- frontmatter
- a thematic break or a setext underline
- a blockquote
- a raw HTML line, such as a `<details>` block
- a link reference definition

A bare prose line under a list item is a lazy continuation, so rule 6 fires there. Fold it into the item, or start a new one.

Rule 6 is the one rule that reads a pair of lines rather than one sentence. On the changed-markdown gate that has a consequence worth naming. An added line under an untouched paragraph line still fires, because the paragraph it joins is the unit, not the diff.

## Rule 1 splits on block type

The word cap depends on the enclosing block, not on sentence mood. A sentence inside a bullet or a numbered item gets the 20-word cap. Every other sentence gets the 25-word cap.

A mood detector needs a verb lexicon, and a wrong guess is worse than no rule. Block type is a clean read from the line, so it needs no lexicon.

One false negative is accepted and kept. An imperative sentence sitting in a paragraph gets the 25-word cap. It is still capped.

Do not fix this with a part-of-speech tagger. The tagger trades a cheap cap for a fragile dependency.

## Rule 4 uses a closed list

The checker matches a fixed set of about 40 contractions. It never matches a possessive. A pattern flags "the agent's body", which is correct English. A missed contraction is a cheaper error than a refused correct sentence.

## Rule 5 has a sharp edge

A sentence like "do Y if X" fails the rule. That is the point of the rule. The condition must lead the command.

Hyphenated forms like "if-branch" are skipped, because the next character is not whitespace.

## Code does not count

A masking pass runs before any rule. A long identifier costs one word, not one word per character.

Removed entirely, so they cost zero words:

- fenced code blocks
- indented code blocks
- YAML frontmatter
- HTML comments, except a style-exception marker
- markdown table rows
- log lines

Replaced by one placeholder token, so they cost one word:

- inline code spans
- paths and URLs
- flags
- markdown link targets, with the link text kept

## The escape

Add a `style-exception:` line with a reason to bypass one body. An empty reason does not count. The escape scopes to the whole unit, so reaching for it is a decision. The receipt prints the reason, so the bypass is never silent.

Set `FNO_STYLE_ENFORCE` to 0 to disable the check in an emergency.

## Where the check runs

- `fno mail send` rejects a body that breaks the rules.
- A PR body that breaks the rules fails CI.
- Changed markdown under `docs/`, `skills/`, and `agents/` fails CI on the added lines.
- A PR comment has no chokepoint. Run `fno lint style --surface comment --stdin` yourself.

The comment surface is honest about refusing nothing. A PR comment goes out through `gh pr comment`, which this repo never wraps. A surface that reads as a guard and blocks nothing is worse than no surface at all.

The checker lives in `cli/src/fno/style.py`. Run it directly with `fno lint style`.
