"""The capability probe: measure the table against the live harness (x-244c).

One verdict per declared field, four values, and UNKNOWN never acts:

``AGREES``      the row and reality tell the same story.
``DISAGREES``   reality contradicts the row, with the evidence line quoted.
``UNPROBEABLE`` the field declares no one-shot instrument; the reason is the
                measurement record (a probe that emitted a value here would
                overwrite a measurement with a guess).
``UNKNOWN``     the instrument could not run - binary absent, timeout, no
                store instrument. An absent instrument is not a measurement,
                so UNKNOWN is reported, never downgraded to a disagreement.

Read-only by default: ``declared`` fields run their authority command (a
help read), ``behavioral`` fields spawn nothing unless ``live`` is set, and
nothing is ever written unless ``write`` is set. A ``behavioral`` field
accepts a candidate form ONLY on a marker the vendor's own store produced -
a form that looks right and opens a FRESH session is the failure this table
exists to prevent (AC5).
"""

from __future__ import annotations

import re
import shlex
import shutil
import subprocess
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Callable, Optional

from fno.agents.harness_map import (
    OVERRIDE_WARNINGS,
    MAP_VERSION,
    capabilities_or_undeclared,
    probe_declarations,
)

VERDICTS = ("AGREES", "DISAGREES", "UNPROBEABLE", "UNKNOWN", "UNDECLARED")
_AUTHORITY_TIMEOUT_S = 15
#: Per-field checks for ``declared`` fields: the pattern matching the
#: authority's output means the harness HAS the surface, so the row property
#: named here must not contradict it. The live specimen is agy: its ``--help``
#: declares ``--effort (low|medium|high)`` while the bundled row says
#: ``kind = "unsupported"``.
_DECLARED_CHECKS: dict[str, Callable[[dict, "re.Match[str]"], tuple[bool, str]]] = {}


@dataclass(frozen=True)
class FieldReport:
    field: str
    kind: str
    verdict: str
    detail: str
    #: The evidence line the authority produced, quoted on a disagreement.
    evidence: str = ""

    def __post_init__(self) -> None:
        if self.verdict not in VERDICTS:
            raise ValueError(f"unknown verdict {self.verdict!r}")


def _run_authority(command: list[str], cwd: Optional[Path] = None) -> tuple[int, str]:
    """Run one authority command directly so its return code stays readable."""
    try:
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=_AUTHORITY_TIMEOUT_S,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired:
        return 124, f"authority timed out after {_AUTHORITY_TIMEOUT_S}s: {command[0]}"
    except OSError as exc:
        return 127, f"could not run {command[0]}: {exc}"
    return result.returncode, (result.stdout + result.stderr)


def _resolve_path(row: dict, dotted: str):
    node = row
    for key in dotted.split("."):
        if not isinstance(node, dict) or key not in node:
            return None
        node = node[key]
    return node


def _declared_field(field: str, harness: str, row: dict, decl: dict) -> FieldReport:
    """Read the vendor's own statement and compare it with the row."""
    if field not in _DECLARED_CHECKS:
        return FieldReport(
            field, "declared", "UNDECLARED",
            "no comparison rule for this declared field; the probe refuses to guess one",
        )
    check = _DECLARED_CHECKS[field]
    argv = [token.replace("{bin}", harness) for token in shlex.split(decl["authority"])]
    if shutil.which(argv[0]) is None:
        return FieldReport(
            field, "declared", "UNKNOWN",
            f"{argv[0]} binary is not on PATH: an absent instrument is not a measurement",
        )
    code, output = _run_authority(argv)
    if code != 0:
        return FieldReport(
            field, "declared", "UNKNOWN",
            f"authority exited {code}: {output.strip()[:200]}",
        )
    match = re.search(decl["pattern"], output)
    agrees, detail = check(row, match)
    evidence = match.group(0) if match else ""
    verdict = "AGREES" if agrees else "DISAGREES"
    suffix = f" (evidence: {evidence})" if evidence else ""
    return FieldReport(field, "declared", verdict, detail + suffix, evidence)


def _model_switch_check(row: dict, match: Optional[re.Match[str]]) -> tuple[bool, str]:
    kind = row.get("model_switch_strategy", {}).get("kind", "unsupported")
    if match:
        vocab = match.groupdict().get("vocab", "")
        if kind == "unsupported":
            return False, (
                "the binary declares a reasoning-effort surface "
                f"({vocab}) but the row says kind = unsupported"
            )
        return True, f"the row ({kind}) and the declared surface ({vocab}) agree"
    if kind != "unsupported":
        return False, (
            f"the row declares {kind} but the authority declares no "
            "reasoning-effort surface"
        )
    return True, "no declared effort surface and the row says unsupported"


#: Registered after the check fns exist; matching the declared field's path
#: in the row against the reality the authority declared.
_DECLARED_CHECKS["model_switch_strategy"] = _model_switch_check


def _behavioral_field(field: str, harness: str, decl: dict, *, live: bool) -> FieldReport:
    """Behavioral probes spawn a scratch session; nothing runs unless live."""
    if not live:
        return FieldReport(
            field, "behavioral", "UNKNOWN",
            "behavioral probe needs --live (a scratch session is spawned); "
            "read-only run spawns nothing",
        )
    runner = _store_instrument(harness)
    if runner is None:
        return FieldReport(
            field, "behavioral", "UNKNOWN",
            "no vendor-store instrument implemented for this harness",
        )
    marker = runner(harness, field)
    if marker:
        return FieldReport(
            field, "behavioral", "AGREES",
            f"accepted on the vendor-produced marker: {marker}",
        )
    return FieldReport(
        field, "behavioral", "UNKNOWN",
        "the scratch run produced no vendor-store marker; the form is not accepted",
    )


def _store_instrument(harness: str) -> Optional[Callable[[str, str], str]]:
    """The vendor-store marker readers the probe knows, one per harness.
    The codex 2026-08-28 template is the shape: a session absent from the
    vendor's own store before the run and present after. NO harness ships a
    wired instrument yet - a live store diff spawns real sessions against a
    real login, so wiring one is its own measured change per harness, never
    a table edit. Harnesses with no wired instrument answer UNKNOWN, never
    a guess; tests inject fakes through this seam."""
    return None


def probe_harness(
    harness: str, *, live: bool = False, write: bool = False
) -> dict:
    """Resolve ``harness`` through the merged row and report one verdict per
    declared field. Read-only unless ``write``."""
    warnings = list(OVERRIDE_WARNINGS)
    try:
        row = capabilities_or_undeclared(harness)
    except Exception as exc:  # noqa: BLE001 - an unreadable row is UNKNOWN
        return {
            "harness": harness,
            "map_version": MAP_VERSION,
            "error": str(exc),
            "fields": [],
            "stanza": None,
            "warnings": warnings,
        }
    fields: list[FieldReport] = []
    for field, decl in sorted(probe_declarations().items()):
        kind = decl["kind"]
        if kind == "unprobeable":
            fields.append(
                FieldReport(field, kind, "UNPROBEABLE", decl["reason"])
            )
        elif kind == "declared":
            fields.append(_declared_field(field, harness, row, decl))
        elif kind == "behavioral":
            fields.append(_behavioral_field(field, harness, decl, live=live))
    disagreements = [f for f in fields if f.verdict == "DISAGREES"]
    stanza = _write_stanza(harness, disagreements) if write else None
    return {
        "harness": harness,
        "map_version": MAP_VERSION,
        "fields": [
            {
                "field": f.field,
                "kind": f.kind,
                "verdict": f.verdict,
                "detail": f.detail,
                **({"evidence": f.evidence} if f.evidence else {}),
            }
            for f in fields
        ],
        "stanza": stanza,
        "warnings": warnings,
    }


def _write_stanza(harness: str, disagreements: list[FieldReport]) -> Optional[str]:
    """The config stanza for each disagreement, evidence line and measurement
    date beside it. The stanza is emitted COMMENTED: the probe writes only
    what its instruments derived, never a guess shaped like a measurement."""
    if not disagreements:
        return None
    today = date.today().isoformat()
    lines: list[str] = [
        f"# capability probe: {len(disagreements)} disagreement(s) on {harness}, measured {today}",
    ]
    for field in disagreements:
        lines.append(f"# DISAGREES: {field.detail}")
        if field.field == "model_switch_strategy":
            lines.extend(
                [
                    "# The derived half is real (the --help surface above); the",
                    "# status pair is the retask surface and stays UNMEASURED -",
                    "# complete it from a live pane before relying on retask.",
                    f"# [harness.{harness}.model_switch_strategy]",
                    '# kind = "direct"',
                    '# tokens = ["--model {model}", "--effort {effort}"]',
                    "# effort_labels = {}",
                    '# status_command = "/status"   # UNMEASURED',
                    '# status_pattern = ""          # UNMEASURED',
                ]
            )
        else:
            lines.append(f"# no stanza template for {field.field}; correct it by hand")
    return "\n".join(lines)
