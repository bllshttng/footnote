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


class _FakeOpener:
    """Stands in for _opener(); .open() returns/raises like urlopen would.

    fetch_url goes through _opener().open(...), so tests stub the opener rather
    than urlopen. _guard_url is also stubbed to a public-IP no-op so these tests
    isolate the fetch/extract behavior from the SSRF resolver.
    """

    def __init__(self, result):
        self._result = result

    def open(self, *a, **k):
        if isinstance(self._result, Exception):
            raise self._result
        return self._result


@pytest.fixture
def _guard_ok(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "_guard_url", lambda url: None)


def test_fetch_text_ok_and_html_stripped(monkeypatch: pytest.MonkeyPatch, _guard_ok) -> None:
    html = b"<html><script>bad()</script><body><h1>Hi</h1> there</body></html>"
    monkeypatch.setattr(core, "_opener", lambda: _FakeOpener(_Resp(html, "text/html")))
    r = core.fetch_url("https://x.com")
    assert r.ok and "Hi there" in r.text and "bad()" not in r.text


def test_fetch_keeps_angle_brackets_in_text(monkeypatch: pytest.MonkeyPatch, _guard_ok) -> None:
    # The regex stripper would eat "< y and y >" as a tag; HTMLParser keeps it.
    html = b"<p>compare x &lt; y and y &gt; z here</p>"
    monkeypatch.setattr(core, "_opener", lambda: _FakeOpener(_Resp(html, "text/html")))
    r = core.fetch_url("https://x.com")
    assert r.ok and "x < y and y > z here" in r.text


def test_fetch_non_text_recorded_unverified(monkeypatch: pytest.MonkeyPatch, _guard_ok) -> None:
    monkeypatch.setattr(core, "_opener", lambda: _FakeOpener(_Resp(b"%PDF", "application/pdf")))
    r = core.fetch_url("https://x.com/doc.pdf")
    assert not r.ok and "non-text" in r.reason


def test_fetch_http_error_recorded_unverified(monkeypatch: pytest.MonkeyPatch, _guard_ok) -> None:
    err = core.urllib.error.HTTPError("https://x", 404, "nf", {}, None)
    monkeypatch.setattr(core, "_opener", lambda: _FakeOpener(err))
    r = core.fetch_url("https://x.com")
    assert not r.ok and "404" in r.reason


def test_build_source_verified_only_with_hash() -> None:
    ok = core.build_source("https://x", core.FetchResult(True, "body text", "text/html", 200, ""))
    assert ok.verified and ok.hash and ok.extract == "body text"
    bad = core.build_source("https://y", core.FetchResult(False, "", "", 404, "http 404"))
    assert not bad.verified and bad.hash == "" and bad.reason == "http 404"


# --- SSRF guard ------------------------------------------------------------

def _addrinfo(ip: str):
    return [(2, 1, 6, "", (ip, 0))]


@pytest.mark.parametrize("scheme", ["ftp", "file", "gopher", ""])
def test_guard_rejects_non_http_scheme(scheme: str) -> None:
    url = f"{scheme}://x.com/a" if scheme else "x.com/a"
    with pytest.raises(core.BlockedHost):
        core._guard_url(url)


@pytest.mark.parametrize(
    "ip",
    ["127.0.0.1", "169.254.169.254", "10.0.0.5", "192.168.1.1", "172.16.0.1", "::1"],
)
def test_guard_rejects_non_public_hosts(ip: str, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core.socket, "getaddrinfo", lambda *a, **k: _addrinfo(ip))
    with pytest.raises(core.BlockedHost):
        core._guard_url("https://evil.example/meta")


@pytest.mark.parametrize("ip", ["::ffff:127.0.0.1", "::ffff:169.254.169.254", "::ffff:10.0.0.1"])
def test_guard_rejects_ipv4_mapped_ipv6_bypass(ip: str, monkeypatch: pytest.MonkeyPatch) -> None:
    # ::ffff:127.0.0.1 does not report .is_loopback on the IPv6 object - the
    # guard must unwrap the mapped v4 or the SSRF protection is bypassable.
    monkeypatch.setattr(core.socket, "getaddrinfo", lambda *a, **k: _addrinfo(ip))
    with pytest.raises(core.BlockedHost):
        core._guard_url("https://evil.example/meta")


def test_guard_allows_public_host(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core.socket, "getaddrinfo", lambda *a, **k: _addrinfo("93.184.216.34"))
    core._guard_url("https://example.com/page")  # does not raise


def test_guard_rejects_unresolvable(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(*a, **k):
        raise core.socket.gaierror("nope")

    monkeypatch.setattr(core.socket, "getaddrinfo", boom)
    with pytest.raises(core.BlockedHost):
        core._guard_url("https://nx.example/x")


def test_fetch_blocked_seed_recorded_unverified(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core.socket, "getaddrinfo", lambda *a, **k: _addrinfo("127.0.0.1"))
    # urlopen must never be reached for a blocked seed.
    monkeypatch.setattr(
        core.urllib.request, "urlopen",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("should not fetch")),
    )
    r = core.fetch_url("http://localhost:8000/admin")
    assert not r.ok and "non-public host" in r.reason


def test_redirect_handler_revalidates_hop(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core.socket, "getaddrinfo", lambda *a, **k: _addrinfo("10.0.0.9"))
    h = core._GuardedRedirectHandler()
    with pytest.raises(core.BlockedHost):
        h.redirect_request(None, None, 302, "Found", {}, "http://internal.svc/secret")


# --- store round-trip ------------------------------------------------------

def test_append_and_read_sources_skips_malformed(tmp_path: Path) -> None:
    p = tmp_path / "t.sources.jsonl"
    core.append_source(p, core.Source("https://a", "2026-01-01", "h", "x", True))
    p.open("a").write("{not json}\n")
    p.open("a").write('["a bare list is not a row"]\n')
    p.open("a").write("42\n")
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
