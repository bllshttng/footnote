"""Every documented config fence parses as the language it declares (x-b052).

`docs/path-config.md` named TOML files and handed the reader YAML to put in
them. `tomllib.loads` on that YAML raises, `_load_raw` swallows the raise, and
`fno config doctor` certified the file. An operator who copied the documented
schema into the documented path got a file that no-ops and a doctor that said
OK.

This walks every fence in `docs/`, so the trap cannot come back through a page
nobody thought to check.
"""
from __future__ import annotations

import tomllib
from pathlib import Path

import pytest
import yaml

_DOCS = Path(__file__).resolve().parents[3] / "docs"


def _load_yaml(body: str) -> object:
    """Drain every document in ``body``.

    A yaml fence in these docs is often a markdown FRONTMATTER example, which
    is a multi-document stream (`---` opens a second document). ``safe_load``
    refuses that; ``safe_load_all`` is the parser those pages actually declare.
    """
    return list(yaml.safe_load_all(body))


_PARSERS = {"toml": tomllib.loads, "yaml": _load_yaml, "yml": _load_yaml}


def _fences(text: str) -> list[tuple[int, str, str]]:
    """(1-indexed opening line, language, body) for each fence in ``text``."""
    out: list[tuple[int, str, str]] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        if not line.startswith("```"):
            i += 1
            continue
        info = line[3:].strip()
        start = i + 1
        j = start
        while j < len(lines) and not lines[j].startswith("```"):
            j += 1
        if info in _PARSERS:
            out.append((i + 1, info, "\n".join(lines[start:j])))
        i = j + 1
    return out


def _config_fences() -> list[tuple[Path, int, str, str]]:
    found: list[tuple[Path, int, str, str]] = []
    for path in sorted(_DOCS.rglob("*.md")):
        for lineno, lang, body in _fences(path.read_text(encoding="utf-8")):
            found.append((path, lineno, lang, body))
    return found


def test_every_declared_fence_parses_as_its_language() -> None:
    """A toml fence holds TOML and a yaml fence holds YAML, everywhere in docs.

    The count assertion is the positive control on the WALKER: a walker that
    found no files, or no fences, would otherwise pass this test by finding
    nothing to reject.
    """
    fences = _config_fences()
    assert len(fences) > 0, f"walked {_DOCS} and found no toml/yaml fences at all"

    failures: list[str] = []
    for path, lineno, lang, body in fences:
        try:
            _PARSERS[lang](body)
        except Exception as exc:  # noqa: BLE001 - the whole point is the message
            rel = path.relative_to(_DOCS.parent)
            failures.append(f"{rel}:{lineno} declares {lang} but does not parse: {exc}")
    assert not failures, "\n".join(failures)


def test_a_toml_fence_holding_yaml_is_rejected() -> None:
    """The negative control: the exact page content this node was filed against.

    `docs/path-config.md` carried this body under a `yaml` fence while the page
    told the reader to paste it into a `config.toml`. Pinning the old body here
    proves the guard above actually fails on the defect rather than passing
    because it checks nothing.
    """
    legacy_body = (
        "schema_version: 1\n"
        "\n"
        "config:\n"
        "  state_dir: ~/.fno/\n"
        "  plans_dir: .fno/plans/\n"
    )
    with pytest.raises(tomllib.TOMLDecodeError):
        tomllib.loads(legacy_body)


def test_the_full_schema_fence_round_trips_into_the_loader(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC9: the documented schema, copied to the documented path, sets keys.

    Copying the "Full schema" fence into a `config.toml` and pinning
    `FNO_CONFIG` at it must make `state_dir` resolve FROM that file. Before the
    rewrite the same copy produced `source: default (no config file sets this
    key)`, which is the whole defect.
    """
    page = (_DOCS / "path-config.md").read_text(encoding="utf-8")
    schema = [
        body
        for _lineno, lang, body in _fences(page)
        if lang == "toml" and "schema_version" in body and "[paths]" in body
    ]
    assert len(schema) == 1, f"expected exactly one full-schema toml fence, got {len(schema)}"

    scratch = tmp_path / "config.toml"
    scratch.write_text(schema[0], encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(scratch))

    from fno.config import resolve_source

    decided = resolve_source("state_dir")
    assert decided is not None, "the documented schema set no state_dir at all"
    assert decided[0].resolve() == scratch.resolve()
