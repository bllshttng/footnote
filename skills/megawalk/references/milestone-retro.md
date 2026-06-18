# Milestone Retrospectives

Automatic think-tank reviews that run mid-roadmap to catch priority drift and course-correct. Uses shallow depth to keep cost low (~20K tokens per retro).

## Trigger Conditions

Check after each successful task:

### 1. Count-Based

```
tasks_completed_since_last_retro >= milestone_interval
```

Default `milestone_interval`: 3. Configurable in settings.yaml.

### 2. Boundary-Based

```
All tasks in the current dependency tier are complete
```

Parse the roadmap dependency graph. If all tasks in the current "tier" (tasks with no unfinished predecessors) are done, trigger a retro. This catches natural milestone boundaries (e.g., "all backend done, starting frontend").

Either trigger fires the retro. Reset counter after retro completes.

## Retro Execution

```
1. Gather context:
   - Tasks completed since last retro (titles, PRs, costs)
   - Any failed or blocked tasks
   - Discovery briefs from completed tasks
   - Remaining backlog (titles, priorities, dependencies)

2. Run think-tank:
   /think panel --autonomous --depth shallow
   Decision: "Review the last N shipped tasks against the roadmap.
             What's working? What's off track? Should priorities change?"
   Context: [task summaries, briefs, remaining backlog]

3. Parse consensus:
   - Priority changes: map to roadmap-tasks.py reprioritize calls
   - Missing work: report to user, do NOT add tasks
   - Risk flags: log to roadmap-state.md decision log
   - "On track" signal: log and continue

4. Update state:
   - Apply priority changes via roadmap-tasks.py reprioritize
   - Append retro summary to roadmap-state.md under ## Retrospectives
   - Reset tasks_completed_since_last_retro counter
   - Log: "Milestone retro complete. {changes made or 'no changes'}."
```

## Retro Constraints

- **Can reprioritize:** reorder remaining tasks based on new information
- **Cannot add tasks:** new work requires user to update vision and regenerate
- **Cannot remove tasks:** user must explicitly defer (`--defer ID`)
- **Cannot change scope:** retro reviews execution, not vision

This prevents scope creep from autonomous operation. The council advises, the user decides on scope changes.

## Retro Output Format

Appended to roadmap-state.md:

```markdown
## Retrospective (after task {N})

**Date:** {date}
**Tasks reviewed:** {list}
**Consensus:**
- {recommendation 1}
- {recommendation 2}
**Priority changes:**
- Task {X}: high -> medium (reason: {rationale})
- Task {Y}: medium -> high (reason: {rationale})
**Suggestions (requires user action):**
- Council suggests: {new feature idea} (update vision to add)
```

## Configuration

```yaml
config:
  megawalk:
    milestone_interval: 3      # Tasks between retros
    retro_depth: shallow        # shallow | standard (deep is overkill for retros)
    retro_on_boundary: true     # Trigger at dependency boundaries
```

## Cost

~20K tokens per retro (shallow think-tank: 3 personas, 1 round). For a 12-task roadmap with interval 3, that's 4 retros = ~80K tokens total. Small compared to the execution cost of 12 target runs.
