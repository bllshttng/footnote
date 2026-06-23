# ship doc

Ship a research **doc** deliverable to its finish line: a cited brief written to `config.research.output_dir`, with a mechanical green from `fno evals grade`. This is the second ship type that earns the `/ship` umbrella - it has a definable green, the same way `pr` does.

The green for a doc is the three model-free assertions `fno evals grade` makes: (a) zero uncited claims, (b) zero dead source URLs, (c) at least one golden checklist item satisfied per section. No model sits in this gate; the research-verify panel is advisory and never changes the verdict.

## Arguments

```
/ship doc <topic | brief.md> [--golden <discovery-*.md>] [--no-deliver] [extra fno research flags]
```

- **`<topic>`** (a plain string) -> research it now: run `fno research`, which retrieves sources, stores them, and (unless `--no-deliver`) ships `<slug>.md` + `<slug>.sources.jsonl` to `config.research.output_dir`.
- **`<brief.md>`** (a path to an existing `.md` brief) -> skip research; grade the brief that already exists.
- **`--golden <path>`** -> the golden `discovery-*.md` doc to grade against. Required to produce the green (grade needs a golden). Without it, the brief is delivered but reported ungraded.

## Step 1: deliver (or locate) the brief

If the first non-flag argument is an existing file path ending in `.md`, treat it as a pre-built brief; set `BRIEF` to that path and skip to Step 2.

Otherwise treat the non-flag arguments as the research topic and run:

```bash
fno research "<topic>" [--no-deliver] [extra flags]
```

- On delivery (the default), `fno research` writes `<slug>.md` + `<slug>.sources.jsonl` to `config.research.output_dir` and reports `DoneAdvisory`. Read the printed output path; set `BRIEF` to `<output_dir>/<slug>.md` and `SIDECAR` to the sibling `<slug>.sources.jsonl`.
- If `config.research.output_dir` is unset, `fno research` exits 5 (`OutputDirUnset`) and never guesses a path. Surface that verbatim and stop - the operator must set `config.research.output_dir` first. Do NOT fabricate a landing path.

## Step 2: grade (the green)

If `--golden <path>` was given, run:

```bash
fno evals grade --brief "$BRIEF" --golden "<golden>" [--sidecar "$SIDECAR"]
```

Report the verdict by exit code:

- **0** -> GREEN. The doc is shipped: delivered to `output_dir` AND eval-green. This is the doc finish line (`DoneAdvisory`, green).
- **1** -> RED. The brief is delivered but failed one or more assertions. Report which (uncited claims / dead URLs / missing golden coverage) from the grader's output. The deliverable exists; the green does not. Do NOT report success.
- **2** -> setup error (missing brief / golden / sidecar). Surface it and stop; fix the inputs and re-run.

If `--golden` was NOT given: the brief is delivered (`DoneAdvisory`) but **not eval-graded**. Report exactly that, and that the green requires a golden:

```
doc delivered to <output_dir>/<slug>.md (ungraded).
to assert the green, re-run with: /ship doc <brief.md> --golden <discovery-*.md>
```

## Step 3: report

State, in one line, the deliverable path and the green verdict (GREEN / RED / ungraded). The brief and its sidecar in `config.research.output_dir` are the artifact; the grade is the finish line.
