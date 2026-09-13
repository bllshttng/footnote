"""Guard: every harness that drives init-target-state.sh and reads a legacy
state literal must install the state-path stub (or carry a seeded reason).

init resolves its manifest, events and cancel sentinel through
`fno-agents state path`; a test that reads `<root>/.fno/target-state.md`
proves the fallback leg only, and goes dark on any machine with the binary
installed. New legacy readers must either install the stub from
tests/helpers/fno-agents-state-path-stub.sh (Python callers: the installer in
cli/tests/_init_space.py) or land in ALLOWED with a reason naming why they
never read init's write location.
"""

from __future__ import annotations

import os
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]

LEGACY_LITERALS = (
    ".fno/target-state.md",
    '.fno" / "target-state.md',
    ".fno/.pending-plan.md",
    ".fno/.target-cancelled",
)
STUB_MARKERS = ("fno-agents-state-path-stub", "install_state_path_stub")
SCRIPT_NAME = "init-target-state.sh"

# Seeded when the guard first ran over the migrated tree. Keys are
# repo-relative paths; each reason names why the file never reads init's
# write location.
ALLOWED = {
    "cli/tests/unit/test_backlog_capture.py":
        "uses the script name as a capture --where payload; never runs init",
    "cli/tests/unit/test_target_cli.py":
        "writes its own stub init script into a fake plugin root; never reads state",
    "cli/tests/unit/test_target_init_review_gate.py":
        "absence is asserted through fno.paths.target_state_path, which the "
        "conftest-pinned FNO_SPACES_DIR resolves to the space path",
    "tests/hooks/test_init_hold_shapes.sh":
        "pins the fno-absent fallback leg (PATH=/usr/bin:/bin by design) and "
        "checks absence at the space path alongside the legacy pin",
    "tests/hooks/test_loop_check_e2e.sh":
        "drives loop-check with an explicit --state and a pinned FNO_SPACES_DIR",
    "tests/target-preflight/test-init-location-gate.sh":
        "unrepaired legacy harness, out of scope per the x-f105 plan surface; "
        "nothing runs tests/target-preflight in CI",
    "tests/test-handoff.sh":
        "names the script only in a comment and never runs init",
    "tests/test-register-task.sh":
        "names the script only in a comment and never runs init",
}


def _flags_legacy_reader(rel: str, text: str) -> bool:
    """True when a file pairs the init script with a legacy literal and
    carries no stub marker. ALLOWED entries are judged by the caller."""
    if SCRIPT_NAME not in text:
        return False
    if not any(lit in text for lit in LEGACY_LITERALS):
        return False
    return not any(marker in text for marker in STUB_MARKERS)


def _scanned_files() -> list[Path]:
    hits: list[Path] = []
    for base in ("tests", os.path.join("cli", "tests")):
        root = REPO_ROOT / base
        if not root.is_dir():
            continue
        for dirpath, dirs, files in os.walk(root):
            dirs[:] = [d for d in dirs if not d.startswith(".")]
            for fn in files:
                if fn.endswith(".sh") or (fn.startswith("test_") and fn.endswith(".py")):
                    hits.append(Path(dirpath) / fn)
    return hits


def test_no_new_legacy_state_readers() -> None:
    flagged = []
    for p in _scanned_files():
        rel = p.relative_to(REPO_ROOT).as_posix()
        if rel in ALLOWED:
            continue
        if _flags_legacy_reader(rel, p.read_text(encoding="utf-8", errors="replace")):
            flagged.append(rel)
    assert not flagged, (
        "files drive init-target-state.sh and read a legacy state literal "
        "without the state-path stub (install tests/helpers/"
        "fno-agents-state-path-stub.sh, or seed a reason in ALLOWED): "
        + ", ".join(flagged)
    )


def test_positive_control_file_is_in_scanned_set() -> None:
    files = _scanned_files()
    control = REPO_ROOT / "cli/tests/integration/test_target_node_claim.py"
    assert control in files, "the walk must reach the migrated claim tests"


def test_predicate_flags_a_synthetic_legacy_reader() -> None:
    text = f"bash {SCRIPT_NAME}\nSTATE='$root/.fno/target-state.md'\n"
    assert _flags_legacy_reader("synthetic", text)
    assert not _flags_legacy_reader(
        "synthetic", text.replace(SCRIPT_NAME, "other-script.sh")
    )
