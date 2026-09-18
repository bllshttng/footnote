---
name: intel
description: Session-provenance report for the operator - who typed, what was satisfied, what corrected the agents, what the relay graph says. Runs the fno-agents intel fold, judges operator turns only, writes one vault report, feeds the S2 corrections writer. Use when: 'session report', 'who typed', 'operator insights', 'intel report'.
---

# intel

The fold counts; you judge. `fno-agents intel` classifies every user-shaped turn in this machine's transcripts by provenance (operator, relay, harness, keepalive), joins sessions to nodes, PRs, and mail, and computes the relay facets. No model runs there. This skill is the judgment layer: you read the fold's operator turns and write the narrative.

The one rule the whole report stands on: **operator turns only**. Relay, harness, and keepalive turns are other agents and machinery talking. They never inform satisfaction, friction, or corrections. The fold's counters tell you exactly what to ignore.

## Steps

1. Run the fold:

   ```bash
   fno-agents intel --json --days 14 --all-projects
   # or one node's story:
   fno-agents intel --json --node <id>
   ```

   (`fno doctor intel` is the same fold with the same flags.) Exit 3 means no sessions in the window; report that and stop.

2. Judge the attended sessions and write one facet file each: `~/.fno/intel/facets/<session>.json`, mode 0600. Key the facet by session id + mtime + size (all three are on the fold's session row); a session whose key matches an existing facet is not re-judged - skip it, so a resumed session re-enters the report instead of stranding on a stale cache:

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

4. Corrections section: quote operator corrections verbatim, dedupe across sessions, rank by repeat count, each on its own line ending with ` #agent-correction` and carrying `signal=<friction category>`. Each one is a candidate AGENTS.md line or law; say which in the report.

5. Feed the S2 writer so the rows land in `~/.fno/corrections.log`:

   ```bash
   bash scripts/corrections-insights-tag.sh --insights-file <report>
   ```

   The script is watermark-idempotent: a second run adds no rows.

6. Relay section: computed from the fold's `relay` facets and `nodes` rows. No model judgment: delivery, answers, contract breaches, and silences are facts.

Judgment runs on this session's own model. There is no profile, no spawned reviewer, no Python shim: the fold is Rust (`fno-agents intel`), the narrative is you, and the S2 writer is the script that already existed.
