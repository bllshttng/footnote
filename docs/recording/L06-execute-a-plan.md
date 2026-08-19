# L06: Execute a plan

**Medium:** Asciinema cast

**The one thing:** Do executes a focused plan in one session or follows declared waves, and its explicit state survives a context reset.

## Setup state

Run the shared setup in [README.md](README.md). Put one small executable plan at `$DEMO_ROOT/plans/demo-plan.md` and one multi-wave plan at `$DEMO_ROOT/plans/demo-waves.md` before recording.

## 1. Create disposable execution state

```run
fno state init --output "$DEMO_ROOT/scratch-target-state.md" --force
```

```expected
created: /Users/Shared/footnote-recording-demo/scratch-target-state.md
```

## 2. Bind the plan once

```run
fno state set --path "$DEMO_ROOT/scratch-target-state.md" --field plan_path --value "$DEMO_ROOT/plans/demo-plan.md"
fno state show --path "$DEMO_ROOT/scratch-target-state.md" --field plan_path
```

```expected
set plan_path = '/Users/Shared/footnote-recording-demo/plans/demo-plan.md'
/Users/Shared/footnote-recording-demo/plans/demo-plan.md
```

## 3. Execute a focused plan

```run
/fno:do flat "$DEMO_ROOT/plans/demo-plan.md"
```

[capture-at-record]

## 4. Execute declared waves

```run
/fno:do waves "$DEMO_ROOT/plans/demo-waves.md"
```

[capture-at-record]

## 5. Resume after a context reset

```run
/fno:do waves "$DEMO_ROOT/plans/demo-waves.md"
```

[capture-at-record]

The second invocation must read the existing wave state and continue at the first incomplete task. If it repeats a completed task, restart the take.

## Cut list

- Keep state creation and the first plan-path fill uncut.
- Compress executor work, but keep each task boundary and verification result visible.
- Keep the context reset and resumed task selection at normal speed.
- Cut no repeated task. A repeated completed task means the rehearsal failed.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L06-execute-a-plan.cast
asciinema upload L06-execute-a-plan.cast
```

[capture-at-record]
