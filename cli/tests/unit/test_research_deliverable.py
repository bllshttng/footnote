"""Tests for the `doc` deliverable (US3): brief + sidecar -> output_dir.

Covers AC1 (cited brief + sidecar written, DoneAdvisory), AC3 (no-sources brief
stamped, not a crash), AC4 (round-cap stop stamped), AC5 (output_dir unset =>
fail loud, never guess).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.research import deliverable as deli
from fno.research.core import Source


def _write_sources(path: Path, rows: list[Source]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for r in rows:
            fh.write(r.to_json_line() + "\n")


def _verified(url: str, extract: str) -> Source:
    return Source(url=url, fetched_at="2026-06-23T00:00:00+00:00", hash="abc", extract=extract, verified=True)


# --------------------------------------------------------------------------- #
# resolve_output_dir - AC5
# --------------------------------------------------------------------------- #


def test_resolve_output_dir_unset_fails_loud() -> None:
    """No output_dir configured => OutputDirUnset (the ship step never guesses)."""
    with pytest.raises(deli.OutputDirUnset):
        deli.resolve_output_dir(configured=None)


def test_resolve_output_dir_unset_empty_string_fails_loud() -> None:
    """An empty/whitespace output_dir is treated as unset (fail loud)."""
    with pytest.raises(deli.OutputDirUnset):
        deli.resolve_output_dir(configured="   ")


def test_resolve_output_dir_expands_and_creates(tmp_path: Path) -> None:
    """A set output_dir is expanded and created (parents ok)."""
    target = tmp_path / "vault" / "research"
    out = deli.resolve_output_dir(configured=str(target))
    assert out == target
    assert out.is_dir()


# --------------------------------------------------------------------------- #
# build_brief - claims cite sources, frontmatter stamps the stop reason
# --------------------------------------------------------------------------- #


def test_build_brief_cites_every_verified_source() -> None:
    srcs = [_verified("https://a.example/x", "alpha finding text"),
            _verified("https://b.example/y", "beta finding text")]
    md = deli.build_brief("my research topic", "my-research-topic", srcs, stopped="declared")
    # frontmatter records topic + stop reason + sidecar name
    assert "stopped: declared" in md
    assert "my-research-topic.sources.jsonl" in md
    # one cited claim per verified source; citations resolve in a Sources section
    assert "[S1]" in md and "[S2]" in md
    assert "[S1]: https://a.example/x" in md
    assert "[S2]: https://b.example/y" in md


def test_build_brief_skips_unverified_sources() -> None:
    srcs = [_verified("https://a.example/x", "alpha"),
            Source(url="https://dead.example", fetched_at="t", hash="", extract="", verified=False, reason="http 404")]
    md = deli.build_brief("topic two words", "topic-two-words", srcs, stopped="declared")
    # the dead source is not cited as a claim (only verified rows become claims)
    assert "https://dead.example" not in md
    assert md.count("[S1]:") == 1


def test_build_brief_no_sources_stamped() -> None:
    """AC3: zero sources -> brief stamped 'no sources found', not a crash."""
    md = deli.build_brief("empty topic here", "empty-topic-here", [], stopped="declared")
    assert "no sources found" in md.lower()


def test_build_brief_round_cap_stamped() -> None:
    """AC4: a round-cap stop is recorded in frontmatter (truncation is stated)."""
    md = deli.build_brief("capped topic words", "capped-topic-words",
                          [_verified("https://a.example", "x")], stopped="cap 5")
    assert "stopped: cap 5" in md


# --------------------------------------------------------------------------- #
# deliver - the end-to-end ship step
# --------------------------------------------------------------------------- #


def test_deliver_writes_brief_and_sidecar(tmp_path: Path) -> None:
    """AC1: deliver writes <slug>.md + <slug>.sources.jsonl to output_dir and
    reports DoneAdvisory."""
    cache = tmp_path / "cache" / "my-topic-words.sources.jsonl"
    _write_sources(cache, [_verified("https://a.example/p", "finding p")])
    out = tmp_path / "out"

    res = deli.deliver(
        "my topic words",
        sources_path=cache,
        stopped="declared",
        output_dir=str(out),
    )

    assert res.terminated == "DoneAdvisory"
    brief = out / "my-topic-words.md"
    sidecar = out / "my-topic-words.sources.jsonl"
    assert brief.is_file() and sidecar.is_file()
    assert Path(res.brief_path) == brief
    # sidecar beside the brief carries the same rows as the cache
    assert json.loads(sidecar.read_text().splitlines()[0])["url"] == "https://a.example/p"


def test_deliver_unset_output_dir_fails_loud(tmp_path: Path) -> None:
    """AC5: deliver with no output_dir raises OutputDirUnset (non-zero upstream)."""
    cache = tmp_path / "c" / "t.sources.jsonl"
    _write_sources(cache, [_verified("https://a.example", "x")])
    with pytest.raises(deli.OutputDirUnset):
        deli.deliver("topic words here", sources_path=cache, stopped="declared", output_dir=None)


def test_deliver_no_sources_still_ships(tmp_path: Path) -> None:
    """AC3: a missing/empty cache still ships a stamped brief, DoneAdvisory."""
    cache = tmp_path / "c" / "none.sources.jsonl"
    cache.parent.mkdir(parents=True)
    cache.touch()
    out = tmp_path / "out"
    res = deli.deliver("nothing found topic", sources_path=cache, stopped="declared", output_dir=str(out))
    assert res.terminated == "DoneAdvisory"
    assert (out / "nothing-found-topic.md").is_file()
    assert "no sources found" in (out / "nothing-found-topic.md").read_text().lower()
