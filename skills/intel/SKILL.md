---
name: intel
description: Session-provenance report for the user - who typed, what was satisfied, what corrected the agents, what the relay graph says. Runs the fno-agents intel fold once, judges a sampled subset, clusters the summaries into categories with per-category metrics, and writes one vault report. Use when the user says session report, who typed, user insights, or intel report.
---

# intel

The fold counts. You judge. `fno-agents intel` classifies every user-shaped turn in this machine's transcripts by provenance (the fold's `operator` class, relay, harness, keepalive, unknown). It joins sessions to nodes, PRs, and mail. It counts tokens, lines, tool errors, languages, response time, hours, and overlapping sessions. It samples idle substantive sessions and computes per-category metrics from the facets. No model runs there. This skill is the judgment layer: you read the fold's sampled rows, judge each sampled session, cluster the summaries into categories, and write the narrative.

The one rule the whole report stands on: **user turns only**. Relay, harness, keepalive, and unknown turns are other agents and machinery talking, or turns no witness can name. They never inform satisfaction, friction, or corrections. The fold's counters tell you exactly what to ignore.

The fold names its populations, and the report keeps them apart. Every number says whether it rests on `scanned` sessions or on `judged` ones. No line blends the two.

## Steps

1. Run the fold once and save its JSON. Every report number and the categories post-process read this one file. Call shape:

   ```bash
   /fno:intel [--scope <project>[,<project>...]|all] [--harness claude,codex,opencode|all] [--period 2w|1m|2m|3m|all] [--sample N|all] [question]
   # or one node's story:
   fno-agents intel --json --node <id>
   ```

(`fno doctor intel` is the same fold. The binary's full flag set, including `--session`, sits on `fno-agents intel`.) The period words map to `--days`. `1m` is the default and maps to 30 days. `2w` maps to 14 days, `2m` to 60, `3m` to 90. `all` removes the window (`--days 0`). Pass any other word nowhere: refuse it with the allowed list. The binary takes `--scope`'s meaning in two flags: `--scope all`, or no `--scope`, maps to `--all-projects` (the skill's default). Each comma entry of `--scope <project>` maps to one `--project <name>`. `--harness` passes through as `-H`. Every word after the flags is the user question. With no question, use `What were the user's sessions about, and where did they stall?`. The question key is the first 8 hex of sha256 over the question lowercased with runs of whitespace collapsed (`printf %s "$Q" | shasum -a 256 | cut -c1-8`). Default `--sample 50`. Run the fold once, under a 10-minute Bash timeout, and save its JSON beside the report:

   ```bash
   fno-agents intel --json --period 1m --project fno --sample 50 > <vault>/fno/intel/<date>-<question key>.fold.json
   ```

   The report's header quotes the fold's `scope` object, so the reader sees which harnesses and roots the fold read. Exit 3 means no sessions in the window. Report that and stop.

2. Judge the sampled sessions and write one facet file each: `~/.fno/intel/facets/<session>.json`, mode 0600. Judge only rows with `sampled: true`. They are idle and substantive by construction. Judge only the turns a session row lists in `operator_turns`. Those are the witnessed turns. The `witness` receipt names the submits, the binds, and the sessions no submit row covers. Key the facet by session id + mtime + size (all three are on the fold's session row). A session whose key matches an existing facet is not re-judged. Skip it, so a resumed session re-enters the report instead of stranding on a stale cache. A matching facet that lacks the current question key gains only that one summary line:

   ```json
   {
     "key": {"session": "...", "mtime": 0, "size": 0},
     "node": "x-...",
     "goal": "one line: what the operator was driving at",
     "outcome": "shipped | blocked | abandoned | research",
     "satisfaction": "satisfied | mixed | frustrated",
     "friction": "<category>",
     "corrections": ["verbatim operator correction", "..."],
     "summaries": {"<question key>": "one line, at most 30 words, that answers the question for this session"}
   }
   ```

   Friction categories (keep to this set so downstream scorers can key on it): `misunderstood_instruction`, `repeated_correction`, `wrong_assumption`, `missing_context`, `workflow_friction`, `tool_failure`.

3. Cluster the sampled sessions' summary lines into 3 to 8 categories that answer the question. Each category carries a name, a one-line description, and at most 5 optional subcategories. Every judged session goes into exactly one category. Use `Other` for a session that fits none. Write `~/.fno/intel/runs/<date>-<question key>.json`, mode 0600, in this schema:

   ```json
   {"schema": 1, "question": "<text>", "question_key": "<8 hex>",
    "categories": [{"name": "...", "description": "...", "sessions": ["<id>", "..."],
      "subcategories": [{"name": "...", "description": "...", "sessions": ["<id>"]}]}]}
   ```

   If that run file exists and its question key and session set match this sample, reuse it.

4. Metrics: the post-process reads the run file and the saved fold JSON. It reads no transcript. It prints the fold JSON with a categories block:

   ```bash
   fno-agents intel --categories <run file> --fold <saved fold JSON> > <vault>/fno/intel/<date>-<question key>.json
   ```

   On exit 2, fix the run file from the named reason and run it once more. If it fails again, keep the saved fold JSON as the report's `fold:` file and write the Categories section as `not written: <stderr line>`.

5. Write the report: `<vault>/fno/intel/<date>-<question key>.md`, where `<vault>/fno/` is the directory `fno do plan path` resolves beside `plans/`. Sections: [references/report-shape.md](references/report-shape.md). Every number comes from the JSON named in the frontmatter `fold:` field.

6. Corrections section: quote user corrections verbatim, dedupe across sessions, rank by repeat count. Each correction sits on its own line ending with ` #agent-correction` and carrying `signal=<friction category>`. When the correction is about how one fno verb behaves, the line also carries `skill=<name>`, that verb's skills/ directory. Each one is a candidate AGENTS.md line, a law, or a SKILL.md diff. Say which in the report.

7. Feed the S2 writer so the rows land in `~/.fno/corrections.log`:

   ```bash
   bash scripts/corrections-insights-tag.sh --insights-file <report>
   ```

   The script is watermark-idempotent: a second run adds no rows.

8. Relay section: computed from the fold's `relay` facets and `nodes` rows. No model judgment: delivery, answers, contract breaches, and silences are facts.

9. Render the shareable HTML copy. Run:

   ```bash
   fno-agents intel --render <report.md>
   ```

   Its stdout line is the HTML path. The renderer reads the report and the fold JSON named in the frontmatter `fold:` field. It scrubs secrets, home paths, quoted blocks, and quotes outside `Operator corrections`, and draws the fold counters as inline SVG. End the run by telling the user both paths, one line each: `Report: <md path>` and `Shareable copy: <html path> (open in a browser; print to PDF; latest.html beside it is always the newest)`. A nonzero exit is relayed with its stderr line. The markdown report stands either way: it stays the file every later step reads.

Judgment runs on this session's own model. No profile, no spawned reviewer, no Python shim. The fold is Rust (`fno-agents intel`), the narrative is you, and the S2 writer is the script that already existed.

## Known Limitations and Deferred Work

- User is witnessed, not inferred. When a person presses Enter in a pane or portal, the mux writes an `operator_submit` row. The fold binds turns to those rows. A turn outside the witness reads `unknown`, never `operator`. Uncovered paths: a bare terminal, the desktop apps, and claude.ai jobs. That typing never passes the mux. A hand-started harness in a shell pane writes `resolution: unresolved`, and nothing joins it. A submit queued past the 30s bind window also reads unknown. The `witness.unwitnessed_sessions` receipt counts sessions typed outside the witness.
- The relay delivered-check is a substring read: a bus body that appears verbatim in the transcript through some other channel reads as delivered even if the mail never landed in this session's turn flow.
- Opencode sessions are folded now. Their `operator` class is the same witness join as claude's (the fold binds opencode turns to `operator_submit` rows by `harness_session`). Subagent child sessions carry a `parent_id` and are excluded. When no store is readable, `skipped.opencode` names the reason.

- Full list: [LIMITATIONS.md](LIMITATIONS.md).
