"""MCP ``claude/channel`` wire-format helpers.

Single source of truth for the ``notifications/claude/channel`` envelope
shape this package emits to Claude Code. Spec reference:
``internal/claude/docs/code/channels-reference.md`` §Notification
format.

Two consumers:

- :mod:`fno.mcp.channel_server` — outbound to CC (it serializes
  the envelope to a JSON-RPC notification line on stdout).
- :mod:`fno.mcp.client` — outbound to the sidecar (it ships the
  envelope across the Unix socket so the per-session channel server can
  forward it to CC).

Sharing the envelope builder + validator avoids drift between the two
emission paths: if the wire format changes, exactly one module updates.

The pinned fixture lives at ``cli/tests/fixtures/mcp_channel_envelope.json``
and the Wave 1.0 smoke script validates that the runtime output matches
it byte-for-byte (modulo non-pinned fields like ``content`` body and
``meta`` key/value pairs).
"""
from __future__ import annotations

import re
from typing import Any, Dict, Iterable, Optional, Tuple

# Wire-format version pinned by Wave 1.0 smoke. Bump in lockstep with
# any deliberate envelope change AND the fixture under
# cli/tests/fixtures/mcp_channel_envelope.json.
ENVELOPE_VERSION = "1"

# JSON-RPC method per channels-reference §Notification format.
MCP_CHANNEL_METHOD = "notifications/claude/channel"

# CC silently drops meta keys that don't match this pattern. See
# channels-reference: "Keys must be identifiers: letters, digits, and
# underscores only. Keys containing hyphens or other characters are
# silently dropped." We refuse to emit them so the operator sees an
# error instead of a silently-stripped attribute.
META_KEY_RE = re.compile(r"^[A-Za-z0-9_]+$")


class MCPChannelEnvelopeError(ValueError):
    """Raised when ``content`` / ``meta`` violates the wire contract.

    Surfaced at build time (so the operator sees a loud failure) AND
    at validate time on receipt (so a drift-detected envelope demotes
    the channel rather than corrupting downstream state). See spec
    AC1-DRIFT.
    """


def _validate_meta(meta: Optional[Dict[str, str]]) -> Dict[str, str]:
    """Validate a meta map against the [A-Za-z0-9_]+ key rule.

    Returns the meta unchanged on success; raises
    :class:`MCPChannelEnvelopeError` with the offending keys listed on
    failure. ``None`` collapses to an empty dict.
    """
    if meta is None:
        return {}
    if not isinstance(meta, dict):
        raise MCPChannelEnvelopeError(
            f"meta must be a dict, got {type(meta).__name__}"
        )
    bad = [k for k in meta.keys() if not (isinstance(k, str) and META_KEY_RE.match(k))]
    if bad:
        raise MCPChannelEnvelopeError(
            "meta keys must match [A-Za-z0-9_]+ (CC silently drops "
            f"non-identifier keys); offenders: {sorted(bad)!r}"
        )
    bad_vals = [k for k, v in meta.items() if not isinstance(v, str)]
    if bad_vals:
        raise MCPChannelEnvelopeError(
            "meta values must be strings; non-string keys: "
            f"{sorted(bad_vals)!r}"
        )
    return dict(meta)


def build_channel_notification(
    *,
    content: str,
    meta: Optional[Dict[str, str]] = None,
    request_id: Optional[str] = None,
) -> Dict[str, Any]:
    """Build a ``notifications/claude/channel`` JSON-RPC envelope.

    Args:
        content: Body of the ``<channel>`` tag that arrives in Claude's
            context. UTF-8 string.
        meta: Optional attribute map. Keys must match
            ``[A-Za-z0-9_]+``; CC drops non-identifier keys silently.
            Values must be strings.
        request_id: Optional correlation id (e.g. 32 lowercase hex chars
            per AC4-INVARIANT). When non-None, lands on
            ``meta["request_id"]`` so the recipient sees it as a
            ``<channel>`` tag attribute, joining started/done event
            pairs across the MCP wire. Receive-side validation tolerates
            absent request_id (Locked Decision #12); only documented
            senders include it.

    Returns:
        A JSON-RPC 2.0 notification dict ready for ``json.dumps``.

    Raises:
        MCPChannelEnvelopeError: If ``content`` is not a string,
            ``meta`` violates the key/value rules, or ``request_id`` is
            not a non-empty string when provided.
    """
    if not isinstance(content, str):
        raise MCPChannelEnvelopeError(
            f"content must be a string, got {type(content).__name__}"
        )

    merged_meta: Dict[str, str] = dict(meta) if meta is not None else {}
    if request_id is not None:
        if not isinstance(request_id, str) or not request_id:
            raise MCPChannelEnvelopeError(
                f"request_id must be a non-empty string, got "
                f"{type(request_id).__name__}={request_id!r}"
            )
        merged_meta["request_id"] = request_id

    return {
        "jsonrpc": "2.0",
        "method": MCP_CHANNEL_METHOD,
        "params": {
            "content": content,
            "meta": _validate_meta(merged_meta),
        },
    }


def validate_envelope(envelope: Any) -> Tuple[bool, Optional[str]]:
    """Validate an inbound (or self-emitted) envelope against the wire
    contract.

    Used at receipt time (sidecar -> channel_server path) so that a
    Claude Code upgrade that renames or restructures the envelope is
    detected and the affected channel demotes to socket fallback
    (per spec AC1-DRIFT). Returns ``(ok, reason)``:

    - ``(True, None)`` — envelope is acceptable.
    - ``(False, "<reason>")`` — envelope failed validation. ``reason``
      is short and machine-stable for the ``mcp_channel_envelope_drift``
      event payload.
    """
    if not isinstance(envelope, dict):
        return False, f"envelope_not_dict:{type(envelope).__name__}"
    if envelope.get("jsonrpc") != "2.0":
        return False, "jsonrpc_version_missing_or_not_2.0"
    if envelope.get("method") != MCP_CHANNEL_METHOD:
        return False, f"method_mismatch:{envelope.get('method')!r}"
    params = envelope.get("params")
    if not isinstance(params, dict):
        return False, "params_not_dict"
    if "content" not in params:
        return False, "content_missing"
    if not isinstance(params["content"], str):
        return False, "content_not_string"
    meta = params.get("meta", {})
    if not isinstance(meta, dict):
        return False, "meta_not_dict"
    for k, v in meta.items():
        if not (isinstance(k, str) and META_KEY_RE.match(k)):
            return False, f"meta_key_invalid:{k!r}"
        if not isinstance(v, str):
            return False, f"meta_value_not_string:{k!r}"
    return True, None


def envelope_drift_diff(
    expected: Dict[str, Any],
    received: Dict[str, Any],
) -> Dict[str, Any]:
    """Compare two envelopes and return a diff suitable for the
    ``mcp_channel_envelope_drift`` event (per AC1-DRIFT).

    The diff carries:
      - ``missing_top_keys``: keys present in ``expected`` but absent
        from ``received``.
      - ``extra_top_keys``: keys present in ``received`` but absent
        from ``expected``.
      - ``params_missing``, ``params_extra``: same for the ``params``
        sub-object.
      - ``method_expected`` / ``method_received``: when the method
        string differs.

    We do NOT compare ``content`` or ``meta`` values; those are
    user-controlled. Only structural keys are pinned.
    """
    def _keys(d: Any) -> Iterable[str]:
        if isinstance(d, dict):
            return d.keys()
        return ()

    diff: Dict[str, Any] = {}

    exp_top = set(_keys(expected))
    rec_top = set(_keys(received))
    if exp_top - rec_top:
        diff["missing_top_keys"] = sorted(exp_top - rec_top)
    if rec_top - exp_top:
        diff["extra_top_keys"] = sorted(rec_top - exp_top)

    if expected.get("method") != received.get("method"):
        diff["method_expected"] = expected.get("method")
        diff["method_received"] = received.get("method")

    exp_params = expected.get("params") or {}
    rec_params = received.get("params") or {}
    exp_p_keys = set(_keys(exp_params))
    rec_p_keys = set(_keys(rec_params))
    if exp_p_keys - rec_p_keys:
        diff["params_missing"] = sorted(exp_p_keys - rec_p_keys)
    if rec_p_keys - exp_p_keys:
        diff["params_extra"] = sorted(rec_p_keys - exp_p_keys)

    return diff
