"""The retraction primitive for review attestations.

Today the only way to revoke an attestation is `emit-attestation.sh <r> fail`,
which stamps the attester from the same env markers as the pass it revokes -
undoing an impersonation would require performing it a second time. With the
attester bound to the emitting process, that shape is impossible, so a forged
attestation would be permanently irretractable without this verb.

A retraction is a `review_attestation` carrying `verdict: fail` and the
revoked pair's attester in `retracts_attester`. It records the RETRACTING
process's own identity in `attester_session_id`, so the audit trail says who
revoked, and the coverage gate's pair-keyed scan reads it as revoking the
`(reviewer, retracts_attester)` pair at the named head.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional


def _project_events_path(events: Optional[Path]) -> Path:
    if events is not None:
        return events
    from fno.paths import resolve_repo_root

    return resolve_repo_root() / ".fno" / "events.jsonl"


def _strip_slashes(name: str) -> str:
    return name.lstrip("/")


def find_pass(
    events_path: Path, reviewer: str, attester: str, head: str
) -> Optional[dict]:
    """The newest passing attestation matching the (reviewer, attester, head)
    triple, or None. None means there is nothing to retract, which the verb
    reports rather than writing a revocation for a pair that never passed."""
    key = _strip_slashes(reviewer)
    match: Optional[dict] = None
    try:
        text = events_path.read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        try:
            val = json.loads(line)
        except json.JSONDecodeError:
            continue
        if val.get("type") != "review_attestation":
            continue
        data = val.get("data") or {}
        if not isinstance(data, dict):
            continue
        if _strip_slashes(str(data.get("reviewer", ""))) != key:
            continue
        if data.get("attester_session_id") != attester:
            continue
        if data.get("head_sha") != head:
            continue
        if data.get("verdict") == "pass":
            match = data
    return match


def _current_branch() -> str:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    if out.returncode != 0:
        return ""
    return out.stdout.strip()


def retract(
    reviewer: str,
    attester: str,
    head: str,
    reason: str,
    events: Optional[Path] = None,
) -> int:
    """Emit the retraction, or refuse. Returns the process exit code."""
    import typer

    events_path = _project_events_path(events)
    if find_pass(events_path, reviewer, attester, head) is None:
        typer.echo(
            f"no passing attestation for reviewer '{reviewer}' attester "
            f"'{attester}' at head {head[:12]} in {events_path}; nothing retracted",
            err=True,
        )
        return 1

    from fno.events import _build, append_event
    from fno.harness_identity import AttesterIdentityConflict, resolve_attester_identity

    try:
        resolved_id, witness = resolve_attester_identity()
    except AttesterIdentityConflict as exc:
        typer.echo(
            f"error: {exc}. No retraction emitted under an identity the caller overrode.",
            err=True,
        )
        return 1

    # session_id/harness from the worktree manifest when one is bound, the
    # same grep the attestation producer performs; a bare-shell retraction
    # leaves them empty and the journal stays honest about it.
    session_id = ""
    harness = ""
    try:
        from fno.paths import resolve_repo_root

        manifest = resolve_repo_root() / ".fno" / "target-state.md"
        if manifest.is_file():
            for line in manifest.read_text(encoding="utf-8").splitlines():
                stripped = line.strip()
                for key in ("session_id", "harness"):
                    prefix = f"{key}:"
                    if stripped.startswith(prefix):
                        value = stripped[len(prefix) :].strip()
                        if value and value != "null":
                            if key == "session_id":
                                session_id = value
                            else:
                                harness = value
    except OSError:
        pass

    data = {
        "reviewer": _strip_slashes(reviewer),
        "head_sha": head,
        "verdict": "fail",
        "session_id": session_id,
        "retracts_attester": attester,
        "attester_session_id": resolved_id,
        "attester_witness": witness,
        "retraction_reason": reason,
        "branch": _current_branch(),
    }
    if harness:
        data["harness"] = harness

    from fno.events import ValidationError

    try:
        event = _build("review_attestation", "target" if session_id else "test", data)
    except ValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        return 1
    try:
        append_event(event, events_path=events_path)
    except Exception as exc:  # noqa: BLE001 - surface any append failure verbatim
        typer.echo(f"error: failed to append retraction: {exc}", err=True)
        return 1
    typer.echo(
        f"retracted: reviewer={data['reviewer']} attester={attester} head={head[:12]} "
        f"by={resolved_id or 'unattributed'} reason={reason}"
    )
    return 0
