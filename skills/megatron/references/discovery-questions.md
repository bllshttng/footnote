# Megatron discovery questions

The skill drives these five questions in order via ``AskUserQuestion``.
Each entry below is the canonical wording, allowed values, default
answer, and an example. Defaults make the easy-path one-keystroke;
the harder cases (research waves, looser failure policy) require the
user to opt in explicitly.

## Q1: Mission goal

**Prompt:** "What's the mission goal?"

**Default:** the positional argument passed to ``/megatron``.

**Format:** free text, one or two sentences.

**Example:** "Ship the new region filter end-to-end across the pipeline and
the frontend."

## Q2: Participating projects

**Prompt:** "Which projects participate?"

**Source:** ``work.workspaces.<workspace>.projects`` keys from the
user's ``settings.yaml``. Multi-select.

**Validation:** at least 2 projects (single-project missions belong
in ``/megawalk`` or ``/blueprint``, not ``/megatron``).

**Example:** ``["example-pipeline", "footnote", "acme-frontend"]``.

## Q3: Wave breakdown

**Prompt:** "Describe the waves: what runs first, second, etc.? Which
are sequential vs parallel?"

**Format:** free-form prose; the skill maps it to a structured
``waves: [...]`` list. Example mappings:

| User says                                              | Manifest waves                                              |
|--------------------------------------------------------|-------------------------------------------------------------|
| "Backend first, then frontend"                         | wave 1 sequential [backend], wave 2 sequential [frontend]   |
| "Backend, then frontend AND ops in parallel"           | wave 1 sequential [backend], wave 2 parallel [frontend, ops]|
| "Everyone at once"                                     | wave 1 parallel [...all projects]                           |

## Q4: Research wave needs

**Prompt:** "Are any waves research waves? (Research waves dispatch
discovery messages and pause for operator approval before continuing.)"

**Default:** No.

**Format:** per-wave yes/no. The skill prompts once per wave.

**Validation:** consecutive research waves are rejected. The skill
catches this at draft time (the validator catches it again at run time).

## Q5: Failure policy and autonomy

**Prompt 5a:** "Failure policy: ``block`` (pause when any participant
fails)?"

**Default:** ``block`` (the only value supported in v0).

**Prompt 5b:** "Autonomy level: ``cautious`` (escalate on cost cap,
research wave proposals)?"

**Default:** ``cautious``.

## Cancellation

If the user types ``/cancel`` at any prompt, the skill exits with:

```
Mission authoring cancelled. No changes saved.
```

No file write, no graph mutation. The user can re-run ``/megatron`` at
any time without leftover state.
