# DND limitations

## Known Limitations and Deferred Work

- `fno agents mail dnd` is not a shell verb and the hold help says "Busy mode" but never DND, until the hold verb is ported out of the over-budget Python file.
- The verb holds only the session that runs it. You cannot quiet another session from here; run the door in that session.
- `fno agents loops resume-all` run inside the held session lifts the hold, whoever armed it.
- Control-lane mail (`control:` prefix) queues too while the hold is on, because the delivery gate has no control exemption.
