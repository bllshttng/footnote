# Model Fallback (Interactive Mode)

When the harness reports a rate-limit or overload error during execution, load this reference. It tells the user their options.

There is no `model_fallback.*` config block. Account and lane rotation already own capacity and outage handling. `fno config accounts`, the spawn-overlay grid, and `agents.profiles.*.on_exhausted` cover that. This reference covers only the one case those do not. The interactive session's own model just hit a rate limit or overload, mid-turn, in the harness the user is looking at right now.

On that error, present the options via AskUserQuestion. Name only what the error itself reported. Do not fabricate a cooldown or a next-model name from a chain that does not exist:

```
{current_model} hit a {error_type}. Options:
  1. Wait and retry with {current_model}
  2. Switch model yourself (the harness's own model picker), then continue
  3. Pause - I'll resume when you say go

Pick [1/2/3]:
```

Record the user's choice as a line in the session's progress notes. It is not a `target-state.md` field, since the manifest is write-once and holds no `model_fallback` schema. Never switch the model yourself. This session cannot log in, rename an account root, enable remote control, or select a model the current account cannot reach. Any switch is the user's own action in their harness. This skill only names the options and continues once they answer.

A phase transition never changes the model on its own. The user sets the model interactively. The resolved lane sets it unattended, via `fno agents spawn`'s grid. It stays whichever one set it, until that same actor changes it again.

In autonomous (unattended) mode there is no AskUserQuestion path. A worker spawned via `fno agents spawn` already resolved its lane through the grid at spawn time. A mid-run outage there is `run_outage_handoff`'s job (`fno.agents.outage_handoff`, invoked by `fno.recovery` on a positively proved outage), not this reference's.
