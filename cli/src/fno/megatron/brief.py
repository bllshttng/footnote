"""Megatron wave brief assembly.

Brief assembly turns the discoveries from a completed wave into a
context block that gets prepended to the next wave's heads-up bodies.
The order is byte-stable across commander restarts (sorted by msg_id)
so the same set of completes always produces the same brief; that's
the determinism property AC4-EDGE depends on.
"""
from __future__ import annotations

import re
from typing import Optional


_DISCOVERIES_RE = re.compile(
    r"^###\s+Discoveries\s*\n(?P<body>.*?)(?=^###\s|\Z)",
    re.DOTALL | re.MULTILINE,
)
_BODY_OVERSIZE_BYTES = 10 * 1024


def _truncate_to_bytes(text: str, limit: int) -> tuple[str, bool]:
    encoded = text.encode("utf-8")
    if len(encoded) <= limit:
        return text, False
    truncated = encoded[:limit]
    # Trim back to a clean line boundary
    last_newline = truncated.rfind(b"\n")
    if last_newline > 0:
        truncated = truncated[: last_newline + 1]
    return truncated.decode("utf-8", errors="ignore"), True


def _extract_discoveries(body: str) -> Optional[str]:
    match = _DISCOVERIES_RE.search(body)
    if match is None:
        return None
    section = match.group("body").strip("\n")
    section, was_truncated = _truncate_to_bytes(section, _BODY_OVERSIZE_BYTES)
    if was_truncated:
        section = section.rstrip("\n") + "\n[truncated at 10KB by commander]"
    return section


def assemble_wave_brief(
    *,
    completes_for_wave: list[dict],
    wave: int,
    now_iso: Optional[str] = None,
) -> str:
    """Concatenate Discoveries sections from each complete in stable msg_id order.

    Order is purely lexicographic on msg_id so the brief is identical
    on a commander restart that re-reads the same state file.
    Missing-discoveries fall back to ``(no discoveries reported)`` per
    AC2-ERR.

    The header omits a synthesis timestamp by default to preserve
    byte-stability across restarts (Gemini MEDIUM finding on PR #216).
    Callers that want a timestamped header can pass ``now_iso``
    explicitly with a stable value (e.g. mission ``created_at``).
    """
    sorted_completes = sorted(completes_for_wave, key=lambda c: c.get("msg_id", ""))

    if now_iso is None:
        sections: list[str] = [f"# Wave {wave} brief\n"]
    else:
        sections = [f"# Wave {wave} brief (synthesized {now_iso})\n"]
    for c in sorted_completes:
        from_proj = c.get("from") or c.get("project", "?")
        # str() before slice: a corrupted record with a non-string commit_sha
        # would raise TypeError on `[:12]`; the defensive coercion mirrors
        # artifact._build_waves and keeps the fallback chain crash-free.
        msg_id = c.get("msg_id") or str(c.get("commit_sha") or "")[:12] or c.get("project", "?")
        # `discoveries` is the canonical source (megatron-discoveries-field
        # spec, ab-bc919f7f). Producer (hooks/target-stop-hook.sh) writes
        # the EXTRACTED SECTION BODY (no `### Discoveries` header) on
        # post-spec completion JSONs - even when the body is empty - so
        # the consumer must (a) take the field as-is rather than running
        # the heading-anchored extractor on it, and (b) distinguish
        # "field present, intentionally empty" from "field absent (legacy
        # record)". A falsy `or` chain would erase the second
        # distinction; running _extract_discoveries on the field would
        # erase the first by demanding a header that the producer never
        # writes.
        if "discoveries" in c:
            raw = c["discoveries"] or ""
            disc = raw.strip("\n")
            disc, was_truncated = _truncate_to_bytes(disc, _BODY_OVERSIZE_BYTES)
            if was_truncated:
                disc = disc.rstrip("\n") + "\n[truncated at 10KB by commander]"
        else:
            raw = c.get("body", "") or ""
            disc = _extract_discoveries(raw)
        if disc is None or not disc.strip():
            sections.append(f"## From {from_proj} ({msg_id}):\n- (no discoveries reported)\n")
        else:
            sections.append(f"## From {from_proj} ({msg_id}):\n{disc}\n")
    return "\n".join(sections)


def inject_brief_into_bodies(
    bodies: list[str],
    brief: Optional[str],
) -> list[str]:
    """Prepend a wave brief to each manifest body for the next wave's dispatch.

    A None or empty brief returns the bodies unchanged so callers can
    pass through to the dispatcher without conditional logic.
    """
    if not brief:
        return list(bodies)
    return [
        f"# Prior wave context:\n{brief}\n\n---\n\n{body}"
        for body in bodies
    ]
