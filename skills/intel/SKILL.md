---
name: intel
description: Session-provenance report for the operator - who typed, what was satisfied, what corrected the agents, what the relay graph says. Runs the fno-agents intel fold, judges operator turns only, writes one vault report, feeds the S2 corrections writer. Use when the operator says session report, who typed, operator insights, or intel report.
---

# intel

The fold counts. You judge. `fno-agents intel` classifies every user-shaped turn in this machine's transcripts by provenance (operator, relay, harness, keepalive, unknown). It joins sessions to nodes, PRs, and mail, and computes the relay facets. No model runs there. This skill is the judgment layer: you read the fold's operator turns and write the narrative.

The one rule the whole report stands on: **operator turns only**. Relay, harness, keepalive, and unknown turns are other agents and machinery talking, or turns no witness can name. They never inform satisfaction, friction, or corrections. The fold's counters tell you exactly what to ignore.

## Steps

1. Run the fold:

   ```bash
   fno-agents intel [--json] [--period 2w|1m|2m|3m|all] [--scope <project>[,<project>...]|all] [--harness claude,codex,opencode|all]
   # or one node's story:
   fno-agents intel --json --node <id>
   ```

   (`fno doctor intel` is the same fold. The binary's full flag set, including `--session`, sits on `fno-agents intel`.) The period words map to `--days`. `1m` is the default and maps to 30 days. `2w` maps to 14 days, `2m` to 60, `3m` to 90. `all` removes the window (`--days 0`). Pass any other word nowhere: refuse it with the allowed list. The binary takes `--scope`'s meaning in two flags: `--scope all`, or no `--scope`, maps to `--all-projects` (the skill's default). Each comma entry of `--scope <project>` maps to one `--project <name>`. `--harness` passes through as `-H`. One worked example: `fno-agents intel --json --period 2w --project fno -H codex`. The report's header quotes the fold's `scope` object, so the reader sees which harnesses and roots the fold read. Exit 3 means no sessions in the window. Report that and stop.

2. Judge the attended sessions and write one facet file each: `~/.fno/intel/facets/<session>.json`, mode 0600. Judge only the turns a session row lists in `operator_turns`. Those are the witnessed turns. The `witness` receipt names the submits, the binds, and the sessions no submit row covers. Key the facet by session id + mtime + size (all three are on the fold's session row). A session whose key matches an existing facet is not re-judged. Skip it, so a resumed session re-enters the report instead of stranding on a stale cache:

   ```json
   {
     "key": {"session": "...", "mtime": 0, "size": 0},
     "node": "x-...",
     "goal": "one line: what the operator was driving at",
     "outcome": "shipped | blocked | abandoned | research",
     "satisfaction": "satisfied | mixed | frustrated",
     "friction": "<category>",
     "corrections": ["verbatim operator correction", "..."]
   }
   ```

   Friction categories (keep to this set so downstream scorers can key on it): `misunderstood_instruction`, `repeated_correction`, `wrong_assumption`, `missing_context`, `workflow_friction`, `tool_failure`.

3. Write the report: `<vault>/fno/intel/<date>.md`, where `<vault>/fno/` is the directory `fno do plan path` resolves beside `plans/`. Sections: [references/report-shape.md](references/report-shape.md).

4. Corrections section: quote operator corrections verbatim, dedupe across sessions, rank by repeat count. Each correction sits on its own line ending with ` #agent-correction` and carrying `signal=<friction category>`. When the correction is about how one fno verb behaves, the line also carries `skill=<name>`, that verb's skills/ directory. Each one is a candidate AGENTS.md line, a law, or a SKILL.md diff. Say which in the report.

5. Feed the S2 writer so the rows land in `~/.fno/corrections.log`:

   ```bash
   bash scripts/corrections-insights-tag.sh --insights-file <report>
   ```

   The script is watermark-idempotent: a second run adds no rows.

6. Relay section: computed from the fold's `relay` facets and `nodes` rows. No model judgment: delivery, answers, contract breaches, and silences are facts.

Judgment runs on this session's own model. No profile, no spawned reviewer, no Python shim. The fold is Rust (`fno-agents intel`), the narrative is you, and the S2 writer is the script that already existed.

## Known Limitations and Deferred Work

- Operator is witnessed, not inferred. When a person presses Enter in a pane or portal, the mux writes an `operator_submit` row. The fold binds turns to those rows. A turn outside the witness reads `unknown`, never `operator`. Uncovered paths: a bare terminal, the desktop apps, and claude.ai jobs. That typing never passes the mux. A hand-started harness in a shell pane writes `resolution: unresolved`, and nothing joins it. A submit queued past the 30s bind window also reads unknown. The `witness.unwitnessed_sessions` receipt counts sessions typed outside the witness.
- The relay delivered-check is a substring read: a bus body that appears verbatim in the transcript through some other channel reads as delivered even if the mail never landed in this session's turn flow.
- Opencode sessions are folded now. Their operator class is the same witness join as claude's (the fold binds opencode turns to `operator_submit` rows by `harness_session`). Subagent child sessions carry a `parent_id` and are excluded. When no store is readable, `skipped.opencode` names the reason.

- Full list: [LIMITATIONS.md](LIMITATIONS.md).
