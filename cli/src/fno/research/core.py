"""Core retrieval+store engine for `fno research` (Group 1: retrieve + store).

The `scout` research executor's primitive. One retrieval path:

    ddgs (the backbone) -> self-fetch each URL -> one sources.jsonl row per
    source (``url, fetched_at, hash, extract, verified``).

Because the backbone *self-fetches*, provenance is clean by default: a row is
``verified=true`` only after a successful fetch produced a content hash. A
non-text / 404 / timeout fetch records the row ``verified=false`` with a reason
and never aborts the round (the Group-2 eval's dead-URL assertion catches it).

ddgs is the floor, not a fallback: free, no API key, identical on every host
CLI, clean URLs. Native-provider websearch enrichment is a Group-2 concern.

Fetched page text is DATA, never instructions: nothing here interpolates a
fetched extract into a prompt. The extract is stored and handed to the agent
as quoted evidence (prompt-injection boundary - the first fno subagent acting
on untrusted web content).
"""
from __future__ import annotations

import hashlib
import ipaddress
import json
import re
import socket
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass
from html.parser import HTMLParser
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


# A topic must carry at least this many whitespace tokens. Empty or one-word
# queries ("", "tesla") are refused before a round is spent - too thin to
# retrieve usefully and the dominant typo/paste-mistake shape.
# ponytail: word-count floor, swap for an embedding-similarity gate only if
# single-word topics turn out legitimately common.
MIN_QUERY_WORDS = 2

# Extract cap: enough context to cite from, small enough that sources.jsonl
# stays grep-able. ponytail: fixed cap, raise if claims need longer quotes.
MAX_EXTRACT_CHARS = 4000

# Default breadth per round. ddgs is rate-limited; keep modest.
DEFAULT_MAX_RESULTS = 10

_FETCH_TIMEOUT_S = 15
_UA = "fno-research/0.1 (+https://github.com/jasonnoahchoi/footnote)"

# Content types we treat as fetchable text. A PDF/image/octet-stream is a
# non-text fetch: row recorded verified=false, never crashes the round.
_TEXT_CT_RE = re.compile(r"\b(text/|application/(json|xml|xhtml))", re.IGNORECASE)

_WS_RE = re.compile(r"[ \t\r\f\v]+")
_BLANKLINES_RE = re.compile(r"\n{3,}")


class BlockedHost(urllib.error.URLError):
    """A URL (initial or redirect target) points at a non-public host/scheme.

    Subclasses URLError so fetch_url's existing handler records the row
    verified=false rather than crashing the round. This is the SSRF boundary:
    self-fetching attacker-influenced URLs (search results + their redirects)
    must never reach cloud-metadata (169.254.169.254), loopback, or RFC-1918
    addresses on the operator's host/network.
    """

    def __init__(self, reason: str):
        super().__init__(reason)


def _guard_url(url: str) -> None:
    """Reject non-http(s) schemes and hosts that resolve to a non-public IP.

    ponytail: validates by resolving the host then checking every returned IP.
    A DNS-rebind TOCTOU remains (urllib re-resolves at connect time); closing it
    fully means pinning the connection to the vetted IP. For a research fetcher
    the resolve-and-check guard blocks the realistic SSRF (metadata/internal
    redirects); pin the IP only if rebinding attacks become a real threat.
    """
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ("http", "https"):
        raise BlockedHost(f"blocked scheme: {parts.scheme or 'none'}")
    host = parts.hostname
    if not host:
        raise BlockedHost("blocked: no host")
    try:
        infos = socket.getaddrinfo(host, parts.port or None, proto=socket.IPPROTO_TCP)
    except socket.gaierror as e:
        raise BlockedHost(f"unresolvable host: {host} ({e})") from e
    for info in infos:
        ip = ipaddress.ip_address(info[4][0])
        # IPv4-mapped IPv6 (::ffff:127.0.0.1) does NOT report .is_private/.is_loopback
        # on the IPv6 object - unwrap to the mapped v4 so it can't bypass the guard.
        if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped is not None:
            ip = ip.ipv4_mapped
        if (
            ip.is_private
            or ip.is_loopback
            or ip.is_link_local
            or ip.is_reserved
            or ip.is_multicast
            or ip.is_unspecified
        ):
            raise BlockedHost(f"blocked non-public host: {host} -> {ip}")


class _GuardedRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Re-validate every redirect hop, not just the seed URL."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[override]
        _guard_url(newurl)  # raises BlockedHost (URLError) -> caught by fetch_url
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(_GuardedRedirectHandler())


class DdgsUnavailable(RuntimeError):
    """The `ddgs` backbone is missing or rate-limited.

    Carries an actionable message (install hint) - never a silent-empty.
    """


class EmptyQuery(ValueError):
    """Topic was empty or one-word - refused before spending a round."""


@dataclass
class Source:
    """One evidence row. Schema is the eval's contract; do not reorder keys."""

    url: str
    fetched_at: str
    hash: str
    extract: str
    verified: bool
    reason: str = ""  # why verified=false (empty when verified)

    def to_json_line(self) -> str:
        return json.dumps(asdict(self), ensure_ascii=False)


@dataclass
class FetchResult:
    ok: bool
    text: str
    content_type: str
    status: Optional[int]
    reason: str


# ---------------------------------------------------------------------------
# Query / slug
# ---------------------------------------------------------------------------

def normalize_query(topic: str) -> str:
    """Collapse whitespace; raise EmptyQuery if empty or one-word."""
    q = _WS_RE.sub(" ", (topic or "").strip())
    if not q or len(q.split()) < MIN_QUERY_WORDS:
        raise EmptyQuery(
            f"topic too thin: {topic!r}. Give at least {MIN_QUERY_WORDS} words, "
            'e.g. fno research "CA CCLD financials".'
        )
    return q


def slugify(topic: str) -> str:
    """Filesystem-safe, stable per-topic slug (the sources.jsonl basename)."""
    s = re.sub(r"[^a-z0-9]+", "-", (topic or "").lower()).strip("-")
    return (s[:60].rstrip("-")) or "topic"


# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

def research_dir() -> Path:
    """`~/.fno/notes/research/`, created on first use (US1)."""
    from fno.paths import state_dir

    d = state_dir() / "notes" / "research"
    d.mkdir(parents=True, exist_ok=True)
    return d


def sources_path(topic: str) -> Path:
    return research_dir() / f"{slugify(topic)}.sources.jsonl"


# ---------------------------------------------------------------------------
# Backbone: ddgs search (attribute-access subprocess so tests can stub)
# ---------------------------------------------------------------------------

def _run_ddgs(query: str, max_results: int) -> str:
    """Shell out to the ddgs CLI, return raw stdout (JSON array of hits).

    Raises DdgsUnavailable if the binary is absent or it errors / rate-limits.
    """
    try:
        proc = subprocess.run(
            ["ddgs", "text", "-q", query, "-m", str(max_results), "-o", "json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=30,  # ddgs hits the network; never block the CLI indefinitely
        )
    except FileNotFoundError as e:
        raise DdgsUnavailable(
            "ddgs not found. Install the backbone: `pip install ddgs` "
            "(or `pipx install ddgs`)."
        ) from e
    except subprocess.TimeoutExpired as e:
        raise DdgsUnavailable(f"ddgs search timed out: {e}") from e
    if proc.returncode != 0:
        err = (proc.stderr or "").strip() or f"exit {proc.returncode}"
        raise DdgsUnavailable(f"ddgs failed (rate-limited?): {err}")
    return proc.stdout or ""


def _parse_ddgs(raw: str) -> list[str]:
    """Pull URLs out of ddgs JSON output, tolerant of key naming across versions."""
    raw = raw.strip()
    if not raw:
        return []
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return []
    if isinstance(data, dict):
        data = data.get("results") or data.get("data") or []
    urls: list[str] = []
    seen: set[str] = set()
    for row in data if isinstance(data, list) else []:
        if not isinstance(row, dict):
            continue
        u = row.get("href") or row.get("url") or row.get("link")
        if isinstance(u, str) and u.startswith(("http://", "https://")) and u not in seen:
            seen.add(u)
            urls.append(u)
    return urls


def search(query: str, max_results: int = DEFAULT_MAX_RESULTS) -> list[str]:
    """Return de-duped result URLs for `query`. [] when the backbone finds none."""
    return _parse_ddgs(_run_ddgs(query, max_results))


# ---------------------------------------------------------------------------
# Self-fetch + extract
# ---------------------------------------------------------------------------

class _TextExtractor(HTMLParser):
    """Strip tags + script/style bodies. Uses the stdlib parser instead of a
    tag regex so text containing `<`/`>` (e.g. `x < y and y > z`) survives."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._parts: list[str] = []
        self._skip = False

    def handle_starttag(self, tag, attrs):
        if tag in ("script", "style"):
            self._skip = True
        else:
            self._parts.append(" ")

    def handle_endtag(self, tag):
        if tag in ("script", "style"):
            self._skip = False
        else:
            self._parts.append(" ")

    def handle_data(self, data):
        if not self._skip:
            self._parts.append(data)

    def text(self) -> str:
        return "".join(self._parts)


def _strip_html(body: str) -> str:
    parser = _TextExtractor()
    try:
        parser.feed(body)
    except Exception:  # noqa: BLE001 - a malformed page never aborts the round
        pass
    text = _WS_RE.sub(" ", parser.text())
    return _BLANKLINES_RE.sub("\n\n", text).strip()


def fetch_url(url: str, timeout: int = _FETCH_TIMEOUT_S) -> FetchResult:
    """GET `url`. Non-text / error responses return ok=False with a reason
    (never raise) so one bad source never aborts a round."""
    req = urllib.request.Request(url, headers={"User-Agent": _UA})
    try:
        _guard_url(url)  # SSRF: vet the seed host before the first connect
        with _opener().open(req, timeout=timeout) as resp:  # noqa: S310
            ct = resp.headers.get("Content-Type", "")
            status = getattr(resp, "status", None) or resp.getcode()
            if not _TEXT_CT_RE.search(ct or ""):
                return FetchResult(False, "", ct, status, f"non-text content-type: {ct or 'unknown'}")
            raw = resp.read(2_000_000)  # 2MB cap; ponytail: bigger only if truncation bites
            charset = resp.headers.get_content_charset() or "utf-8"
            text = raw.decode(charset, errors="replace")
            return FetchResult(True, _strip_html(text), ct, status, "")
    except BlockedHost as e:
        return FetchResult(False, "", "", None, str(e.reason))
    except urllib.error.HTTPError as e:
        return FetchResult(False, "", "", e.code, f"http {e.code}")
    except (urllib.error.URLError, TimeoutError, OSError, ValueError) as e:
        return FetchResult(False, "", "", None, f"fetch error: {e}")


def content_hash(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8", errors="replace")).hexdigest()


def build_source(url: str, fetch: FetchResult, *, now: Optional[datetime] = None) -> Source:
    """Turn a fetch into an evidence row. verified=True ONLY on a hashed text fetch."""
    ts = (now or datetime.now(timezone.utc)).isoformat()
    if not fetch.ok:
        return Source(url=url, fetched_at=ts, hash="", extract="", verified=False, reason=fetch.reason)
    extract = fetch.text[:MAX_EXTRACT_CHARS]
    return Source(
        url=url,
        fetched_at=ts,
        hash=content_hash(fetch.text),
        extract=extract,
        verified=True,
    )


# ---------------------------------------------------------------------------
# Evidence store (line-append is the write unit; one writer per topic claim)
# ---------------------------------------------------------------------------

def append_source(path: Path, source: Source) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as fh:
        fh.write(source.to_json_line() + "\n")


def read_sources(path: Path) -> list[Source]:
    """Parse sources.jsonl, skipping malformed rows (best-effort, like maintain)."""
    if not path.exists():
        return []
    out: list[Source] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            d = json.loads(line)
            if not isinstance(d, dict):
                continue  # a bare list/str/number is not a source row
            out.append(
                Source(
                    url=d["url"],
                    fetched_at=d.get("fetched_at", ""),
                    hash=d.get("hash", ""),
                    extract=d.get("extract", ""),
                    verified=bool(d.get("verified", False)),
                    reason=d.get("reason", ""),
                )
            )
        except (json.JSONDecodeError, KeyError, TypeError):
            continue  # skip the row, keep going
    return out


# ---------------------------------------------------------------------------
# Topic single-writer claim (stubbable subprocess; non-fatal if absent)
# ---------------------------------------------------------------------------

def _run_claim(args: list[str]) -> tuple[int, str]:
    try:
        proc = subprocess.run(
            ["fno-py", "claim", *args], capture_output=True, text=True, check=False, timeout=10
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        # Missing binary OR a hung/contended lock: degrade to the single-process
        # assumption rather than blocking the whole pipeline.
        return (127, "")
    return (proc.returncode, (proc.stdout or "").strip())


def acquire_topic(slug: str, holder: str) -> bool:
    """Acquire `node:research:<slug>` so cache + sources.jsonl have one writer.

    Returns True if held by us / acquired. Returns False ONLY when another live
    holder owns it (refuse). A missing claim binary degrades to True (proceed):
    the claim is a guard, not a hard dependency.
    """
    key = f"node:research:{slug}"
    rc, _out = _run_claim(["acquire", key, "--holder", holder, "--ttl", "2h"])
    if rc == 127:
        return True  # claim primitive unavailable; degrade to single-process assumption
    return rc == 0


def release_topic(slug: str, holder: str) -> None:
    _run_claim(["release", f"node:research:{slug}", "--holder", holder])


# ---------------------------------------------------------------------------
# Orchestration
# ---------------------------------------------------------------------------

@dataclass
class RoundResult:
    topic: str
    slug: str
    sources_path: str
    found: int
    verified: int
    failed: int
    note: str = ""


def run_round(
    topic: str,
    *,
    max_results: int = DEFAULT_MAX_RESULTS,
    holder: Optional[str] = None,
    claim: bool = True,
) -> RoundResult:
    """One retrieve+store round: validate -> claim -> search -> self-fetch ->
    append rows. Always writes the (possibly empty) sources file so downstream
    has a path to read."""
    query = normalize_query(topic)  # raises EmptyQuery
    slug = slugify(query)
    holder = holder or f"research:{slug}"
    out = sources_path(query)

    if claim and not acquire_topic(slug, holder):
        return RoundResult(query, slug, str(out), 0, 0, 0, note="topic claimed by another writer")

    try:
        urls = search(query, max_results)  # raises DdgsUnavailable
        if not urls:
            # Zero results: write nothing, stamp the note. DoneAdvisory upstream.
            out.parent.mkdir(parents=True, exist_ok=True)
            out.touch(exist_ok=True)
            return RoundResult(query, slug, str(out), 0, 0, 0, note="no sources found")
        verified = failed = 0
        for url in urls:
            src = build_source(url, fetch_url(url))
            append_source(out, src)
            if src.verified:
                verified += 1
            else:
                failed += 1
        return RoundResult(query, slug, str(out), len(urls), verified, failed)
    finally:
        if claim:
            release_topic(slug, holder)
