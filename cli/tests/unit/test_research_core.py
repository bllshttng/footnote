"""Unit tests for fno.research.core - the retrieve+store engine (Group 1).

Covers the design-doc ACs/failure-modes that live in this module:
- AC1 (happy path): search -> self-fetch -> sources.jsonl rows.
- AC3 (no sources): zero ddgs results -> stamped note, no crash.
- Boundaries: empty/one-word query refused; non-text/404 -> verified=false.
- Invariants: verified=true only on a hashed text fetch; malformed row skipped.
- Concurrency: a topic held by another writer refuses (no write).

Filesystem isolation: research_dir() resolves via fno.paths.state_dir, which
we monkeypatch to tmp_path so nothing touches the real ~/.fno.
"""
from __future__ import annotations

from pathlib import Path

import pytest

import fno.paths
from fno.research import core


@pytest.fixture(autouse=True)
def _tmp_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr(fno.paths, "state_dir", lambda: tmp_path)
    return tmp_path


# --- query / slug ----------------------------------------------------------

@pytest.mark.parametrize("bad", ["", "   ", "tesla", "  oneword  "])
def test_empty_or_oneword_query_refused(bad: str) -> None:
    with pytest.raises(core.EmptyQuery):
        core.normalize_query(bad)


def test_normalize_collapses_whitespace() -> None:
    assert core.normalize_query("  CA   CCLD\tfinancials ") == "CA CCLD financials"


def test_slugify_stable_and_safe() -> None:
    assert core.slugify("CA CCLD financials!") == "ca-ccld-financials"
    assert core.slugify("") == "topic"
    assert "/" not in core.slugify("a/b c")


def test_research_dir_created_on_first_use(_tmp_state: Path) -> None:
    d = core.research_dir()
    assert d == _tmp_state / "notes" / "research"
    assert d.is_dir()


# --- ddgs parse ------------------------------------------------------------

def test_parse_ddgs_tolerant_of_keys_and_dedupes() -> None:
    raw = (
        '[{"href": "https://a.com"}, {"url": "https://b.com"}, '
        '{"link": "https://a.com"}, {"title": "no url"}, {"href": "ftp://x"}]'
    )
    assert core._parse_ddgs(raw) == ["https://a.com", "https://b.com"]


def test_parse_ddgs_empty_and_malformed() -> None:
    assert core._parse_ddgs("") == []
    assert core._parse_ddgs("not json") == []
    assert core._parse_ddgs('{"results": [{"href": "https://a.com"}]}') == ["https://a.com"]


def test_search_missing_binary_raises_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(*a, **k):
        raise FileNotFoundError()

    monkeypatch.setattr(core.subprocess, "run", boom)
    with pytest.raises(core.DdgsUnavailable):
        core.search("two words")


# --- fetch / source build --------------------------------------------------

class _Resp:
    def __init__(self, body: bytes, ct: str, status: int = 200):
        self._body = body
        self.headers = _Headers(ct)
        self.status = status

    def read(self, n: int = -1) -> bytes:
        return self._body

    def getcode(self) -> int:
        return self.status

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class _Headers:
    def __init__(self, ct: str):
        self._ct = ct

    def get(self, k: str, default=""):
        return self._ct if k.lower() == "content-type" else default

    def get_content_charset(self):
        return "utf-8"


def test_fetch_text_ok_and_html_stripped(monkeypatch: pytest.MonkeyPatch) -> None:
    html = b"<html><script>bad()</script><body><h1>Hi</h1> there</body></html>"
    monkeypatch.setattr(core.urllib.request, "urlopen", lambda *a, **k: _Resp(html, "text/html"))
    r = core.fetch_url("https://x.com")
    assert r.ok and "Hi there" in r.text and "bad()" not in r.text


def test_fetch_non_text_recorded_unverified(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core.urllib.request, "urlopen", lambda *a, **k: _Resp(b"%PDF", "application/pdf"))
    r = core.fetch_url("https://x.com/doc.pdf")
    assert not r.ok and "non-text" in r.reason


def test_fetch_http_error_recorded_unverified(monkeypatch: pytest.MonkeyPatch) -> None:
    def raise_404(*a, **k):
        raise core.urllib.error.HTTPError("https://x", 404, "nf", {}, None)

    monkeypatch.setattr(core.urllib.request, "urlopen", raise_404)
    r = core.fetch_url("https://x.com")
    assert not r.ok and "404" in r.reason


def test_build_source_verified_only_with_hash() -> None:
    ok = core.build_source("https://x", core.FetchResult(True, "body text", "text/html", 200, ""))
    assert ok.verified and ok.hash and ok.extract == "body text"
    bad = core.build_source("https://y", core.FetchResult(False, "", "", 404, "http 404"))
    assert not bad.verified and bad.hash == "" and bad.reason == "http 404"


# --- store round-trip ------------------------------------------------------

def test_append_and_read_sources_skips_malformed(tmp_path: Path) -> None:
    p = tmp_path / "t.sources.jsonl"
    core.append_source(p, core.Source("https://a", "2026-01-01", "h", "x", True))
    p.open("a").write("{not json}\n")
    core.append_source(p, core.Source("https://b", "2026-01-01", "", "", False, "http 500"))
    rows = core.read_sources(p)
    assert [r.url for r in rows] == ["https://a", "https://b"]
    assert rows[0].verified and not rows[1].verified


# --- orchestration ---------------------------------------------------------

def test_run_round_happy_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "search", lambda q, m=10: ["https://a.com", "https://b.com"])
    monkeypatch.setattr(
        core, "fetch_url",
        lambda u, timeout=15: core.FetchResult(True, f"body {u}", "text/html", 200, ""),
    )
    res = core.run_round("two good words", claim=False)
    assert res.found == 2 and res.verified == 2 and res.failed == 0
    rows = core.read_sources(Path(res.sources_path))
    assert len(rows) == 2 and all(r.verified for r in rows)


def test_run_round_no_sources_stamps_note(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "search", lambda q, m=10: [])
    res = core.run_round("nothing here ever", claim=False)
    assert res.note == "no sources found" and res.found == 0
    assert Path(res.sources_path).exists()  # empty file written


def test_run_round_mixed_verified(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "search", lambda q, m=10: ["https://ok", "https://bad"])

    def fetch(u, timeout=15):
        if u.endswith("ok"):
            return core.FetchResult(True, "good", "text/plain", 200, "")
        return core.FetchResult(False, "", "", 403, "http 403")

    monkeypatch.setattr(core, "fetch_url", fetch)
    res = core.run_round("mixed bag topic", claim=False)
    assert res.verified == 1 and res.failed == 1


def test_run_round_refuses_when_claimed(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "acquire_topic", lambda slug, holder: False)
    called = {"search": False}
    monkeypatch.setattr(core, "search", lambda *a, **k: called.__setitem__("search", True) or [])
    res = core.run_round("held topic words", claim=True)
    assert "claimed by another" in res.note and not called["search"]


def test_acquire_topic_degrades_when_binary_missing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "_run_claim", lambda args: (127, ""))
    assert core.acquire_topic("slug", "holder") is True  # proceed, single-process assumption
