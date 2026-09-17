"""The Rust half of the verb ratchet reads the generated native inventory.

Earlier gates compared hand-typed lists and re-derived the tree from Rust
source text; both drifted. The reader reads ONE generated owner instead:
``scripts/ci/native-command-tree.txt`` is emitted from the typed clap tree by
``cargo run --example native_command_tree``, and a freshness test in
``cli_args.rs`` fails ``cargo test`` the moment the artifact lags the tree.

A reader that silently returned nothing on a missing file would read as "no
Rust verbs", so every failure below is a named refusal.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from fno.lint_verb_ratchet import (
    FNO_AGENTS_SOURCE,
    INVENTORY_REGENERATE,
    NATIVE_TREE_REL,
    VerbRatchetError,
    enumerate_rust_leaves,
    read_native_inventory,
    scan_fno_agents_source,
)


def _write_inventory(root: Path, body: str) -> Path:
    path = root / NATIVE_TREE_REL
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return path


MINIMAL_TREE = (
    "# generated header\n"
    "(root)\troot\tvisible\t-\t--server,!--session\n"
    "mux\tgroup\tvisible\t-\t-\n"
    "version\tleaf\tvisible\t-\t--json,-J\n"
    "mux pane ls\tleaf\tvisible\t-\t-\n"
)


def test_missing_inventory_names_the_file_and_regenerate(tmp_path: Path) -> None:
    with pytest.raises(VerbRatchetError) as err:
        read_native_inventory(tmp_path)
    assert NATIVE_TREE_REL in str(err.value)
    assert INVENTORY_REGENERATE in str(err.value)


def test_empty_inventory_refuses_rather_than_reading_as_empty(tmp_path: Path) -> None:
    _write_inventory(tmp_path, "# only a comment\n")
    with pytest.raises(VerbRatchetError) as err:
        read_native_inventory(tmp_path)
    assert "empty" in str(err.value)


def test_inventory_positive_control_mux_present(tmp_path: Path) -> None:
    _write_inventory(tmp_path, MINIMAL_TREE)
    leaves = enumerate_rust_leaves(tmp_path)
    assert "mux" in leaves
    assert "version" in leaves
    assert "fno-agents" in leaves


def test_added_root_row_becomes_a_leaf(tmp_path: Path) -> None:
    # Mutate the artifact, watch the answer move: proves the reader reads the
    # file rather than a constant.
    _write_inventory(tmp_path, MINIMAL_TREE + "fno-web\tleaf\tvisible\t-\t-\n")
    assert "fno-web" in enumerate_rust_leaves(tmp_path)


def test_inventory_missing_roots_refuse(tmp_path: Path) -> None:
    _write_inventory(tmp_path, "mux\tgroup\tvisible\t-\t-\n")
    with pytest.raises(VerbRatchetError) as err:
        enumerate_rust_leaves(tmp_path)
    assert "mux/version" in str(err.value)

def test_removed_mux_verbs_carry_a_tombstone() -> None:
    """A removed Rust verb names its replacement instead of a bare refusal."""
    real_root = Path(__file__).resolve().parents[3]
    main_rs = (real_root / "crates/fno/src/main.rs").read_text()
    assert "MUX_TOMBSTONES" in main_rs
    assert '"squad",' in main_rs, "the removed squad alias must keep its tombstone"


def test_added_fno_agents_dispatch_is_seen(tmp_path: Path) -> None:
    """The fno-agents half keeps its source scan: its action table stays
    frozen until the client retires, and it is still read independently."""
    import shutil

    real_root = Path(__file__).resolve().parents[3]
    dst = tmp_path / FNO_AGENTS_SOURCE
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(real_root / FNO_AGENTS_SOURCE, dst)
    before = scan_fno_agents_source(tmp_path)
    anchor = '    if verb == "ping" {'
    source = dst.read_text()
    assert anchor in source
    dst.write_text(
        source.replace(anchor, '    if verb == "zzthrowaway" {\n    }\n' + anchor)
    )
    after = scan_fno_agents_source(tmp_path)
    assert "zzthrowaway" in (after - before)
