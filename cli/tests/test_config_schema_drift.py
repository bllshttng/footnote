"""Anti-drift guards for the unified config schema.

Mirrors the repo's `fno bundle check` pattern: the model is the single source
of truth, and these tests fail CI the moment a derived artifact drifts.

Four guards:
  1. registry completeness  - every model leaf has exactly one registry entry.
  2. docs freshness         - docs/configuration-guide.md matches the generator.
  3. wizard-key existence   - every wizard-surfaced path is a real model leaf.
  4. bash-default equality  - each `get_config "K" "D"` whose config.K is a
                              modeled leaf has D equal to the model default.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest

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


def test_example_yaml_is_deterministic_and_valid() -> None:
    """The example yaml regenerates byte-identically and is a valid config."""
    import yaml

    from fno.config import SettingsModel

    rendered = schema_gen.render_example_yaml()
    assert rendered == schema_gen.render_example_yaml()
    # Parses as YAML and round-trips through the model (defaults are valid).
    SettingsModel.model_validate(yaml.safe_load(rendered))


def test_committed_example_yaml_is_fresh() -> None:
    """docs/settings.example.yaml must equal the generator's output.

    Regenerate with `fno config schema --yaml --write`.
    """
    example = _repo_root() / "docs" / "settings.example.yaml"
    committed = example.read_text(encoding="utf-8")
    assert committed == schema_gen.render_example_yaml(), (
        "docs/settings.example.yaml is stale; run "
        "`fno config schema --yaml --write`"
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
                leaf = f"config.{key}"
                if leaf not in defaults:
                    continue  # not a modeled leaf; out of scope for this guard
                checked += 1
                if _normalize(defaults[leaf]) != bash_default:
                    mismatches.append(
                        f"{path.relative_to(root)}: get_config \"{key}\" "
                        f"default={bash_default!r} != model default "
                        f"{_normalize(defaults[leaf])!r}"
                    )
    assert checked > 0, "no modeled get_config defaults found to check (guard inert?)"
    assert not mismatches, "bash/model default drift:\n" + "\n".join(mismatches)
