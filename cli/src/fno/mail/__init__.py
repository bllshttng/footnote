"""fno mail: the durable polled mailbox CLI surface (ab-cee91152).

Messaging extracted from ``fno agents`` (send) and ``fno inbox`` (receive) into
one namespace over the jsonl-canon bus log. The render/data layer stays in
``fno.inbox`` (store, drain, triage, settings); this package owns only the
CLI verbs. ``fno agents`` is lifecycle-only; ``fno inbox`` is retired.
"""
