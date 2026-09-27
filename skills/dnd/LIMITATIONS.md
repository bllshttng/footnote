# DND limitations

## Known Limitations and Deferred Work

- The hold verb still lives in the over-budget Python file. `fno agents mail dnd` is not a shell verb, and the hold help says "Busy mode", never DND.
- The verb holds only the session that runs it. You cannot quiet another session from here. Run the door in that session.
- `fno agents loops resume-all` run inside the held session lifts the hold, whoever armed it.
- While the hold is on, control-lane mail (`control:` prefix) queues too, because the delivery gate has no control exemption.
- The automatic conversation hold needs a harness whose UserPromptSubmit and Stop hooks run the fno entries (claude and codex today) and a session typed through the fno mux. Harnesses without the prompt hook keep only the manual door.
