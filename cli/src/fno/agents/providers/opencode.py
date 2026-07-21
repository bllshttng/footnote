"""fno.agents.providers.opencode - what teardown can and cannot do.

opencode is a pane-hosted provider (``READABLE_PROVIDERS``): fno never
drives it through a Python ask adapter, so this module carries only what
``fno agents rm`` needs to reason about opencode's session store.

**There is no record-only teardown for opencode, so ``rm`` does not
attempt one.** Verified against opencode v1.14.50:

- ``opencode db`` opens the store READ-ONLY -- a ``delete`` is rejected
  with "attempt to write a readonly database" (exit 1). It is a query
  surface, not a mutation one.
- ``opencode session delete <id>`` is the only supported deletion verb,
  and it is not record-only: deleting a session also deletes its CHILD
  sessions and every message row. The store enforces part of this in
  schema (``message.session_id`` is ``ON DELETE CASCADE``), so no
  direct-sqlite variant can remove the record while keeping the
  conversation either -- the history is keyed to the record being
  removed.

The JSON tree under ``storage/`` does NOT soften this, which is the
obvious objection and worth answering: it is the legacy pre-sqlite store,
left in place by the one-time migration (``storage/migration`` holds its
version) and no longer written. Driving a real session against a fresh
data dir persists the message and part rows to sqlite and creates no
``storage/`` tree at all; on a migrated store, a pre-migration session's
file count matches its message-row count exactly, because the migration
copied rather than moved. So a session created since the migration has no
on-disk copy to fall back to.

Expect the vendor docs to disagree. The troubleshooting page still
describes session and message data as files under
``project/<slug>/storage/`` and never mentions sqlite, while v1.14.50
announces a one-time sqlite migration on first run and writes nothing to
any of those paths. This module follows the binary's observed behavior,
not the page.

That makes opencode teardown irreversible destruction of conversation
history, which is a different act from the index-record cleanup ``rm``
performs for codex. ``rm`` therefore drops the registry row only and
says so, leaving the deletion to a deliberate operator command.

:func:`is_session_id` stays because callers still validate ids, and the
constant below is the message ``rm`` prints so the wording lives with
the reasoning rather than in the dispatch layer.
"""
from __future__ import annotations

import re as _re

_SESSION_ID_RE = _re.compile(r"ses_[0-9A-Za-z]+\Z")

# Printed by `fno agents rm` for an opencode row. Names the escape hatch,
# because "we did not do it" is only useful with "here is how, if you want it".
REGISTRY_ONLY_NOTE = (
    "opencode session record left in place: deleting it would also delete "
    "the session's child sessions and its full message history, which `rm` "
    "will not do implicitly. Run `opencode session delete {sid}` to remove "
    "the conversation."
)


def is_session_id(value: str) -> bool:
    """True iff ``value`` is a well-formed opencode session id.

    ``ses_`` + ASCII alphanumerics. Mirrors the Rust probe's
    ``is_opencode_session_id`` so the two languages agree on what
    addresses an opencode session.
    """
    return isinstance(value, str) and bool(_SESSION_ID_RE.fullmatch(value))
