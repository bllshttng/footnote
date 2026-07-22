"""Unit tests for retro harvest (Wave 3.1, US2)."""
from __future__ import annotations

from pathlib import Path

from fno.retro import harvest
from fno.retro.types import KIND_CARVEOUT, KIND_DEFERRED, KIND_REVIEW


# --- severity normalization (AC2-EDGE) ------------------------------------

def test_normalize_severity_badges():
    assert harvest.normalize_severity("![critical] boom") == "critical"
    assert harvest.normalize_severity("![high] x") == "high"
    assert harvest.normalize_severity("![P1 Badge] x") == "high"
    assert harvest.normalize_severity("![P2 Badge] x") == "medium"
    assert harvest.normalize_severity("![P3 Badge] x") == "low"


def test_ac2_edge_no_badge_defaults_medium():
    """AC2-EDGE: a comment with no severity badge defaults to medium."""
    assert harvest.normalize_severity("just a plain comment, no badge") == "medium"


# --- declined-finding harvest (AC2-HP, AC2-FR) ----------------------------

def test_ac2_hp_declined_vs_implemented():
    """AC2-HP: 3 findings, 1 implemented (resolved), 2 declined (High + Low)."""
    comments = [
        {"id": "1", "body": "![high] missing null check", "reviewer": "gemini[bot]"},
        {"id": "2", "body": "![low] nit: rename var", "reviewer": "gemini[bot]"},
        {"id": "3", "body": "![high] fixed this one", "reviewer": "gemini[bot]"},
    ]
    items = harvest.harvest_reviews(
        comments=comments, resolved_ids={"3"}, source_pr=42
    )
    assert len(items) == 2
    by_id = {i.source_id: i for i in items}
    assert by_id["1"].severity == "high"
    assert by_id["2"].severity == "low"
    assert "3" not in by_id  # implemented -> not surfaced
    assert all(i.kind == KIND_REVIEW and i.source_pr == 42 for i in items)


def test_ac2_fr_fix_evidence_wins_over_skipped_table():
    """AC2-FR: author table says skipped, but a fix commit addressed it -> fix wins, logged."""
    comments = [{"id": "9", "body": "![medium] thing", "reviewer": "codex[bot]"}]
    warnings: list[str] = []
    items = harvest.harvest_reviews(
        comments=comments,
        resolved_ids={"9"},
        skipped_ids={"9"},
        source_pr=7,
        warnings=warnings,
    )
    assert items == []  # excluded
    assert any("fix evidence wins" in w for w in warnings)


# --- gh error handling (AC2-ERR) ------------------------------------------

def test_ac2_err_gh_failure_yields_nothing_with_warning():
    """AC2-ERR: gh error -> no comments, a WARN names the source."""
    def failing_gh(args):
        return 1, "", "HTTP 403: rate limit exceeded"

    warnings: list[str] = []
    comments = harvest.fetch_review_comments(
        99, gh_runner=failing_gh, warnings=warnings
    )
    assert comments == []
    assert any("gh api` failed" in w and "#99" in w for w in warnings)


def test_fetch_review_comments_parses_gh_json():
    def ok_gh(args):
        payload = (
            '[{"id": 555, "body": "![high] x", "html_url": "http://c/1",'
            ' "user": {"login": "gemini[bot]"}}]'
        )
        return 0, payload, ""

    comments = harvest.fetch_review_comments(5, gh_runner=ok_gh)
    assert len(comments) == 1
    assert comments[0]["id"] == "555"
    assert comments[0]["reviewer"] == "gemini[bot]"


def test_fetch_review_comments_flattens_slurped_pages():
    """--paginate --slurp yields [[page1],[page2]]; flatten to one comment list."""
    seen = {}

    def slurp_gh(args):
        seen["args"] = args
        payload = (
            '[[{"id": 1, "body": "a", "user": {"login": "g[bot]"}}],'
            ' [{"id": 2, "body": "b", "user": {"login": "g[bot]"}}]]'
        )
        return 0, payload, ""

    comments = harvest.fetch_review_comments(7, gh_runner=slurp_gh)
    assert [c["id"] for c in comments] == ["1", "2"]  # both pages flattened
    assert "--slurp" in seen["args"] and "--paginate" in seen["args"]


# --- carve-out harvest (malformed-line tolerance) -------------------------

def test_harvest_carveouts_filters_session_and_skips_malformed(tmp_path: Path):
    fnodir = tmp_path / ".fno"
    fnodir.mkdir()
    (fnodir / "carveouts.jsonl").write_text(
        '{"id":"cv-1","session_id":"S1","kind":"deferred","need":"q","description":"d1"}\n'
        "this is not json\n"
        '{"id":"cv-2","session_id":"S2","kind":"oos-bug","description":"d2"}\n',
        encoding="utf-8",
    )
    warnings: list[str] = []
    items = harvest.harvest_carveouts(
        tmp_path, session_ids={"S1"}, source_pr=3, warnings=warnings
    )
    assert len(items) == 1
    assert items[0].kind == KIND_CARVEOUT
    assert items[0].text == "d1"
    assert items[0].subkind == "deferred"
    assert items[0].title_hint == "q"
    assert any("malformed JSON" in w for w in warnings)


def test_harvest_carveouts_all_sessions_when_unfiltered(tmp_path: Path):
    fnodir = tmp_path / ".fno"
    fnodir.mkdir()
    (fnodir / "carveouts.jsonl").write_text(
        '{"id":"cv-1","session_id":"S1","kind":"deferred","description":"d1"}\n'
        '{"id":"cv-2","session_id":"S2","kind":"oos-bug","description":"d2"}\n',
        encoding="utf-8",
    )
    items = harvest.harvest_carveouts(tmp_path)
    assert len(items) == 2


def test_harvest_carveouts_skips_non_object_line(tmp_path: Path):
    """A valid-JSON but non-object line (bare list/string) is skipped with a warning,
    never an AttributeError crashing the harvest (gemini high on PR #465)."""
    fnodir = tmp_path / ".fno"
    fnodir.mkdir()
    (fnodir / "carveouts.jsonl").write_text(
        '[1, 2, 3]\n'  # valid JSON, not an object
        '"just a string"\n'
        '{"id":"cv-1","kind":"deferred","description":"d1"}\n',
        encoding="utf-8",
    )
    warnings: list[str] = []
    items = harvest.harvest_carveouts(tmp_path, warnings=warnings)
    assert [i.source_id for i in items] == ["cv-1"]  # the two non-objects skipped
    assert sum("not a JSON object" in w for w in warnings) == 2


def test_harvest_carveouts_skips_backfill(tmp_path: Path):
    """ab-4a1a4fea: a kind:backfill carve-out is routed to /pr merged's backfill
    slot, NOT swept into generic retro triage. It must SURVIVE the harvest (never
    classified, never returned in harvested ids the caller would consume)."""
    fnodir = tmp_path / ".fno"
    fnodir.mkdir()
    (fnodir / "carveouts.jsonl").write_text(
        '{"id":"cv-1","session_id":"S1","kind":"deferred","description":"d1"}\n'
        '{"id":"cv-2","session_id":"S1","kind":"backfill","need":"mig","description":"bf"}\n',
        encoding="utf-8",
    )
    items = harvest.harvest_carveouts(tmp_path, session_ids={"S1"})
    assert [i.source_id for i in items] == ["cv-1"]  # backfill excluded
    assert all(i.subkind != "backfill" for i in items)


# --- deferred-finding harvest from COMPLETION.md --------------------------

def test_harvest_deferred_findings_parses_section(tmp_path: Path):
    comp = tmp_path / "COMPLETION.md"
    comp.write_text(
        "# Completion\n\n"
        "## Deferred Findings (from done-with-concerns verdicts)\n\n"
        "### review-x (phase: review, session: S1)\n"
        "- finding one needs follow-up\n"
        "- finding two also deferred\n\n"
        "## Commits\n- abc123\n",
        encoding="utf-8",
    )
    items = harvest.harvest_deferred_findings(comp, source_pr=11)
    assert len(items) == 2
    assert all(i.kind == KIND_DEFERRED and i.source_pr == 11 for i in items)
    assert items[0].text == "finding one needs follow-up"


def test_harvest_deferred_findings_missing_file(tmp_path: Path):
    assert harvest.harvest_deferred_findings(tmp_path / "nope.md") == []


# --- resolved-thread state (LATEST review state, ab-bb7fa74f) --------------

import json as _json


def _graphql_page(nodes, *, has_next=False, end_cursor=None):
    return _json.dumps(
        {
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "pageInfo": {
                                "hasNextPage": has_next,
                                "endCursor": end_cursor,
                            },
                            "nodes": nodes,
                        }
                    }
                }
            }
        }
    )


def test_fetch_review_thread_state_collects_resolved_ids():
    """A resolved thread's comment databaseIds become resolved_ids (LATEST state)."""
    nodes = [
        {
            "isResolved": True,
            "path": "src/a.py",
            "comments": {"nodes": [{"databaseId": 111, "body": "fix me"}]},
        },
        {
            "isResolved": False,
            "path": "src/b.py",
            "comments": {"nodes": [{"databaseId": 222, "body": "still open"}]},
        },
    ]

    def gh(args):
        assert args[:2] == ["api", "graphql"]
        return 0, _graphql_page(nodes), ""

    threads, unavail = harvest.fetch_review_thread_state(7, repo="o/r", gh_runner=gh)
    assert unavail is False
    assert harvest.resolved_ids_from_threads(threads) == {"111"}
    assert {t["path"] for t in threads} == {"src/a.py", "src/b.py"}


def test_addressed_ids_includes_resolved_and_outdated():
    """addressed = resolved OR outdated; a still-open thread is NOT suppressed (ab-158ab951)."""
    threads = [
        {"is_resolved": True, "is_outdated": False, "comment_ids": ["r1"]},
        {"is_resolved": False, "is_outdated": True, "comment_ids": ["o1"]},   # fix pushed, not resolved
        {"is_resolved": False, "is_outdated": False, "comment_ids": ["open1"]},
    ]
    assert harvest.addressed_ids_from_threads(threads) == {"r1", "o1"}
    # resolved_ids_from_threads stays resolved-only (unchanged contract).
    assert harvest.resolved_ids_from_threads(threads) == {"r1"}
    # Back-compat: a thread dict predating the is_outdated key reads as not-outdated.
    assert harvest.addressed_ids_from_threads(
        [{"is_resolved": False, "comment_ids": ["x"]}]
    ) == set()


def test_fetch_review_thread_state_parses_is_outdated():
    """An unresolved-but-OUTDATED thread's comments become addressed (suppressed)."""
    nodes = [
        {
            "isResolved": False,
            "isOutdated": True,
            "path": "src/a.py",
            "comments": {"nodes": [{"databaseId": 42}]},
        },
    ]

    def gh(args):
        return 0, _graphql_page(nodes), ""

    threads, unavail = harvest.fetch_review_thread_state(7, repo="o/r", gh_runner=gh)
    assert unavail is False
    assert threads[0]["is_outdated"] is True
    assert harvest.resolved_ids_from_threads(threads) == set()  # not resolved
    assert harvest.addressed_ids_from_threads(threads) == {"42"}  # but addressed


def test_fetch_review_thread_state_paginates():
    """hasNextPage walks the cursor and accumulates both pages."""
    page1 = _graphql_page(
        [{"isResolved": True, "path": "f", "comments": {"nodes": [{"databaseId": 1}]}}],
        has_next=True,
        end_cursor="CUR",
    )
    page2 = _graphql_page(
        [{"isResolved": True, "path": "g", "comments": {"nodes": [{"databaseId": 2}]}}]
    )
    calls = []

    def gh(args):
        calls.append(args)
        return (0, page1, "") if len(calls) == 1 else (0, page2, "")

    threads, unavail = harvest.fetch_review_thread_state(9, repo="o/r", gh_runner=gh)
    assert unavail is False
    assert harvest.resolved_ids_from_threads(threads) == {"1", "2"}
    assert len(calls) == 2
    assert any("cursor=CUR" in a for a in calls[1])  # second call carried the cursor


def test_fetch_review_thread_state_gh_failure_marks_unavailable():
    def gh(args):
        return 1, "", "HTTP 403 rate limit"

    warnings: list[str] = []
    threads, unavail = harvest.fetch_review_thread_state(
        9, repo="o/r", gh_runner=gh, warnings=warnings
    )
    assert threads == [] and unavail is True
    assert any("graphql" in w and "#9" in w for w in warnings)


def test_fetch_review_thread_state_no_repo_is_unavailable():
    warnings: list[str] = []
    threads, unavail = harvest.fetch_review_thread_state(
        9, repo=None, gh_runner=lambda a: (0, "{}", ""), warnings=warnings
    )
    assert threads == [] and unavail is True
    assert any("owner/repo" in w for w in warnings)


# --- skipped-table parse + map (AC2-FR cross-check) -----------------------

_SKIPPED_REPLY = """## Code Review Response

Thanks @gemini for the review!

### Implemented

| Reviewer | File | Issue | Fix |
|---|---|---|---|
| Gemini | `src/a.py` | HIGH: null check | added guard |

### Skipped

| Reviewer | File | Issue | Reason |
|---|---|---|---|
| Gemini | `src/b.py` | MEDIUM: rename var | out of scope |
| Codex | `src/c.py` | P3: nit | cosmetic |
"""


def test_parse_skipped_table_reads_rows():
    rows = harvest.parse_skipped_table(_SKIPPED_REPLY)
    assert len(rows) == 2
    assert rows[0]["file"] == "src/b.py"  # backticks stripped
    assert rows[0]["reviewer"] == "Gemini"
    assert "rename var" in rows[0]["issue"]
    assert rows[1]["file"] == "src/c.py"


def test_parse_skipped_table_no_section():
    assert harvest.parse_skipped_table("just a plain comment, no table") == []


def test_fetch_skipped_rows_finds_reply():
    issue_comments = _json.dumps([[{"body": _SKIPPED_REPLY}, {"body": "lgtm"}]])

    def gh(args):
        assert "issues" in args[1]
        return 0, issue_comments, ""

    rows, unavail = harvest.fetch_skipped_rows(5, repo="o/r", gh_runner=gh)
    assert unavail is False
    assert {r["file"] for r in rows} == {"src/b.py", "src/c.py"}


def test_fetch_skipped_rows_no_table_is_not_error():
    """A PR with no skipped table is the common case, NOT a gh failure."""
    rows, unavail = harvest.fetch_skipped_rows(
        5, repo="o/r", gh_runner=lambda a: (0, "[[{\"body\": \"lgtm\"}]]", "")
    )
    assert rows == [] and unavail is False


def test_fetch_skipped_rows_gh_failure_marks_unavailable():
    warnings: list[str] = []
    rows, unavail = harvest.fetch_skipped_rows(
        5, repo="o/r", gh_runner=lambda a: (1, "", "boom"), warnings=warnings
    )
    assert rows == [] and unavail is True
    assert any("issue-comments" in w for w in warnings)


def test_skipped_ids_from_rows_maps_by_path():
    threads = [
        {"is_resolved": True, "path": "src/b.py", "comment_ids": ["50"], "body": "x"},
        {"is_resolved": False, "path": "src/z.py", "comment_ids": ["99"], "body": "y"},
    ]
    rows = [{"reviewer": "g", "file": "src/b.py", "issue": "MEDIUM: x", "reason": "no"}]
    assert harvest.skipped_ids_from_rows(rows, threads) == {"50"}


def test_fetch_review_thread_state_graphql_error_envelope_is_unavailable():
    """rc=0 with a GraphQL `errors` envelope must be treated as unavailable, NOT
    as zero resolved threads (which would re-file implemented findings)."""
    body = _json.dumps({"data": None, "errors": [{"message": "FORBIDDEN"}]})
    warnings: list[str] = []
    threads, unavail = harvest.fetch_review_thread_state(
        9, repo="o/r", gh_runner=lambda a: (0, body, ""), warnings=warnings
    )
    assert threads == [] and unavail is True
    assert any("GraphQL errors" in w for w in warnings)


def test_fetch_review_thread_state_missing_pull_request_is_unavailable():
    """rc=0 but no pullRequest object (e.g. partial failure) -> unavailable."""
    body = _json.dumps({"data": {"repository": {"pullRequest": None}}})
    threads, unavail = harvest.fetch_review_thread_state(
        9, repo="o/r", gh_runner=lambda a: (0, body, "")
    )
    assert threads == [] and unavail is True


def test_fetch_review_thread_state_empty_threads_is_available():
    """A valid PR with zero review threads is available (NOT unavailable)."""
    body = _graphql_page([])
    threads, unavail = harvest.fetch_review_thread_state(
        9, repo="o/r", gh_runner=lambda a: (0, body, "")
    )
    assert threads == [] and unavail is False


def test_skipped_ids_from_rows_no_spurious_substring_match():
    """A row naming 'a.py' must NOT match a thread on 'schema.py'."""
    threads = [
        {"is_resolved": True, "path": "src/schema.py", "comment_ids": ["77"]},
    ]
    rows = [{"reviewer": "g", "file": "a.py", "issue": "x", "reason": "y"}]
    assert harvest.skipped_ids_from_rows(rows, threads) == set()


def test_fetch_review_thread_state_paginates_inner_comments():
    """Codex P2: a resolved thread with >100 comments pages the inner comments
    connection so later databaseIds are not dropped from resolved_ids."""
    page1 = _json.dumps({"data": {"repository": {"pullRequest": {"reviewThreads": {
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": [{
            "id": "THREAD1", "isResolved": True, "path": "f.py",
            "comments": {
                "pageInfo": {"hasNextPage": True, "endCursor": "C1"},
                "nodes": [{"databaseId": 1}],
            },
        }],
    }}}}})
    node_page = _json.dumps({"data": {"node": {"comments": {
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": [{"databaseId": 2}],
    }}}})
    calls = []

    def gh(args):
        calls.append(args)
        # First call: reviewThreads query. Second: node() thread-comments page.
        is_node_query = any("node(" in a for a in args)
        return (0, node_page, "") if is_node_query else (0, page1, "")

    threads, unavail = harvest.fetch_review_thread_state(9, repo="o/r", gh_runner=gh)
    assert unavail is False
    # Both the first-page id and the paged id are present.
    assert harvest.resolved_ids_from_threads(threads) == {"1", "2"}
    assert any("THREAD1" in str(a) for a in calls)  # the node() follow-up ran


def test_fetch_review_thread_state_inner_pagination_failure_retains():
    """A gh failure while paging inner comments -> unavailable (retain)."""
    page1 = _json.dumps({"data": {"repository": {"pullRequest": {"reviewThreads": {
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": [{
            "id": "T1", "isResolved": True, "path": "f.py",
            "comments": {"pageInfo": {"hasNextPage": True, "endCursor": "C1"},
                         "nodes": [{"databaseId": 1}]},
        }],
    }}}}})

    def gh(args):
        if any("node(" in a for a in args):
            return 1, "", "HTTP 500"        # inner page fails
        return 0, page1, ""

    threads, unavail = harvest.fetch_review_thread_state(9, repo="o/r", gh_runner=gh)
    assert threads == [] and unavail is True


def test_fetch_review_thread_state_tolerates_null_node():
    """Gemini MEDIUM: a null element in nodes[] must not crash the loop."""
    body = _json.dumps({"data": {"repository": {"pullRequest": {"reviewThreads": {
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": [None, {"isResolved": True, "path": "f", "comments": {
            "pageInfo": {"hasNextPage": False}, "nodes": [None, {"databaseId": 5}]}}],
    }}}}})
    threads, unavail = harvest.fetch_review_thread_state(
        9, repo="o/r", gh_runner=lambda a: (0, body, "")
    )
    assert unavail is False
    assert harvest.resolved_ids_from_threads(threads) == {"5"}


# --- NEW: addressed_ids_from_comments (loop-check parity) -------------------
#
# These tests cover the signal-based addressed detection: a finding is
# ADDRESSED iff its thread has a non-bot reply AND (a commit landed after the
# finding's timestamp OR a reply body contains "wontfix:").

def test_addressed_ids_from_comments_bot_finding_nonbot_reply_commit_after():
    """(i) Bot finding + non-bot reply + commit-after -> addressed."""
    comments = [
        {
            "id": "10",
            "body": "![high] null check missing",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-01T10:00:00Z",
        },
        {
            "id": "11",
            "body": "Fixed in abc123",
            "reviewer": "authorlogin",
            "is_bot": False,
            "in_reply_to_id": "10",
            "created_at": "2024-01-01T11:00:00Z",
        },
    ]
    # A commit landed after the finding's timestamp
    commit_dates = ["2024-01-01T12:00:00+00:00"]
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == {"10"}


def test_addressed_ids_from_comments_nonbot_reply_no_commit_after_not_addressed():
    """(ii) Non-bot reply but NO commit-after, no wontfix -> NOT addressed."""
    comments = [
        {
            "id": "20",
            "body": "![high] something",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-02T10:00:00Z",
        },
        {
            "id": "21",
            "body": "I'll look into this",
            "reviewer": "author",
            "is_bot": False,
            "in_reply_to_id": "20",
            "created_at": "2024-01-02T11:00:00Z",
        },
    ]
    # Commit is BEFORE the finding -> not after
    commit_dates = ["2024-01-01T09:00:00+00:00"]
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == set()


def test_addressed_ids_from_comments_bot_finding_no_human_reply_commit_after_addressed():
    """(iii) A BOT finding with only a bot reply (no human engagement) but a
    commit-after IS addressed: bot findings skip the reply gate (x-632c)."""
    comments = [
        {
            "id": "30",
            "body": "![medium] suggestion",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-03T10:00:00Z",
        },
        {
            "id": "31",
            "body": "I agree with this",
            "reviewer": "other-bot[bot]",
            "is_bot": True,
            "in_reply_to_id": "30",
            "created_at": "2024-01-03T11:00:00Z",
        },
    ]
    commit_dates = ["2024-01-04T10:00:00+00:00"]
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == {"30"}


def test_addressed_ids_from_comments_bot_finding_no_reply_commit_after_addressed():
    """(iv) The x-632c case: a bot finding, no reply at all, but a commit landed
    after -> addressed. This is the gap that filed already-fixed findings."""
    comments = [
        {
            "id": "40",
            "body": "![high] major issue",
            "reviewer": "chatgpt-codex-connector[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-04T10:00:00Z",
        },
    ]
    commit_dates = ["2024-01-05T10:00:00+00:00"]
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == {"40"}


def test_addressed_ids_from_comments_bot_finding_no_commit_after_still_files():
    """A bot finding with NO subsequent commit is shipped-as-is -> not addressed
    (so a genuinely-declined finding still becomes a node)."""
    comments = [
        {
            "id": "45",
            "body": "![high] shipped as-is",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-05T10:00:00Z",
        },
    ]
    commit_dates = ["2024-01-04T10:00:00+00:00"]  # commit BEFORE the finding
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == set()


def test_addressed_ids_from_comments_human_finding_no_reply_commit_after_not_addressed():
    """A HUMAN reviewer's finding keeps the strict gate: a commit-after WITHOUT a
    non-bot reply does not bury it (only bot findings skip the reply gate)."""
    comments = [
        {
            "id": "40h",
            "body": "please guard this",
            "reviewer": "a-human-reviewer",
            "is_bot": False,
            "in_reply_to_id": None,
            "created_at": "2024-01-04T10:00:00Z",
        },
    ]
    commit_dates = ["2024-01-05T10:00:00+00:00"]
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == set()


def test_addressed_ids_from_comments_wontfix_no_commit_after():
    """(v) Non-bot reply with 'wontfix:' body and no commit-after -> addressed."""
    comments = [
        {
            "id": "50",
            "body": "![low] minor nit",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-05T10:00:00Z",
        },
        {
            "id": "51",
            "body": "wontfix: this is by design",
            "reviewer": "author",
            "is_bot": False,
            "in_reply_to_id": "50",
            "created_at": "2024-01-05T11:00:00Z",
        },
    ]
    # Commit is before the finding
    commit_dates = ["2024-01-04T10:00:00+00:00"]
    result = harvest.addressed_ids_from_comments(comments, commit_dates)
    assert result == {"50"}


def test_addressed_ids_from_comments_empty_inputs():
    """Tolerates empty comments list and empty commit_dates."""
    assert harvest.addressed_ids_from_comments([], []) == set()
    assert harvest.addressed_ids_from_comments([], ["2024-01-01T10:00:00Z"]) == set()


def test_addressed_ids_from_comments_missing_keys_tolerant():
    """Tolerates comments with missing is_bot / in_reply_to_id / created_at keys."""
    # A top-level comment (no in_reply_to_id) with missing created_at -> commit_after = False
    comments = [
        {"id": "60", "body": "![high] x", "reviewer": "bot[bot]"},
        {"id": "61", "body": "Fixed", "reviewer": "human", "in_reply_to_id": "60"},
    ]
    # No commit dates -> commit_after always False, no wontfix -> not addressed
    result = harvest.addressed_ids_from_comments(comments, [])
    assert result == set()


# --- NEW: fetch_review_comments enriched fields ----------------------------

def test_fetch_review_comments_parses_in_reply_to_id_and_created_at():
    """fetch_review_comments now captures in_reply_to_id and created_at."""
    payload = _json.dumps([[
        {
            "id": 100,
            "body": "top level comment",
            "html_url": "http://x/1",
            "user": {"login": "gemini[bot]", "type": "Bot"},
            "created_at": "2024-01-01T10:00:00Z",
        },
        {
            "id": 101,
            "body": "reply to 100",
            "html_url": "http://x/2",
            "user": {"login": "humanuser", "type": "User"},
            "created_at": "2024-01-01T11:00:00Z",
            "in_reply_to_id": 100,
        },
    ]])

    def gh(args):
        return 0, payload, ""

    comments = harvest.fetch_review_comments(5, gh_runner=gh)
    assert len(comments) == 2

    top = next(c for c in comments if c["id"] == "100")
    assert top["in_reply_to_id"] is None
    assert top["created_at"] == "2024-01-01T10:00:00Z"
    assert top["is_bot"] is True  # user.type == "Bot"

    reply = next(c for c in comments if c["id"] == "101")
    assert reply["in_reply_to_id"] == "100"
    assert reply["is_bot"] is False  # type == "User"


def test_fetch_review_comments_is_bot_from_login_suffix():
    """A login ending in '[bot]' is recognized as a bot even if type != 'Bot'."""
    payload = _json.dumps([[
        {
            "id": 200,
            "body": "finding",
            "user": {"login": "codex[bot]", "type": "User"},  # type mismatch, login suffix wins
            "created_at": "2024-01-01T10:00:00Z",
        },
    ]])

    def gh(args):
        return 0, payload, ""

    comments = harvest.fetch_review_comments(6, gh_runner=gh)
    assert comments[0]["is_bot"] is True


# --- NEW: fetch_pr_commit_dates -------------------------------------------

def test_fetch_pr_commit_dates_parses_committer_date():
    """fetch_pr_commit_dates prefers commit.committer.date."""
    payload = _json.dumps([[
        {
            "sha": "abc",
            "commit": {
                "committer": {"date": "2024-01-10T12:00:00Z"},
                "author": {"date": "2024-01-10T11:00:00Z"},
            },
        },
        {
            "sha": "def",
            "commit": {
                "committer": {"date": "2024-01-11T12:00:00Z"},
                "author": {"date": "2024-01-11T11:00:00Z"},
            },
        },
    ]])

    def gh(args):
        return 0, payload, ""

    dates, unavail = harvest.fetch_pr_commit_dates(10, gh_runner=gh)
    assert unavail is False
    assert dates == ["2024-01-10T12:00:00Z", "2024-01-11T12:00:00Z"]


def test_fetch_pr_commit_dates_fallback_to_author_date():
    """Falls back to commit.author.date when committer.date is absent."""
    payload = _json.dumps([[
        {
            "sha": "abc",
            "commit": {
                "author": {"date": "2024-01-10T11:00:00Z"},
            },
        },
    ]])

    def gh(args):
        return 0, payload, ""

    dates, unavail = harvest.fetch_pr_commit_dates(11, gh_runner=gh)
    assert unavail is False
    assert dates == ["2024-01-10T11:00:00Z"]


def test_fetch_pr_commit_dates_gh_failure_returns_empty_unavailable():
    """On gh rc!=0, returns ([], True) and appends a WARN."""
    warnings: list[str] = []
    dates, unavail = harvest.fetch_pr_commit_dates(
        12, gh_runner=lambda a: (1, "", "HTTP 403"), warnings=warnings
    )
    assert dates == []
    assert unavail is True
    assert any("commit" in w.lower() for w in warnings)


# --- NEW: fetch_pr_author --------------------------------------------------

def test_fetch_pr_author_returns_login():
    """fetch_pr_author returns the PR author's login string on success."""
    def gh(args):
        assert "pulls" in args[1]
        return 0, "octocat\n", ""

    login = harvest.fetch_pr_author(42, gh_runner=gh)
    assert login == "octocat"


def test_fetch_pr_author_gh_failure_returns_none():
    """On gh failure, returns None and appends a WARN (best-effort)."""
    warnings: list[str] = []
    login = harvest.fetch_pr_author(
        43, gh_runner=lambda a: (1, "", "not found"), warnings=warnings
    )
    assert login is None
    assert len(warnings) == 1


def test_fetch_pr_author_empty_output_returns_none():
    """Empty output -> None (no WARN since rc==0 but blank)."""
    login = harvest.fetch_pr_author(44, gh_runner=lambda a: (0, "  \n", ""))
    assert login is None


# --- NEW: harvest_reviews with author_login filter -------------------------

def test_harvest_reviews_skips_pr_author_comments():
    """A comment whose reviewer == author_login is skipped (not a finding)."""
    comments = [
        {"id": "1", "body": "![high] bot finding", "reviewer": "gemini[bot]"},
        {"id": "2", "body": "Fixed in abc123", "reviewer": "PR_Author"},
        {"id": "3", "body": "![medium] another finding", "reviewer": "codex[bot]"},
    ]
    items = harvest.harvest_reviews(
        comments=comments, author_login="PR_Author", source_pr=99
    )
    ids = {i.source_id for i in items}
    assert "2" not in ids  # author's own reply excluded
    assert "1" in ids and "3" in ids


def test_harvest_reviews_author_login_case_insensitive():
    """Author login comparison is case-insensitive (GitHub logins are case-insensitive)."""
    comments = [
        {"id": "1", "body": "my comment", "reviewer": "MyUser"},
        {"id": "2", "body": "![high] issue", "reviewer": "gemini[bot]"},
    ]
    # author_login in lowercase, reviewer in mixed case
    items = harvest.harvest_reviews(
        comments=comments, author_login="myuser", source_pr=5
    )
    ids = {i.source_id for i in items}
    assert "1" not in ids  # excluded despite case difference
    assert "2" in ids


def test_harvest_reviews_no_author_login_keeps_all():
    """When author_login is None, no filtering by author occurs."""
    comments = [
        {"id": "1", "body": "a comment", "reviewer": "anyone"},
    ]
    items = harvest.harvest_reviews(comments=comments, source_pr=5)
    assert len(items) == 1


# --- NEW: harvest_reviews with commit_dates (addressed suppression) --------

def test_harvest_reviews_skips_addressed_bot_finding():
    """An addressed bot finding (non-bot reply + commit-after) is skipped."""
    comments = [
        {
            "id": "100",
            "body": "![high] null check missing",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-01T10:00:00Z",
        },
        {
            "id": "101",
            "body": "Fixed in abc",
            "reviewer": "author",
            "is_bot": False,
            "in_reply_to_id": "100",
            "created_at": "2024-01-01T11:00:00Z",
        },
    ]
    # resolved_ids=set() means it's not thread-resolved; we rely on addressed signal
    commit_dates = ["2024-01-01T12:00:00+00:00"]
    items = harvest.harvest_reviews(
        comments=comments,
        resolved_ids=set(),
        commit_dates=commit_dates,
        source_pr=10,
    )
    ids = {i.source_id for i in items}
    assert "100" not in ids  # addressed -> suppressed
    assert "101" not in ids  # reply, also suppressed if it were a top-level (it's not found separately)


def test_harvest_reviews_still_open_finding_survives():
    """A still-open bot finding - flagged with NO subsequent commit (shipped
    as-is) - is NOT suppressed. (A commit-after would now mark it addressed;
    see test_addressed_ids_from_comments_bot_finding_no_reply_commit_after.)"""
    comments = [
        {
            "id": "200",
            "body": "![high] open issue",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-02T10:00:00Z",
        },
    ]
    commit_dates = ["2024-01-01T10:00:00+00:00"]  # commit BEFORE -> genuinely open
    items = harvest.harvest_reviews(
        comments=comments,
        resolved_ids=set(),
        commit_dates=commit_dates,
        source_pr=11,
    )
    assert any(i.source_id == "200" for i in items)


# --- NEW: triage_pr end-to-end with injected comments + commit_dates + author_login ---

def test_triage_pr_with_commit_dates_and_author_login(tmp_path: Path):
    """End-to-end: addressed bot finding + author reply -> zero review candidates;
    genuine unaddressed finding lands."""
    from fno.retro.routine import triage_pr

    # Three comments:
    # - id=1: bot finding WITH a non-bot reply + commit-after -> addressed -> suppressed
    # - id=2: author's own reply to finding 1 -> author filter suppresses it
    # - id=3: genuine unaddressed finding -> lands
    comments = [
        {
            "id": "1",
            "body": "![high] addressed gemini finding",
            "reviewer": "gemini[bot]",
            "is_bot": True,
            "in_reply_to_id": None,
            "created_at": "2024-01-01T10:00:00Z",
        },
        {
            "id": "2",
            "body": "Fixed in commit abc",
            "reviewer": "PR_Author",
            "is_bot": False,
            "in_reply_to_id": "1",
            "created_at": "2024-01-01T11:00:00Z",
        },
        {
            "id": "3",
            "body": "![medium] still open finding",
            "reviewer": "codex[bot]",
            "is_bot": False,
            "in_reply_to_id": None,
            "created_at": "2024-01-01T10:00:00Z",
        },
    ]
    commit_dates = ["2024-01-01T12:00:00+00:00"]  # after the finding timestamp

    landed = []

    def fake_create(*, title, details, priority, project, cwd, domain="code", queued=False):
        # Capture the title so we can identify which candidate landed.
        landed.append(title)
        return "fake-id"

    report = triage_pr(
        repo_root=tmp_path,
        pr_number=472,
        mode="queued",
        comments=comments,
        resolved_ids=set(),
        commit_dates=commit_dates,
        author_login="PR_Author",
        create_fn=fake_create,
    )

    # Only the genuine finding (id=3) should produce a landed candidate.
    # fake_create records title strings; we check by the fragment in the body.
    assert any("still open finding" in t for t in landed), "genuine finding must land"
    assert not any("addressed gemini finding" in t for t in landed), "addressed finding must not land"
    # The author reply (id=2) is filtered before classification, so it never lands.
    assert not any("Fixed in commit" in t for t in landed), "author reply must not land"
