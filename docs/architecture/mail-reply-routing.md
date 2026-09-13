# `fno agents mail reply` routing

`reply` resolves the answered msg-id in two places, in order.

The durable bus comes FIRST. A directed message (to_kind `name`, `session`, or `node`) answers at its original sender. The caller never re-types the handle: `in_reply_to` correlates the thread. A `node`-addressed job message is answered at its sender too. The job address is the routing key on the way IN; the reply goes back to who sent it.

Any other target falls through to the thread-store reply.

If the id is not on the bus, this session's own TRANSCRIPT is searched next. That path is the common one, not a fallback for odd cases. A live-confirmed delivery writes no durable thread, so an id that arrived live is absent from the bus by design. `resolve_live_sender` recovers the sender from the injected `<fno_mail id=...>` envelope.

Only an id absent from BOTH is a hard error. An earlier revision of `reply --help` described the bus step alone, and two agents read that as proof the verb could not answer live mail at all. The bus and the transcript are both first-class resolution surfaces; neither is a fallback for the other.
