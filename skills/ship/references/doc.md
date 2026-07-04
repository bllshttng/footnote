# ship doc

Ship a research **doc** deliverable to its finish line: a cited brief written to `config.research.output_dir`, with a mechanical green from `fno evals grade`. This is the second ship type that earns the `/ship` umbrella - it has a definable green, the same way `pr` does.

The green for a doc is the three model-free assertions `fno evals grade` makes: (a) zero uncited claims, (b) zero dead source URLs, (c) at least one golden checklist item satisfied per section. No model sits in this gate; the research-verify panel is advisory and never changes the verdict.

**Delivery is mandatory.** "Ship" means the brief lands in `config.research.output_dir`. This mode always delivers there before it reports anything - it never grades or reports GREEN against a brief that was not shipped to `output_dir`. `--no-deliver` (a `fno research` flag that writes only the local research cache) is therefore rejected here: it would let `/ship doc` report a finish line that does not exist.

## Arguments

```
/ship doc <topic> [--golden <discovery-*.md>] [extra fno research flags except --no-deliver]
```

- **`<topic>`** (required, a plain string) -> the subject to research and ship.
- **`--golden <path>`** -> the golden `discovery-*.md` doc to grade against. Required to produce the green (the grade needs a golden). Without it, the brief is delivered but reported ungraded.

## Step 0: argument guard

If no non-flag topic argument is given (only flags, or nothing), do NOT call `fno research` with an empty topic. Print and stop with a non-zero result:

```
/ship doc needs a topic. usage:
  /ship doc "<topic>" [--golden <discovery-*.md>]
```

If `--no-deliver` is among the arguments, reject and stop with a non-zero result:

```
/ship doc always delivers to config.research.output_dir; --no-deliver is not allowed.
to retrieve sources without shipping, use `fno research <topic> --no-deliver` directly.
```

## Step 1: deliver the brief

Run, with delivery on (the default; never pass `--no-deliver`):

```bash
fno research "<topic>" [other allowed flags]
```

- On success, `fno research` writes `<slug>.md` + `<slug>.sources.jsonl` to `config.research.output_dir` and reports `DoneAdvisory`. Read the printed output path; set `BRIEF` to `<output_dir>/<slug>.md` and `SIDECAR` to the sibling `<slug>.sources.jsonl`.
- If `config.research.output_dir` is unset, `fno research` exits 5 (`OutputDirUnset`) and never guesses a path. Surface that verbatim and stop - the operator must set `config.research.output_dir` first. Do NOT fabricate a landing path.
- On any other non-zero exit (network failure, invalid flag, retrieval error), STOP immediately and surface the error verbatim. Do NOT proceed to Step 2 against a brief that was not written - there is nothing shipped to grade.

## Step 2: grade (the green)

If `--golden <path>` was given, set `GOLDEN` to that path and run:

```bash
fno evals grade --brief "$BRIEF" --golden "$GOLDEN" [--sidecar "$SIDECAR"]
```

Report the verdict by exit code:

- **0** -> GREEN. The doc is shipped: delivered to `output_dir` AND eval-green. This is the doc finish line (`DoneAdvisory`, green). Exit 0.
- **1** -> RED. The brief is delivered but failed one or more assertions. Report which (uncited claims / dead URLs / missing golden coverage) from the grader's output. The deliverable exists; the green does not. Do NOT report success; stop with a non-zero result.
- **2** -> setup error (missing brief / golden / sidecar). Surface it and stop with a non-zero result; fix the inputs and re-run.

If `--golden` was NOT given: the brief is delivered (`DoneAdvisory`) but **not eval-graded**. This is an INCOMPLETE finish line, not a success: report it as ungraded and stop with a non-zero result so no caller mistakes "delivered" for "green":

```
doc delivered to <output_dir>/<slug>.md (ungraded - not a green finish line).
to assert the green, re-run with: /ship doc "<topic>" --golden <discovery-*.md>
```

## Step 3: report

State, in one line, the deliverable path and the green verdict (GREEN / RED / ungraded). The brief and its sidecar in `config.research.output_dir` are the artifact; the grade is the finish line. Only a GREEN grade is a successful (exit 0) ship.
