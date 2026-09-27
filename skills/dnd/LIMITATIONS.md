# DND limitations

## Known Limitations and Deferred Work

- The hold verb still lives in the over-budget Python file. `fno agents mail dnd` is not a shell verb, and the hold help says "Busy mode", never DND.
- The verb holds only the session that runs it. You cannot quiet another session from here. Run the door in that session.
- `fno agents loops resume-all` run inside the held session lifts the hold, whoever armed it.
- While the hold is on, control-lane mail (`control:` prefix) queues too, because the delivery gate has no control exemption.
- The automatic conversation hold needs two things. The harness must run the fno UserPromptSubmit and Stop hooks (claude and codex today). The session must be typed through the fno mux. Harnesses without the prompt hook keep only the manual door.
- Sessions typed outside the fno mux write no typing or submit witness, so the quiet wait cannot see their drafts and C11 cannot protect them.
- A held-mail digest delivered on a mux pane keeps the Python verbatim path with real newlines; the flattened one-line typing covers the claude, keeper and pane typed lanes. cursor-agent and agy never withdraw: no landing can be seen there, so the inject types and reports unconfirmed rather than deleting the composer's contents.
- The codex question-picker manifest rule was not added: the plan's live capture (change 10) found no codex TUI pane with a picker open to record, and no shipped fixture exists. The picker signal covers claude rows only (AskUserQuestion/ExitPlanMode report blocked through the inside-leg hook); a codex picker still reads idle until a fixture lands.
