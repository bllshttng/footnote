# Model Fallback (Interactive Mode)

**Load when:** the API returns a rate-limit or overload error during execution and you need to decide whether to wait, switch model, or pause.

If the API returns a rate limit or overload error during execution:

1. Read `model_fallback.chain` from settings.yaml (default: `[claude-opus-4-6, claude-sonnet-4-6, claude-haiku-4-5]`)
2. Read `model_fallback.cooldown_seconds` (default: 60)

Present to user via AskUserQuestion:

```
Claude {current_model} hit a {error_type}. Options:
  1. Wait {cooldown}s and retry with {current_model}
  2. Switch to {next_model} and continue
  3. Pause - I'll resume when you say go

Pick [1/2/3]:
```

Update `target-state.md` `model_fallback` section with the choice. If user picks 2, update `model_fallback.fallback_index` and `model_fallback.current_model`.

**Note:** In interactive mode, the model switch is advisory - Claude Code itself manages model selection. The tracking helps the user understand what happened and informs the stop hook's status messages.

In autonomous (unattended) mode, there is no AskUserQuestion path - the loop chooses based on `model_fallback.policy` in settings.yaml (`wait_then_switch` is the default).
