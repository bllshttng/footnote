"""The in-memory hold-verdict adapter the src-tree tests share.

The verdict answers from the graph on disk (one fno-agents receipt); tests
that build in-memory graphs ship their rows in the payload instead. Lives
under tests/ so the test-infrastructure lines stay out of the production
tally.
"""

from __future__ import annotations

import json as _json



def install(tmp_path, monkeypatch):
    """Patch the ladder's verdict to ship this call's rows in the payload."""
    from fno.graph import ladder
    from fno.rust_binary import call_binary_json as _real_call

    real = ladder.dispatch_hold_verdict

    def patched(entry, by_id):
        rows = list(by_id.values())
        if isinstance(entry, dict) and entry not in rows:
            rows = rows + [entry]
        rows = [e for e in rows if isinstance(e, dict) and e.get("id")]

        def seeded_call(verb, args, *, timeout=None):
            payload = _json.loads(args[0])
            payload["entries"] = rows
            return _real_call(verb, [_json.dumps(payload)], timeout=15)

        monkeypatch.setattr("fno.rust_binary.call_binary_json", seeded_call)
        if not isinstance(entry, dict) or not entry.get("id"):
            # A row the graph cannot carry: read as unheld, as before.
            return None
        return real(entry, by_id)

    monkeypatch.setattr(ladder, "dispatch_hold_verdict", patched)
