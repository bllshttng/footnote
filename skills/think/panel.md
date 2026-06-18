
# Panel

Multi-persona product intelligence. A panel of opinionated experts
debates your product decisions on the main thread. This is the `panel` mode of
`/think`.

`/think` is you and Claude 1-on-1. `/think panel` is you and a panel.

<HARD-GATE>
Do NOT write code, create plans, or take implementation actions.
Panel mode produces analysis and recommendations only.
Implementation happens via /think, /plan, or /target after handoff.
</HARD-GATE>

## Usage

```
# Interactive (default) - user has a seat on the panel
/think panel "should we add dark mode"
/think panel deep "rebuild billing or iterate on current"
/think panel startup "is this the right market"
/think panel --chain plan "add provider onboarding flow"
/think panel --rounds 3 "pricing strategy for enterprise"

# Autonomous - runs without pauses, briefing phase gathers ground truth upfront
/think panel auto "quarterly roadmap priorities"
/think panel auto deep "pricing strategy for enterprise"
/think panel auto startup "is this the right market"

# Continue - resume a prior debate with new information
/think panel continue temporal-moat "we ran the engagement analysis, opens spike around inspection time"
/think panel continue list

# Modifiers (combine with any mode)
/think panel --user-role "Small Business Owner" "pricing strategy"
/think panel --no-user-seat "pricing strategy"
/think panel auto --rounds 5 "pricing strategy"
```

## Process

### 0. Parse Arguments

Extract subcommands and flags from arguments:

**Subcommands** (positional, before the decision text):
- `auto` - autonomous mode: skip interactive pauses, run briefing phase instead (implies `--no-user-seat`)
- `continue {slug}` - resume a prior session (or `continue list` to show available sessions)
- `deep` / `shallow` - depth preset (default: standard)
- `startup` / `adversarial` - persona set (default: default)

**Flags** (dashed, combine with any mode):
- `--chain` (think/plan/megawalk/none) - default: none
- `--rounds N` - override debate rounds (default: from depth preset)
- `--no-user-seat` - opt out of formal user seat on the panel
- `--user-role "Role"` - custom name for the user's panel seat

Remaining text after subcommand/flag extraction = the decision/question.
If remaining text is empty or whitespace-only, and `continue` is not set, treat as "no decision provided" and enter interactive setup.

**Resolution (do this before anything else):**
- If `auto` is set, also set `--no-user-seat` to true
- If `--rounds` is present and not a positive integer, error and stop
- Subcommands are case-insensitive (`Auto` = `auto`)

### 1. Interactive Setup (when no decision provided)

Load [references/workflow.md](references/workflow.md) Phase 1.

If no decision text in arguments, ask (batched, one AskUserQuestion):
- What decision are you trying to make?
- What perspective matters most? (product fit / technical / market / all)
- How deep? (shallow: 3 personas 1 round / standard: 5 personas 2 rounds / deep: all personas 3 rounds)
- Chain after? (think / plan / megawalk / none)

### 2. Gather Context

Load [references/workflow.md](references/workflow.md) Phase 2.

Read project vision from settings.yaml. Read recent git log (last 20 commits).
Check for existing think docs or plans. Build product context block for injection
into persona prompts.

### 3. Generate Personas

Load [references/persona-templates.md](references/persona-templates.md).

Select set: default (product), startup, adversarial, or custom.
Apply depth preset to trim persona count.
Announce: "Panel assembled: [list persona names and roles]"

### 4. Independent Analysis

Load [references/workflow.md](references/workflow.md) Phase 4.

Each persona analyzes the decision independently. Process sequentially on the main
thread. Output each persona's analysis visibly as it's produced.
Finding limit per persona: `ceil(24 / persona_count)`.
Devil's Advocate goes last.

### 5. Interactive Pause

Present key themes across all analyses. Ask:
"All personas have analyzed. Key themes: [summary]. Any reactions before the debate?"

- If `auto`: skip this pause
- If user responds: inject as context for debate
- If user says "skip debate": jump to step 7

### 6. Debate

Load [references/workflow.md](references/workflow.md) Phase 6.

Run debate rounds (1-3 per depth setting, or `--rounds N` override).
Each persona cross-examines others' findings.
Devil's Advocate rules enforced: surface unstated assumptions the panel shares, challenge the highest-confidence finding, concede with conditions when evidence is overwhelming.
Interactive pause after each round (unless `auto` mode).

### 7. Consensus

Load [references/workflow.md](references/workflow.md) Phase 7.

Synthesize findings: aggregate by theme, rank by severity/confidence, deduplicate,
note dissent. Produce ranked recommendations with rationale.

### 8. Report and Handoff

Load [references/output-template.md](references/output-template.md).

Save report to plans directory or `.fno/think-tank-{date}-{slug}.md`.
If `--chain`: invoke the chained skill with top recommendation.
If no chain: present options:
- `/think "{top recommendation}"` - design in depth
- `/blueprint full "{top recommendation}"` - jump to execution planning
- `/megawalk` - feed into roadmap
- "Done for now" - keep the report

## Key Principles

- All personas run on the main thread. No agents spawned.
- The user is a participant, not an observer. Pauses between phases.
- Findings need rationale, not just opinions.
- Devil's Advocate always challenges majority. Always.
- The report feeds downstream skills. It's not a dead document.

## Depth Presets

| Depth | Built-in Personas | Rounds | Base Time | Base Tokens |
|-------|-------------------|--------|-----------|-------------|
| shallow | 2 + DA | 1 | ~3 min | ~20K |
| standard (default) | 4 + DA | 2 | ~7 min | ~50K |
| deep | all + DA | 3 | ~12 min | ~80K |

Project default personas (from settings.yaml) and the user seat are always included regardless of depth.
Actual persona count = trimmed built-in count + project defaults + user seat + DA.
Finding limit per persona: `ceil(24 / actual_persona_count)`.
Time and token estimates scale linearly with additional personas.

## NEVER

- NEVER spawn Agent or Task tools for persona work
- NEVER skip the Devil's Advocate persona
- NEVER summarize away persona reasoning (save verbatim)
- NEVER make implementation decisions (that's /think and /plan's job)
- NEVER run for more than ~15 minutes (deep mode ceiling)
