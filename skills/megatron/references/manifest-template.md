# Megatron mission manifest template

The skill renders this template with the gathered discovery answers
and writes the result to ``~/.fno/fleet/{slug}/00-INDEX.md``.
Substitution markers use ``{{ name }}`` syntax (Mustache-compatible).
The skill body performs the substitutions; this file is the source of
truth so the validator and parser stay in sync.

```markdown
---
mission_type: fleet
mission_id: {{ mission_id }}
slug: {{ slug }}
created: {{ created_iso }}
goal: |
  {{ goal }}
budget:
  cost_cap_usd_per_mission: {{ cost_cap_usd_per_mission | default(50.0) }}
failure_policy: {{ failure_policy | default("block") }}
autonomy_level: {{ autonomy_level | default("cautious") }}
waves:
{{#waves}}
  - wave: {{ wave }}
    mode: {{ mode }}
    {{#wave_type}}
    wave_type: {{ wave_type }}
    {{/wave_type}}
    projects:
      {{#projects}}
      - name: {{ name }}
        body: |
          {{ body }}
      {{/projects}}
{{/waves}}
---

# Mission: {{ goal }}

Authored {{ created_iso }} via /megatron.

## Goal

{{ goal_description }}

## Participating projects

{{#participating_projects}}
- {{ . }}
{{/participating_projects}}

## Wave plan

{{#waves}}
### Wave {{ wave }} - {{ mode }}{{#wave_type}} ({{ wave_type }}){{/wave_type}}

{{#projects}}
- **{{ name }}**: {{ body | first_line }}
{{/projects}}
{{/waves}}

## Notes

{{ free_form_notes }}
```

## Substitution mapping

| Template var                  | Source                              |
|-------------------------------|-------------------------------------|
| ``mission_id``                | generated: ``ab-`` + 8 hex chars    |
| ``slug``                      | ``YYYY-MM-DD-<sanitized-goal>``     |
| ``created_iso``               | UTC ISO8601 at draft time           |
| ``goal``                      | discovery question 1                |
| ``goal_description``          | optional elaboration                |
| ``participating_projects``    | discovery question 2                |
| ``waves``                     | derived from question 3 + 4         |
| ``cost_cap_usd_per_mission``  | settings.yaml or default 50.0       |
| ``failure_policy``            | discovery question 5 (default block)|
| ``autonomy_level``            | discovery question 5 (default cautious) |

## Validator contract

After substitution, the rendered manifest MUST pass
``fno.megatron.validate_manifest`` cleanly. The skill calls the
validator before moving the draft into the canonical fleet directory.
Non-empty error lists trigger the failure-mode flow described in
``SKILL.md`` Step 4.
