# Output Template

Format for the think-tank report saved after each session.

## Report Structure

```markdown
# Panel Report: {decision title}

**Date:** {date} | **Depth:** {shallow/standard/deep} | **Personas:** {count} | **Session:** {slug}
**Decision:** {the question or proposal}
**Anti-Herd Status:** PASSED | GROUPTHINK WARNING

**Continued from:** {original_date} session | **New rounds:** {N} | **New context:** "{summary of injected context}"
_(Only present when session was loaded via continue. Omit for first-run sessions.)_

## Executive Summary

[3-5 sentence consensus with the top recommendation and key dissent]

## Consensus Summary

| Finding | Consensus | Priority | Confirmed By | Disputed By |
|---------|-----------|----------|--------------|-------------|
| [finding title] | Confirmed/Probable/Minority | 2.1 | PM, Designer, TU | Developer |
| [finding title] | Confirmed/Probable/Minority | 1.6 | ... | ... |

## Ranked Recommendations

### Recommendation 1: [action]
**Priority Score:** 2.1 | **Consensus:** Confirmed (3/5)

| Persona | Vote | Note |
|---------|------|------|
| PM | confirm | [rationale] |
| Designer | confirm | - |
| Developer | dispute | [reason for disagreement] |
| Target User | confirm | - |
| Devil's Advocate | abstain | [reason] |

[Rationale synthesized from supporting analyses]

### Recommendation 2: [action]
**Priority Score:** 1.6 | **Consensus:** Probable (2/5)

| Persona | Vote | Note |
|---------|------|------|
| ... | ... | ... |

[Rationale]

## Persona Analyses

### {Persona 1 Name} ({abbr})

[Full findings from Phase 4, preserved verbatim]

### {Persona 2 Name} ({abbr})

[Full findings preserved verbatim]

...

## Debate Highlights

### Round 1

- {Persona A} challenged {Persona B} on {topic}: {outcome}
- {Devil's Advocate} raised: {non-obvious point}
- User interjected: {user input, if any}

### Round 2

...

## Consensus Details

[Full synthesizer output with voting, dissent, and confidence scores]

### Anti-Herd Metrics

| Signal | Value | Threshold | Status |
|--------|-------|-----------|--------|
| flip_rate | {value} | > 0.8 | OK / SUSPICIOUS |
| entropy | {value} | < 0.3 | OK / SUSPICIOUS |
| convergence_speed | {rounds} | 1 round | OK / EXPECTED (shallow) |

**Overall:** PASSED | GROUPTHINK WARNING

## Open Questions

[Things the council couldn't resolve - need user input or data]

## Handoff

- Top recommendation ready for `/think` or `/blueprint`
- Report saved to: {path}
- Session state saved to: `.fno/think-tank-sessions/{slug}.json`
- Resume later: `/think panel continue {slug} "new information"`
```

## Key Rules

1. **Persona analyses are saved verbatim, not summarized.** The value is in the reasoning, not just the conclusions.
2. **Dissenting opinions are always noted.** Minority views often contain the most important warnings.
3. **Open questions are explicit.** If the panel couldn't resolve something, say so. Don't fake consensus.
4. **The executive summary leads with the action.** "Do X because Y" not "The panel discussed many perspectives."
