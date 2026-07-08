# Panel Workflow

Eight-phase process adapted from autoresearch-predict. Runs entirely on the main thread.

## Phase 1: Setup

Parse flags and resolve configuration.

**Entry:** User invocation with flags and decision text.
**Exit:** Resolved config: depth, persona set, chain target, autonomous flag, rounds.

If no decision text provided, use interactive setup (one batched AskUserQuestion):
- What decision are you trying to make?
- What perspective matters most? (product fit / technical feasibility / market positioning / all)
- How deep? (shallow: 3 personas 1 round / standard: 5 personas 2 rounds / deep: all personas 3 rounds)
- Chain after? (think / plan / megawalk / none)

**Defaults:** depth=standard, personas=default, chain=none, auto=false, rounds=auto (from depth).

**Continuation mode (when `continue {slug}` subcommand is present):**

1. If `continue list`: list all sessions in `.fno/think-tank-sessions/` with slug, decision, date, and top recommendation. If the directory does not exist or contains no `.json` files, respond: "No prior sessions found. Start a new session with `/think panel \"your question\"`." Then stop.
2. Load `.fno/think-tank-sessions/{slug}.json`
3. If file not found: error with "No session found for '{slug}'. Run `/think panel --continue list` to see available sessions."
4. If file exists but cannot be parsed as valid JSON: error with "Session file for '{slug}' is corrupted. Delete `.fno/think-tank-sessions/{slug}.json` and start fresh, or fix the file manually."
4. Restore panel config: same personas, same depth (can be overridden with flags like `--rounds N`)
5. Present prior state summary to user (see Phase 5.5)
6. Remaining text after `--continue {slug}` = new context to inject
7. Skip Phases 2-4 (context, personas, independent analysis already done)
8. Jump to Phase 5.5 (Continuation Round)

## Phase 2: Context Gathering

Build product context for persona prompts. This is NOT code analysis.

**Sources:**
1. Project vision and goals from config.toml (`project.vision`, `project.goals`)
2. Recent git log: `git log --oneline -20` (what's been shipped recently)
3. Existing think docs or plans in the plans directory (if any)
4. User-provided context from the invocation

**Output:** A product-context block (~200-500 tokens) injected into each persona prompt:

```
Product Context:
- Vision: {from config.toml}
- Goals: {G1-G5 from config.toml}
- Recent work: {1-line summaries of last 5-10 commits}
- Related plans: {titles of existing think/plan docs if relevant}
- User context: {any additional context from the user}
```

**NEVER:** Run code analysis, build dependency maps, or create knowledge files. Product decisions need product context, not import graphs.

### Autonomous Briefing (only when `auto` mode is active)

In autonomous mode, the human won't be available to inject ground truth mid-session. The briefing step compensates by gathering more context upfront. This runs after standard context gathering and before persona generation.

**Step 1: Extract ground truth from existing data**

Search for quantitative facts the panel will need:
- Read any existing think-tank reports in the plans directory (prior sessions on related topics)
- Read recent session state files in `.fno/think-tank-sessions/` for related decisions
- Check config.toml for project constraints, goals, and metrics
- Read the decision text carefully for embedded data ("we have X users", "pricing is $Y")

**Step 2: Identify information gaps**

Before assembling the panel, list what the panel will likely need but doesn't have:
- Market data (user count, pricing, competitors, TAM)
- Technical constraints (what's built, what's not, data density)
- Prior experiments (what's been tried, what worked)
- User behavior (engagement patterns, churn reasons, feedback themes)

**Step 3: Build enriched context block**

Extend the standard product-context block with a briefing section:

```
Autonomous Briefing:
- Known facts: {quantitative data extracted from existing docs/settings}
- Prior decisions: {relevant recommendations from past think-tank sessions}
- Information gaps: {what we don't know - panel should flag these as open questions rather than speculating}
- Constraints: {budget, timeline, team size, technical limits from config.toml}
```

**Step 4: Inject briefing as ground truth**

The enriched context block replaces user interjections. Personas should treat briefing facts as authoritative (equivalent to the human saying "actually it's 2% not 1%") and flag information gaps explicitly rather than making assumptions.

**If no enriched context can be gathered** (no prior sessions, minimal config.toml): proceed with standard context only. The panel will be less informed but the session is still useful - it will surface what questions need answering.

## Phase 3: Persona Generation

Load and configure the persona panel.

**Entry:** Resolved persona set name (default/startup/adversarial/custom).
**Exit:** List of configured persona prompts ready for analysis.

1. Load persona-templates.md
2. **Persona loading order:**
   a. If `custom` (inline YAML) with inline YAML: use custom set only (skip project defaults)
   b. If `--personas default|startup|adversarial`: load built-in set
   c. Read `.fno/config.toml` -> `think_tank.default_personas`
   d. If project defaults exist and no `custom` (inline YAML): insert project default personas BEFORE Devil's Advocate in the panel. Trim built-in personas (not project defaults) if depth preset requires fewer personas. DA is always last, project defaults are always included.
   e. If project defaults + DA exceed the depth limit, expand the depth limit to fit.
3. **User seat resolution** (see persona-templates.md "User Persona" section):
   a. If `--no-user-seat` or `auto`: no user seat, use full built-in set
   b. If `--user-role "X"`: user seat with custom name
   c. Default: user seat as "Domain Expert"
   d. Replace the appropriate built-in persona (Target User in default set, Solo Founder in startup set, Skeptical Customer in adversarial set)
   e. Mark user persona with `is_user: true` in panel config
4. Apply depth preset to trim persona count (project defaults and user seat are never trimmed)
5. Generate prompt for each simulated persona using the template
6. Announce the panel: "Panel assembled: [list all persona names including user seat and project defaults]"

## Phase 4: Independent Analysis

Each persona analyzes the decision independently.

**Entry:** List of persona prompts with product context injected.
**Exit:** Structured findings from each persona.

**Rules:**
- Process personas sequentially (this is main-thread, not parallel agents)
- Output each persona's analysis visibly as it's produced
- Finding limit per persona: `ceil(24 / persona_count)` (e.g., 5 personas = 5 findings each)
- Devil's Advocate goes last (sees other findings before producing their own). DA's primary job is to identify shared assumptions across the other personas' analyses, not to nitpick individual findings.
- Findings use severity/confidence ratings

**User seat in Phase 4:**

After all simulated personas have produced findings (but before DA), prompt the user for their analysis. Use the same structure as simulated personas:

"Your turn as {user_role_name}. Based on your domain expertise, what are your findings on this decision?

You can provide up to {finding_limit} findings. For each:
- **Title:** one-line summary
- **Severity:** CRITICAL / HIGH / MEDIUM / LOW
- **Confidence:** HIGH / MEDIUM / LOW
- **Your reasoning:** why this matters
- **Recommendation:** what to do about it

Or just share your thoughts in plain text and I'll structure them."

**User seat rules:**
- User goes AFTER other simulated personas but BEFORE DA (DA sees user findings too)
- If user provides plain text, structure it into findings format and show the user the structured version
- If user says "skip" or "pass", mark user as abstaining for this phase
- Finding limit is the same as other personas: `ceil(24 / persona_count)`
- If `--no-user-seat` or `auto`: skip this entirely

**Output per persona:**
```
### {Name} ({Abbr}) Findings

**{ABBR}-1: {Title}**
- **Severity:** HIGH | **Confidence:** MEDIUM
- {Rationale with specific evidence}
- **Recommendation:** {Concrete action}
```

## Phase 5: Interactive Pause 1

Let the user participate before debate.

**Entry:** All independent analyses complete.
**Exit:** User input (or skip) acknowledged.

Present a brief summary of key themes across all personas, then ask:
"All personas have analyzed. Key themes: [summary]. Any reactions before the debate?"

**With user seat:** Phase 5 is lighter since the user already contributed in Phase 4.
Present the key themes summary and ask:
"You've given your findings. Anything to add or correct before the debate?"
If user has nothing to add, proceed immediately. Don't repeat the full pause protocol.

**Without user seat:** Use the standard pause - this is the user's main input opportunity.

**Responses:**
- User provides input -> inject as additional context for debate round
- User says "skip debate" -> jump to Phase 7 (Consensus)
- User says nothing / continues -> proceed to debate
- `auto` flag -> skip this pause entirely

## Phase 5.5: Continuation Round (only when `continue` is active)

**Entry:** Loaded session state + new context from user.
**Exit:** Panel has processed new information and is ready for debate.

### Step 1: Present Prior State

Summarize the prior session concisely:
- Original decision and date
- Top 3 recommendations with consensus labels
- Key open questions from the prior session
- Anti-herd status from prior session

Format:
"Resuming the '{decision}' panel from {date}. Here's where we left off:

**Prior recommendations:**
1. {recommendation 1} (Confirmed, 4/5)
2. {recommendation 2} (Probable, 3/5)
3. {recommendation 3} (Minority, 2/5)

**Open questions from last time:**
- {question 1}
- {question 2}

**New context you're providing:**
'{new_context_text}'"

### Step 2: Persona Re-Analysis

Each persona (including user seat if applicable) responds to the new context in light of their prior findings:
- Does the new information change any of their prior findings?
- Does it resolve any open questions?
- Does it create new findings?

Format per persona:
```
### {Name} ({Abbr}) - Continuation Response

**Revised findings:**
- {ABBR}-1: [unchanged | revised | withdrawn] - {explanation if changed}

**New findings from new context:**
- {ABBR}-N+1: {new finding prompted by the new data}

**Open questions resolved:**
- {question}: {answer based on new data}
```

### Step 3: Proceed to Debate

After all personas have responded to the new context, proceed to Phase 6 (Debate) with:
- Prior findings (updated with revisions from Step 2)
- New findings from continuation
- New context as shared knowledge

Debate rounds are controlled by `--rounds` flag or depth preset.
Default continuation depth is 1 round regardless of original session depth - the user can override with `--rounds N`.

---

## Phase 6: Debate

Personas cross-examine each other's findings.

**Entry:** All independent analyses + optional user input from Phase 5.
**Exit:** Revised positions, resolved disagreements, highlighted irreconcilable differences.

**Per round:**
1. Each persona sees ALL other personas' findings
2. Each persona can: challenge a finding, support a finding, revise their own position, raise a new point
3. Devil's Advocate enforcement: surface the highest-confidence assumption the panel shares but hasn't questioned. Challenge the most-agreed-upon finding. Concede with conditions when evidence is overwhelming rather than blanket disagreement. MUST raise one non-obvious angle per round.
4. Output each persona's debate contribution visibly

**Rounds:** Determined by depth preset:
- shallow: 1 round
- standard: 2 rounds
- deep: 3 rounds
- Override with `--rounds N`

**User seat in debate:**

At the end of each debate round (after all simulated personas have responded), give the user a turn:

"Round {N} debate complete. As {user_role_name}, you can:
- Challenge a specific persona's position
- Provide new data that changes the picture
- Revise your own findings from Phase 4
- Say 'pass' to skip this round"

User input is integrated as debate contributions with the same format as simulated personas.
The DA then responds to the user's contributions just as it would to any other persona.
If `--no-user-seat` or `auto`: skip user debate turns.

**Interactive pause after each round** (unless `auto`):
"Round {N} complete. [brief summary of movement]. Want to add context or redirect?"

## Phase 7: Consensus

Synthesize findings into ranked recommendations.

**Entry:** All analyses and debate contributions.
**Exit:** Ranked recommendation list with rationale and confidence.

**Synthesizer process:**

### Step 1: Structured Voting

After debate completes, each persona votes on every unique finding (deduplicated by title similarity):

| Vote | Meaning |
|------|---------|
| `confirm` | Persona agrees the finding is valid |
| `dispute` | Persona disagrees or thinks it's overstated |
| `abstain` | Finding is outside their domain |

### Step 2: Consensus Thresholds

| Votes Confirming | Label |
|-----------------|-------|
| >= ceil(persona_count * 0.6) | **Confirmed** |
| >= ceil(persona_count * 0.4) | **Probable** |
| >= 1 persona | **Minority** |
| 0 personas | **Discarded** |

Abstentions reduce the denominator - threshold is calculated against non-abstaining personas. If all personas abstain on a finding, set `consensus_ratio` to 0.0 and label as Discarded. If a finding is missing severity or confidence (e.g., from unstructured user input), default to MEDIUM for both.

### Step 3: Priority Ranking

Each finding receives a composite priority score:

```
priority_score = severity_weight * 0.4 + confidence_boost * 0.2 + consensus_ratio * 0.4

Where:
  severity_weight = CRITICAL:4, HIGH:3, MEDIUM:2, LOW:1
  confidence_boost = HIGH:1.0, MEDIUM:0.6, LOW:0.3
  consensus_ratio  = personas_confirmed / (personas_total - personas_abstaining)
```

Sort findings descending by priority_score to produce ranked recommendations.

Scores range from 0.5 (LOW severity, LOW confidence, consensus_ratio 0) to 2.2 (CRITICAL severity, HIGH confidence, consensus_ratio 1.0). Worked example: a CRITICAL finding (severity_weight 4) at HIGH confidence (confidence_boost 1.0) confirmed by 3 of 4 non-abstaining personas (consensus_ratio 0.75) scores `4*0.4 + 1.0*0.2 + 0.75*0.4 = 2.1`. Example scores below must stay within this range.

### Step 4: Preserve Dissent

For every finding, record which personas disputed and their rationale. Minority findings are preserved in the report - never suppress them.

### Step 5: Anti-Herd Detection

Measure three signals after voting completes:

| Signal | Formula | Threshold |
|--------|---------|-----------|
| `flip_rate` | Findings where persona changed position during debate / total findings | > 0.8 = suspicious |
| `entropy` | Shannon entropy of final vote distribution across all findings | < 0.3 = suspicious |
| `convergence_speed` | Rounds needed to reach >= 80% agreement | 1 round = suspicious |

**GROUPTHINK WARNING** triggered when: `(flip_rate > 0.8 AND entropy < 0.3) OR (convergence_speed == 1 AND depth != shallow)`

Response to groupthink detection:
1. Preserve ALL minority findings in the report - do not discard them
2. Flag in report header: "Anti-Herd: GROUPTHINK WARNING - high convergence detected. Minority findings may be underweighted."
3. Suggest user re-run with `--personas adversarial` for more diverse perspectives

When no groupthink: "Anti-Herd: PASSED"

Note: In shallow mode (1 debate round), convergence_speed = 1 is expected and is NOT automatically suspicious. This exception applies to original shallow sessions, not to continuation rounds that default to 1 round. If debate was skipped entirely (zero rounds), set convergence_speed to null and exclude it from groupthink evaluation, noting "convergence_speed: N/A (debate skipped)." Groupthink requires BOTH flip_rate > 0.8 AND entropy < 0.3 to trigger.

**Output:**
```
## Consensus Summary

| Finding | Consensus | Priority | Confirmed By | Disputed By |
|---------|-----------|----------|--------------|-------------|
| {title} | Confirmed | 2.1 | PM, Designer, TU | Developer |

## Recommendations

### Recommendation 1: {action}
**Priority Score:** 2.1 | **Consensus:** Confirmed (3/5)

| Persona | Vote | Note |
|---------|------|------|
| PM | confirm | Aligns with user research |
| Designer | confirm | - |
| Developer | dispute | Implementation cost underestimated |
| Target User | confirm | - |
| Devil's Advocate | abstain | Outside scope of challenge |

{Rationale synthesized from supporting analyses}

### Recommendation 2: {action}
...

### Open Questions
- {Things the panel couldn't resolve}
```

## Phase 8: Report and Handoff

Save the report and offer next steps.

**Entry:** Complete analysis, debate, and consensus.
**Exit:** Saved report + optional chain invocation.

1. Generate report using output-template.md format
2. Determine save path:
   - If plan_path exists in target-state.md: save alongside existing plan
   - Otherwise: save to `.fno/think-tank-{date}-{slug}.md`
3. **Session state save (AUTO):**
   a. Generate slug from decision text: lowercase, replace spaces/punctuation with hyphens, truncate to 40 chars (see references/session-state.md for full rules)
   b. If session was loaded via `continue`, use the existing slug (overwrite)
   c. Create `.fno/think-tank-sessions/` directory if it doesn't exist
   d. Write session state to `.fno/think-tank-sessions/{slug}.json` with all findings, votes, anti-herd metrics, recommendations, user contributions, and continuation history
   e. See references/session-state.md for the full JSON schema
4. Present handoff options:
   - `/think "{top recommendation}"` - design the recommendation in depth
   - `/blueprint full "{top recommendation}"` - jump to execution planning
   - `/megawalk` - feed into roadmap generation
   - `/think panel continue {slug}` - resume this debate later with new information
   - "Done for now" - just keep the report
5. If `--chain` flag was set: invoke the chained skill automatically
6. If `auto`: skip handoff prompt, just save report and session state
