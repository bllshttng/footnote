"""A ledger row that carries a pr_number must also carry its pr_url.

A url-less row attributes no ownership at read time (fno.ledger_join), and
nothing repairs it afterwards: upsert_ledger_pr returns early once a row has
the number. So the write side has to resolve the slug as hard as the read side
does, including the `gh repo view` fallback for a checkout whose GitHub remote
is not named `origin`.
"""
from __future__ import annotations

import pytest

from fno.cost._register import _pr_url_for

SSH = "git@github.com:bllshttng/footnote.git"
HTTPS = "https://github.com/bllshttng/footnote"


@pytest.mark.parametrize("remote", [SSH, HTTPS, SSH.removesuffix(".git")])
def test_origin_derives_url_without_shelling_out(remote, monkeypatch):
    """The common case stays a pure string parse - no gh, no network."""
    monkeypatch.setattr(
        "fno.graph._reconcile.resolve_current_repo_slug",
        lambda *a, **k: pytest.fail("must not shell out when origin parses"),
    )
    assert _pr_url_for(507, remote, "/repo") == (
        "https://github.com/bllshttng/footnote/pull/507"
    )


@pytest.mark.parametrize("remote", [None, "", "git@gitlab.com:o/r.git"])
def test_unparseable_origin_falls_back_to_slug_resolution(remote, monkeypatch):
    """Missing, renamed, or non-github `origin` still yields a url.

    This is the regression the fallback exists for: `gh repo view` resolves the
    slug for these checkouts, so the row must not land url-less and become
    permanently unattributable.
    """
    monkeypatch.setattr(
        "fno.graph._reconcile.resolve_current_repo_slug", lambda *a, **k: "o/r"
    )
    assert _pr_url_for(507, remote, "/repo") == "https://github.com/o/r/pull/507"


def test_no_slug_anywhere_yields_no_url(monkeypatch):
    """Refusing beats guessing: an unresolvable repo gets no url rather than a
    fabricated one that would attribute the row to the wrong repo."""
    monkeypatch.setattr(
        "fno.graph._reconcile.resolve_current_repo_slug", lambda *a, **k: None
    )
    assert _pr_url_for(507, None, "/repo") is None


def test_no_pr_number_yields_no_url(monkeypatch):
    monkeypatch.setattr(
        "fno.graph._reconcile.resolve_current_repo_slug",
        lambda *a, **k: pytest.fail("must not resolve a slug with no PR"),
    )
    assert _pr_url_for(None, SSH, "/repo") is None
