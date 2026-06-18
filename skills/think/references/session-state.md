# Session State Schema

JSON schema for think-tank session files stored at `.fno/think-tank-sessions/{slug}.json`.

## Schema

```json
{
  "slug": "string - kebab-case, max 40 chars, derived from decision text",
  "created_at": "ISO 8601 timestamp - when session was first created",
  "updated_at": "ISO 8601 timestamp - when session was last updated (continuation)",
  "decision": "string - the original question or proposal",
  "depth": "shallow | standard | deep",
  "persona_set": "default | startup | adversarial | custom",
  "rounds_completed": "number - total debate rounds across all sessions",

  "personas": [
    {
      "name": "string - display name",
      "abbr": "string - 2-3 letter abbreviation",
      "role": "string - role description",
      "expertise": "string - expertise areas",
      "bias": "string - bias direction",
      "is_user": "boolean - true if this is the human user's seat",
      "source": "built-in | project-default | custom | user"
    }
  ],

  "findings": [
    {
      "id": "string - e.g. SF-1, TU-2, DA-3",
      "persona": "string - persona abbreviation",
      "title": "string - one-line finding title",
      "severity": "CRITICAL | HIGH | MEDIUM | LOW",
      "confidence": "HIGH | MEDIUM | LOW",
      "rationale": "string - full reasoning",
      "recommendation": "string - concrete action",
      "votes": {
        "SF": "confirm | dispute | abstain",
        "TU": "confirm | dispute | abstain"
      },
      "consensus": "Confirmed | Probable | Minority | Discarded",
      "priority_score": "number - computed from severity, confidence, consensus_ratio",
      "status": "active | revised | withdrawn - tracks changes across continuations"
    }
  ],

  "anti_herd": {
    "flip_rate": "number - 0.0 to 1.0",
    "entropy": "number - Shannon entropy of vote distribution",
    "convergence_speed": "number - rounds to reach 80% agreement",
    "status": "PASSED | GROUPTHINK WARNING"
  },

  "recommendations": [
    {
      "rank": "number - position in priority order",
      "action": "string - recommended action",
      "confidence": "HIGH | MEDIUM | LOW",
      "supporters": ["string - persona abbreviations"],
      "dissenters": ["string - persona abbreviations with reasons"]
    }
  ],

  "user_contributions": [
    {
      "phase": "number - which phase (4, 5, 5.5, 6)",
      "round": "number - debate round (0 for independent analysis)",
      "content": "string - what the user said",
      "structured_findings": ["string - finding IDs created from user input"]
    }
  ],

  "open_questions": ["string - unresolved items"],

  "debate_rounds": [
    {
      "round": "number",
      "contributions": [
        {
          "persona": "string - abbreviation",
          "action": "challenge | support | revise | new_point",
          "target_finding": "string - finding ID if challenge/support, null otherwise",
          "content": "string - the contribution text"
        }
      ]
    }
  ],

  "continuation_history": [
    {
      "date": "ISO 8601 timestamp",
      "new_context": "string - what new information was provided",
      "rounds_added": "number - debate rounds in this continuation",
      "findings_revised": ["string - finding IDs that changed"],
      "findings_added": ["string - new finding IDs"]
    }
  ]
}
```

## Slug Generation

1. Take the decision text
2. Lowercase
3. Replace spaces and punctuation with hyphens. If the resulting string is empty (e.g., input was only emojis or symbols), use "session-" followed by a short timestamp (YYYYMMDD-HHMM)
4. Remove consecutive hyphens
5. Truncate to 40 characters
6. Trim trailing hyphens

Examples:
- "should we add dark mode" -> `should-we-add-dark-mode`
- "rebuild billing or iterate on current" -> `rebuild-billing-or-iterate-on-current`
- "Can temporal intelligence be a defensible moat for our product?" -> `can-temporal-intelligence-be-a-defensibl`

## File Location

```
.fno/think-tank-sessions/
  should-we-add-dark-mode.json
  temporal-moat.json
  pricing-strategy.json
```

## Continuation Behavior

When `--continue {slug}` loads a session:
- `updated_at` is set to current time
- `rounds_completed` is incremented by the new rounds
- New findings are appended to `findings[]` with fresh IDs
- Revised findings have their `status` changed to `revised`
- Withdrawn findings have their `status` changed to `withdrawn`
- A new entry is appended to `continuation_history[]`
- `recommendations[]` is regenerated from the updated findings and votes
