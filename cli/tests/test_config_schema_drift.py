"""Anti-drift guards for the unified config schema.

Mirrors the repo's `fno doctor bundle check` pattern: the model is the single source
of truth, and these tests fail CI the moment a derived artifact drifts.

Five guards:
  1. registry completeness  - every model leaf has exactly one registry entry.
  2. docs freshness         - docs/configuration-guide.md matches the generator.
  3. wizard-key existence   - every wizard-surfaced path is a real model leaf.
  4. bash-default equality  - each `get_config "K" "D"` whose config.K is a
                              modeled leaf has D equal to the model default.
  5. rust allowlist parity  - the merge_strategy allowlist re-stated in Rust
                              matches AutoMergeBlock's.
"""
from __future__ import annotations

import inspect
import re
from pathlib import Path

import pytest

from fno.config import AutoMergeBlock
from fno.config import registry as _registry
from fno.config import schema_gen


def _repo_root() -> Path:
    """Walk up from this test file until docs/configuration-guide.md is found."""
    here = Path(__file__).resolve()
    for parent in here.parents:
        if (parent / "docs" / "configuration-guide.md").exists():
            return parent
    raise AssertionError("could not locate repo root (docs/configuration-guide.md)")


def test_registry_is_complete_and_exact() -> None:
    """Every model leaf has a registry entry and vice-versa (the drift-killer)."""
    leaves = set(schema_gen.all_leaf_paths())
    reg = set(_registry.FIELD_META)
    missing = leaves - reg
    extra = reg - leaves
    assert not missing, f"model leaves missing a registry entry: {sorted(missing)}"
    assert not extra, f"registry entries with no model leaf: {sorted(extra)}"


def test_registry_wizard_tiers_are_valid() -> None:
    for path, meta in _registry.FIELD_META.items():
        assert meta.wizard in ("always", "advanced", "never"), (
            f"{path}: invalid wizard tier {meta.wizard!r}"
        )


def test_markdown_generation_is_deterministic() -> None:
    assert schema_gen.render_markdown() == schema_gen.render_markdown()


def test_markdown_table_rows_never_split_on_doc_pipes() -> None:
    """A registry doc blurb carrying a literal ``|`` must render escaped.

    ``recovery.watchdog`` shipped a five-cell row split into eight columns:
    every pipe in a cell splits the rendered table exactly where the new
    feature is documented. Escaping in the generator (not hand-patching the
    generated file) is the guard, so pin it here.
    """
    rows = [
        line
        for line in schema_gen.render_markdown().splitlines()
        if line.startswith("| `")
    ]
    assert rows  # sanity: the reference table exists
    for line in rows:
        unescaped = re.sub(r"\\\|", "", line)
        assert unescaped.count("|") == 6, f"row split by unescaped pipe: {line[:80]}"


def test_committed_docs_are_fresh() -> None:
    """docs/configuration-guide.md must equal the generator's output.

    Regenerate with `fno config schema --markdown --write`.
    """
    docs = _repo_root() / "docs" / "configuration-guide.md"
    committed = docs.read_text(encoding="utf-8")
    assert committed == schema_gen.render_markdown(), (
        "docs/configuration-guide.md is stale; run "
        "`fno config schema --markdown --write`"
    )


def test_example_toml_is_deterministic_and_valid() -> None:
    """The example toml regenerates byte-identically and is a valid config."""
    import tomllib

    from fno.config import SettingsModel

    rendered = schema_gen.render_example_toml()
    assert rendered == schema_gen.render_example_toml()
    # Parses as TOML and round-trips through the flat model (defaults are valid;
    # optional keys are commented out so tomllib sees only the live ones).
    SettingsModel.model_validate(tomllib.loads(rendered))


def test_committed_example_toml_is_fresh() -> None:
    """docs/config.example.toml must equal the generator's output.

    Regenerate with `fno config schema --toml --write`.
    """
    example = _repo_root() / "docs" / "config.example.toml"
    committed = example.read_text(encoding="utf-8")
    assert committed == schema_gen.render_example_toml(), (
        "docs/config.example.toml is stale; run "
        "`fno config schema --toml --write`"
    )


def test_wizard_surfaced_paths_are_real_leaves() -> None:
    """Every always/advanced field maps to a real model leaf (kills the
    DEAD-key class: a wizard cannot ask about a key that doesn't exist)."""
    leaves = set(schema_gen.all_leaf_paths())
    for path, meta in _registry.FIELD_META.items():
        if meta.wizard != "never":
            assert path in leaves, f"wizard-surfaced {path!r} is not a model leaf"


def _model_defaults() -> dict[str, object]:
    """Map config.<dotted> -> model default for every leaf (bash reads config.K)."""
    return {leaf.path: leaf.default for leaf in schema_gen.iter_leaves()}


def _normalize(value: object) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return ""
    if isinstance(value, (list, dict)):
        return "" if not value else str(value)
    return str(value)


def test_bash_get_config_defaults_match_model() -> None:
    """For each `get_config "K" "D"` whose config.K is a modeled leaf, D must
    equal the model default (cheap dual-reader drift guard).

    Keys that are NOT modeled leaves (legacy / session-input / dead) are
    skipped - this guard only protects the keys that exist in both readers.
    """
    root = _repo_root()
    defaults = _model_defaults()
    pattern = re.compile(
        r'get_config(?:_or_default)?\s+"([a-zA-Z0-9_.]+)"\s+"([^"]*)"'
    )
    mismatches: list[str] = []
    checked = 0
    for sub in ("scripts", "skills", "hooks"):
        base = root / sub
        if not base.is_dir():
            continue
        for path in base.rglob("*.sh"):
            if "__pycache__" in str(path):
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except OSError:
                continue
            for key, bash_default in pattern.findall(text):
                # Model leaves are flat now; a bash key may carry the legacy
                # `config.` prefix or not - normalize either to the flat leaf.
                leaf = key[len("config.") :] if key.startswith("config.") else key
                if leaf not in defaults:
                    continue  # not a modeled leaf; out of scope for this guard
                # A key-PRESENCE probe deliberately passes an empty default so
                # unset reads as "" (the caller distinguishes set from unset).
                # It is exempt by its trailing marker, never by evasion: the
                # marker is the reviewable claim that this is a probe.
                probe_line = next(
                    (
                        ln
                        for ln in text.splitlines()
                        if f'get_config "{key}"' in ln and "presence-probe" in ln
                    ),
                    None,
                )
                if probe_line:
                    continue
                checked += 1
                if _normalize(defaults[leaf]) != bash_default:
                    mismatches.append(
                        f"{path.relative_to(root)}: get_config \"{key}\" "
                        f"default={bash_default!r} != model default "
                        f"{_normalize(defaults[leaf])!r}"
                    )
    assert checked > 0, "no modeled get_config defaults found to check (guard inert?)"
    assert not mismatches, "bash/model default drift:\n" + "\n".join(mismatches)


def test_rust_merge_strategy_allowlist_matches_model() -> None:
    """Rust is the third reader of `auto_merge.merge_strategy`, after Python and
    bash, and it cannot import the model.

    `fno-agents` arms auto-merge from `finalize`, so it re-states the allowlist
    as a `matches!` arm. Adding a fourth strategy to `AutoMergeBlock` without
    touching Rust would coerce it back to `merge` on the arming path only - the
    exact one-key-honored-on-two-of-three-paths bug this guard exists to stop.
    Source-to-source, same shape as the bash guard above.
    """
    rs = _repo_root() / "crates" / "fno-agents" / "src" / "agents_config.rs"
    m = re.search(r"matches!\(v\.as_str\(\),([^)]*)\)", rs.read_text(encoding="utf-8"))
    assert m, f"merge_strategy allowlist not found in {rs.name} (guard inert?)"
    rust = set(re.findall(r'"([a-z]+)"', m.group(1)))

    py = re.search(r"in \{([^}]*)\}", inspect.getsource(AutoMergeBlock._coerce_strategy))
    assert py, "AutoMergeBlock._coerce_strategy allowlist not found (guard inert?)"
    model = set(re.findall(r'"([a-z]+)"', py.group(1)))

    assert rust, "empty rust allowlist (regex matched the wrong thing?)"
    assert rust == model, (
        f"{rs.name} allows {sorted(rust)} but AutoMergeBlock allows {sorted(model)}"
    )


def test_rust_auto_merge_grant_reads_both_spellings() -> None:
    """`auto_merge_grant` is the Rust reader of the actor-scope key (x-4be1).

    It must read the canonical `auto_merge.grant` spelling AND keep the legacy
    `dispatch.auto_merge` arm for one release, mirroring the Python alias; a
    rename honored on one of N reachable paths is exactly the pitfalls-corpus
    trap this file exists to pin. Source-to-source, same shape as the
    strategy guard above; the behavior cases live in the Rust unit tests.
    """
    rs = _repo_root() / "crates" / "fno-agents" / "src" / "agents_config.rs"
    src = rs.read_text(encoding="utf-8")
    m = re.search(r"pub fn auto_merge_grant.*?\n\}", src, re.DOTALL)
    assert m, f"auto_merge_grant not found in {rs.name} (guard inert?)"
    body = m.group(0)
    assert 'get("auto_merge")' in body and 'get("grant")' in body, (
        "Rust reader lost the canonical auto_merge.grant spelling"
    )
    assert 'get("dispatch")' in body and "auto_merge" in body, (
        "Rust reader lost the legacy dispatch.auto_merge arm"
    )
    assert '"dispatch"' in body, "Rust reader does not compare against the grant literal"
