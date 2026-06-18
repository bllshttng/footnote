# Research Protocol

Pre-execution phase that dispatches read-only workers to investigate the codebase
before implementation begins. Research findings feed into the synthesis protocol,
producing richer implementation prompts with fewer retries.

## Activation Conditions

ALL of these must be true:
1. Scratchpad exists: `.fno/scratchpad/research/` directory present
2. Plan is a folder plan (has 00-INDEX.md, not a single .md file)
3. Plan has 2 or more phase files (01-*.md, 02-*.md, etc.)

If ANY condition is false: skip research, proceed to wave execution.

**Override:** The `--research` flag forces research phase even when conditions
are not fully met (e.g., single-phase plan where you still want codebase
investigation before implementation).

## Worker Dispatch

For each phase file in the plan, construct a Task tool invocation:

- **Agent:** archer
- **Tools override:** `["Read", "Grep", "Glob"]` (no Write, Edit, or Bash)
- **Run in parallel:** Dispatch ALL workers in a single message with multiple Task tool calls

### Prompt Template

```
You are investigating the codebase for phase {NN}: {phase_title}.
DO NOT write code, create files, or modify anything.

Read the files listed in the tasks below and report:
1. What each file currently does and what patterns it follows
2. Existing utilities or helpers that implementation can reuse
3. What could break when these files are modified
4. Anything the implementation worker needs to know

Tasks in this phase:
{task details with file paths from the phase file}

Return your findings in this format (the orchestrator will write them to scratchpad):
## Phase {NN}: {title} - Research Findings
### Files Examined
- `path/to/file`: [what it does, key functions, patterns]
### Reusable Patterns
- [pattern]: found at [location], can be reused for [purpose]
### Risk Areas
- [risk]: [what could break and why]
### Integration Notes
- [anything the implementation worker needs to know]
```

### Writing Findings to Scratchpad

Workers return findings as text output (they have no Write tool).
The orchestrator captures each worker's return text and writes it to scratchpad:

```bash
# After each worker completes, write its output to scratchpad
# $SCRATCHPAD/research/phase-{NN}-findings.md
```

## Collecting Results

- Wait for all workers to complete
- For each worker: capture return text, write to scratchpad findings file
- If a worker failed or returned empty: log which phase had no research, continue
- Research failures never block implementation

## Synthesis After Research

After reading all findings, apply the existing synthesis protocol:

1. Read all `research/phase-*-findings.md` files from scratchpad
2. Follow the five-point synthesis checklist in `references/synthesis-checklist.md`
3. For each task in wave execution, enrich the implementation prompt with:
   - File paths and current state (from research findings)
   - Reusable patterns the worker should follow
   - Risk areas to watch out for
   - Integration notes from adjacent phases

Research findings are INPUT to synthesis, not a replacement for it.
The synthesis checklist remains the governing standard.

## Coordinator State

Update `coordinator_phase` in target-state.md:
- Set to `research` before dispatching research workers
- Set to `execution` when research completes and waves begin

On resume: if `coordinator_phase: research`, re-run research from scratch
(research workers are idempotent - they overwrite findings files).

## Cost Note

- Research workers are read-only and typically use 5-15K tokens each
- For a 4-phase plan, research phase costs ~40-60K tokens total
- This investment pays back through fewer implementation failures and retries
