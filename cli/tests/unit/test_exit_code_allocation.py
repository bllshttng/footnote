"""The exit-code allocation gate: one number, one meaning.

Every EXIT_* constant with a value >= 64 across the Python and Rust trees
claims its number once. The same NAME at the same number in both trees is
byte-parity (the shared capacity pair, the fleet pair); two different names
at one number is a collision and is the defect this gate exists to catch:
78 once meant both the Python slot-queue refusal and the Rust state-root
refusal, and advance retried the permanent one forever. The convention band
(small ints 0-5 and 13-25) is exempt by design. The table lives in
fno/agents/spawn_gate.py, mirrored in crates/fno-agents/src/spawn_gate.rs.
"""
import re
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[3]
_EXIT = re.compile(r"EXIT_([A-Z0-9_]+)\s*(?::[^=\n]*)?=\s*(\d+)")
_GATES = ("cli/src/fno/agents/spawn_gate.py", "crates/fno-agents/src/spawn_gate.rs")


def _claims() -> dict[int, dict[str, list[str]]]:
    claims: dict[int, dict[str, list[str]]] = {}
    for pattern in ("cli/src/fno/**/*.py", "crates/*/src/**/*.rs"):
        for path in sorted(_ROOT.glob(pattern)):
            for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                m = _EXIT.search(line)
                if m and int(m.group(2)) >= 64:
                    name, value = m.group(1), int(m.group(2))
                    claims.setdefault(value, {}).setdefault(name, []).append(
                        f"{path.relative_to(_ROOT)}:{n}"
                    )
    return claims


def test_gate_band_exit_codes_are_unique_across_both_trees():
    claims = _claims()
    # Positive control: the scan must see the allocation tables themselves,
    # else a moved tree makes this test pass vacuously.
    for gate in _GATES:
        assert any(gate in loc for sites in claims.values() for locs in sites.values() for loc in locs), (
            f"scan missed {gate}"
        )
    dupes = {v: sites for v, sites in claims.items() if len(sites) > 1}
    assert not dupes, "one exit code, one meaning (same name in both trees is parity): " + "; ".join(
        f"{v} -> {sites}" for v, sites in sorted(dupes.items())
    )
