# Style rules for agent-authored text

Agent text is re-read by every recipient on every turn.
It must read once.
A machine checks five rules at the tool boundary.
The same rules run on mail, PR bodies, and changed markdown.

## The five rules

1. A list-item sentence is 20 words or fewer.
   Every other sentence is 25 words or fewer.
2. No semicolon. Write two sentences.
3. No "should", "would", "may", "might", or "could".
   Write "can", "will", or "must".
4. No contractions. Write "do not", not "don't".
5. If a sentence carries "if" or "when", that word starts the sentence.

## Rule 1 splits on block type

The word cap depends on the enclosing block, not on sentence mood.
A sentence inside a bullet or a numbered item gets the 20-word cap.
Every other sentence gets the 25-word cap.

A mood detector needs a verb lexicon, and a wrong guess is worse than no rule.
Block type is a clean read from the line, so it needs no lexicon.

One false negative is accepted and kept.
An imperative sentence sitting in a paragraph gets the 25-word cap.
It is still capped.
Do not fix this with a part-of-speech tagger.
The tagger trades a cheap cap for a fragile dependency.

## Rule 4 uses a closed list

The checker matches a fixed set of about 40 contractions.
It never matches a possessive.
A pattern flags "the agent's body", which is correct English.
A missed contraction is a cheaper error than a refused correct sentence.

## Rule 5 has a sharp edge

A sentence like "do Y if X" fails the rule.
That is the point of the rule.
The condition must lead the command.
Hyphenated forms like "if-branch" are skipped, because the next character is not whitespace.

## Code does not count

A masking pass runs before any rule.
A long identifier costs one word, not one word per character.

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

Add a `style-exception:` line with a reason to bypass one body.
An empty reason does not count.
The escape scopes to the whole unit, so reaching for it is a decision.
The receipt prints the reason, so the bypass is never silent.

Set `FNO_STYLE_ENFORCE` to 0 to disable the check in an emergency.

## Where the check runs

- `fno mail send` rejects a body that breaks the rules.
- A PR body that breaks the rules fails CI.
- Changed markdown under `docs/`, `skills/`, and `agents/` fails CI on the added lines.

The checker lives in `cli/src/fno/style.py`.
Run it directly with `fno lint style`.
