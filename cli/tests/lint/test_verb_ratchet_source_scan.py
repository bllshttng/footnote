"""The Rust half of the verb ratchet reads real source, behaviourally proven.

The gate this replaced compared a hand-typed Python constant against a
hand-typed Rust usage string. Both were written by the same person in the same
sitting and omitted the same five verbs, so it passed green over an unbaselined
surface for a year.

A test that asserts "the scan finds `mux pane ls`" would pass over a hardcoded
list just as happily, so it proves nothing and the plan refuses it in advance.
These tests MUTATE the Rust source instead and assert the scan's answer moves
with it: add an arm, the scan gains a verb; delete an arm, the scan loses it.
Only a scan that actually reads the file can pass both.
"""

from __future__ import annotations

import shutil
from pathlib import Path

import pytest

from fno.lint_verb_ratchet import RUST_SOURCES, VerbRatchetError, scan_rust_source


@pytest.fixture
def rust_tree(tmp_path: Path) -> Path:
    """A throwaway copy of the two Rust dispatcher files, editable in place."""
    real_root = Path(__file__).resolve().parents[3]
    for rel in RUST_SOURCES:
        dst = tmp_path / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(real_root / rel, dst)
    return tmp_path


def _mux(tmp: Path) -> Path:
    return tmp / RUST_SOURCES[1]


def test_scan_reflects_the_real_tree(rust_tree: Path) -> None:
    """Baseline for the mutations below: the families are read, not invented."""
    _tops, families = scan_rust_source(rust_tree)
    assert set(families) == {"pane", "tab", "layout", "block", "workspace"}


def test_added_match_arm_is_seen(rust_tree: Path) -> None:
    """Add a throwaway arm to the pane dispatcher; the scan must gain it.

    This is the acceptance criterion in its behavioural form. A hardcoded list
    cannot pass it: the verb exists in no constant anywhere.
    """
    path = _mux(rust_tree)
    src = path.read_text()
    anchor = '        "release" => PaneCmd::Release {'
    assert anchor in src, "the pane dispatcher moved; re-anchor this test"
    path.write_text(src.replace(anchor, '        "zzthrowaway" => PaneCmd::Ls {},\n' + anchor))

    _tops, families = scan_rust_source(rust_tree)
    assert "zzthrowaway" in families["pane"]


def test_removed_match_arm_is_lost(rust_tree: Path) -> None:
    """Delete a real arm; the scan must stop reporting it.

    The other direction matters just as much. A scan that only ever ADDS is
    satisfied by a union with a stale constant, and a dropped verb then keeps a
    baseline line forever with nothing behind it.
    """
    path = _mux(rust_tree)
    src = path.read_text()
    anchor = '        "kill" => PaneCmd::Kill {'
    assert anchor in src, "the pane dispatcher moved; re-anchor this test"
    path.write_text(src.replace(anchor, '        "kkill" => PaneCmd::Kill {'))

    _tops, families = scan_rust_source(rust_tree)
    assert "kill" not in families["pane"]
    assert "kkill" in families["pane"]


def test_equality_guard_dispatch_is_seen(rust_tree: Path) -> None:
    """`pane run` and `layout apply` dispatch on an `==` guard, not a match arm.

    They were among the five verbs the old gate missed, and a scan that reads
    only match arms misses them for the same reason.
    """
    _tops, families = scan_rust_source(rust_tree)
    assert "run" in families["pane"]
    assert {"apply", "graft"} <= families["layout"]


def test_flag_spellings_are_not_proposed_as_verbs(rust_tree: Path) -> None:
    """The `pane run` flag parser spells `workspace`, `squad`, and `split`.

    `split` IS a pane verb; the other two are flag aliases nested inside another
    parser. A function-wide scan proposed all three, which would have written
    two verbs that do not exist into the baseline.
    """
    _tops, families = scan_rust_source(rust_tree)
    assert "split" in families["pane"]
    assert "workspace" not in families["pane"]
    assert "squad" not in families["pane"]
    assert "current" not in families["pane"]


def test_missing_dispatch_refuses_rather_than_returning_empty(rust_tree: Path) -> None:
    """A scan that finds nothing must fail closed.

    An empty result reads as "the Rust front has no verbs", which passes against
    a baseline that omits every one of them - the same silent-green shape the
    old gate had.
    """
    (rust_tree / RUST_SOURCES[0]).write_text("fn main() {}\n")
    with pytest.raises(VerbRatchetError, match="could not find the `mux` dispatch"):
        scan_rust_source(rust_tree)


def test_catch_all_arm_shape_does_not_change_the_answer(rust_tree: Path) -> None:
    """Both spellings of the catch-all arm must scan to the same verbs.

    Rust lets a catch-all be a braced block or a single expression, and rustfmt
    keeps whichever it is handed - so the shape tracks whether the refusal
    message happens to fit on one line, not anything about the dispatch. Hoisting
    a verb list into a const is enough to flip it.

    The scan used to walk up a FIXED two levels from the refusal (arm, then
    match), which only holds for the braced form. On the brace-less one it
    walked past the match into the enclosing fn, found no arms at that level and
    reported zero verbs for the family. That fails closed, but it names the
    verbs it cannot see rather than the shape, so the message points away from
    the cause. Asserting equality across the two shapes is what pins it: a scan
    that can only read one of them cannot pass this.
    """
    path = _mux(rust_tree)
    src = path.read_text()
    flat = '        other => return Err(format!("unknown pane verb: {other} ({PANE_VERBS})")),'
    assert flat in src, "the pane catch-all moved; re-anchor this test"
    _tops, before = scan_rust_source(rust_tree)

    braced = (
        '        other => {\n'
        '            return Err(format!("unknown pane verb: {other} ({PANE_VERBS})"));\n'
        '        }'
    )
    path.write_text(src.replace(flat, braced))
    _tops, after = scan_rust_source(rust_tree)

    assert after["pane"] == before["pane"]
    # Guard the guard: a scan that returned {} for both shapes would satisfy the
    # equality above while seeing nothing at all.
    assert "focus" in after["pane"] and len(after["pane"]) > 5


def test_removed_mux_verbs_carry_a_tombstone() -> None:
    """A removed Rust verb names its replacement instead of a bare banner."""
    real_root = Path(__file__).resolve().parents[3]
    main_rs = (real_root / RUST_SOURCES[0]).read_text()
    assert "MUX_TOMBSTONES" in main_rs
    assert '"squad",' in main_rs, "the removed squad alias must keep its tombstone"
