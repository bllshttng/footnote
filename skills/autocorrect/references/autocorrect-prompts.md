# autocorrect reviewer prompts

The canonical prompt template lives below. `scripts/autocorrect-review.sh` reads this file, extracts the prompt body between the `<!-- PROMPT_START -->` and `<!-- PROMPT_END -->` markers, substitutes `{PACKET}` with the actual packet yaml, and sends the result to a fresh Claude API call.

Decoupling the prompt from the script lets the user tune wording without touching shell code, and lets the corrections loop eventually propose a CONVERT-TO-VERIFIER patch that modifies *this* file (the loop reviewing the loop).

## Canonical prompt

<!-- PROMPT_START -->
You are reviewing a personal AI coding toolkit. Below is a packet containing corrections (mistakes the toolkit's user had to manually correct), a git log of changes to the toolkit's rules, BLOCKED state from the toolkit's feature graph, and the CURRENT FULL TEXT of every rule or skill file referenced by those corrections.

Your job: propose patches the user should consider. For each correction class (group by source field), recommend ONE of:

(a) **DELETE**: the rule is dead. It's never referenced, or it has been fully superseded by a verifier that catches the same class mechanically. Cite the rule text being deleted.
(b) **REWORD**: the rule is unclear or contradicts another rule. Provide the new wording.
(c) **CONVERT-TO-VERIFIER**: the rule expresses a check that could be mechanized as a script, hook, or pre-commit step. Sketch the verifier in pseudocode. Note that the rule text MUST be deleted in the same commit as the verifier addition.
(d) **KEEP**: the rule is doing its job; no action needed. Use sparingly: this is the default and shouldn't dominate the output.
(e) **NEW-RULE**: this correction class isn't covered by any existing rule. Propose minimal new rule text and where it should land.

## Severity discipline

Severity tier S0 events MUST result in either CONVERT-TO-VERIFIER or NEW-RULE. They are too dangerous to leave to instruction-following alone. A KEEP or DELETE recommendation on an S0 event is a review error; flag it explicitly if the input data makes you uncertain.

S1 events: prefer CONVERT-TO-VERIFIER when feasible; otherwise REWORD or NEW-RULE. DELETE only when the rule is genuinely dead.

S2 events: tolerate KEEP and REWORD more freely; CONVERT-TO-VERIFIER is welcome but not required.

## Output format

Numbered list. Each item contains exactly these fields, in this order:

```
N. Source: <correction class / source field>
   Action: <a|b|c|d|e>
   Target file: <path>
   Patch: <diff if reword/convert/delete; full new text if new-rule; "n/a" if keep>
   Rationale: <one sentence>
```

Group by source field; if multiple events share a source, generate ONE recommendation that covers all of them.

If the packet contains zero events in a window, output exactly: `No corrections in this window; no patches proposed.`

## What NOT to recommend

- Do not propose changes to files whose full text is not provided in `implicated_rules`. You don't have the current state of those files, so any patch would be guesswork.
- Do not propose patches that touch the corrections.log itself, the watermark files, or the capture scripts. Those are infrastructure, not rules.
- Do not propose general "improvements" beyond what the events justify. The events are the evidence; without an event you have no warrant.

## Packet

PACKET:
{PACKET}
<!-- PROMPT_END -->

## Maintenance notes

- The prompt is intentionally short. Most context is in the packet (events + full rule text).
- The CONVERT-TO-VERIFIER recommendation has a hard invariant: same-commit deletion of the rule text. This is enforced by `autocorrect-triage.sh` (Task 2.3) at apply time. The reviewer just states the invariant; triage enforces it.
- "Group by source field" is the unit of recommendation: ten emdash-grep hits across the month yield ONE recommendation, not ten.
- The reviewer is given full rule text rather than diffs because most patches need to see the rule as it currently is, not how it changed. The git log is supplementary signal for "what's been edited lately", not the primary review input.
