# Spawn a deepseek worker

The harness table has no `deepseek` row, so there is no verified spawn line to write here. Do not guess one.

Check what fno knows before you spawn:

1. Run `fno agents capabilities deepseek`. A row means the lane is declared. No row means `fno agents spawn -H deepseek` has no gate to pass.
2. If the model reaches you through another harness, spawn that harness and pick the model with `-m`. For opencode see [spawn-opencode.md](spawn-opencode.md).
3. Before you call a new lane supported, run `fno doctor harness deepseek --live`.

When a row lands, replace this page with the exact spawn line, the payload prefix and the known refusals, as the other five pages do.

Back to [the harness index](beat-by-harness.md#spawn-by-harness).
