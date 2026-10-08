"""AC4 (x-9c91): force-release reports what it found at the path.

The 2026-09-09 x-cff2 specimen: `claim release <key> --force` printed
`force-released` while the real lock file stayed byte-identical - the key
resolved to one default root and the file lived in the other. A missing
file is a refusal now, naming the path that was read and, when the
encoded file exists in the other default root, that path.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner


runner = CliRunner()


@pytest.fixture(autouse=True)
def _two_default_roots(tmp_path, monkeypatch):
    """Global root and space root both redirected under tmp.

    Both legs must see the same roots, and the native leg only reads env
    (a Python path-symbol patch is invisible across the seam): HOME carries
    the global root and FNO_SPACES_DIR the space base. FNO_CLAIMS_ROOT stays
    unset - it would collapse claims_dir(None) into the global root and leave
    nothing "other" to find.
    """
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path / "global"))
    monkeypatch.setenv("FNO_SPACES_DIR", str(tmp_path / "spaces"))



def test_AC4_HP_existing_claim_is_archived_and_named():
    from fno.claims.cli import cli
    from fno.claims.core import acquire_claim, claim_status, force_release_claim

    acquire_claim("walker:x", "walker:x", root=None)
    outcome = force_release_claim("walker:x", "test override", root=None)
    assert outcome.archived is True
    assert outcome.previous_holder == "walker:x"
    assert claim_status("walker:x")["state"] == "free"

    acquire_claim("walker:z", "walker:z", root=None)
    r = runner.invoke(cli, ["release", "walker:z", "--force", "--reason", "why"])
    assert r.exit_code == 0, r.output
    assert "force-released: walker:z (archived " in r.output


def test_AC4_HP_json_output_names_archived_and_path():
    import json

    from fno.claims.cli import cli
    from fno.claims.core import acquire_claim

    acquire_claim("walker:y", "walker:y", root=None)
    r = runner.invoke(cli, ["release", "walker:y", "--force", "--reason", "why", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["archived"] is True
    assert payload["path"]


def test_AC4_ERR_file_only_in_the_other_root_is_a_refusal():
    """session: routes to the global root; a legacy claim file in the space
    root reads empty there, so the release refuses, names that file and
    leaves it byte-identical."""
    from fno.claims.cli import cli
    from fno.claims.io import claims_dir

    space_claims = claims_dir(None)
    space_claims.mkdir(parents=True)
    stray = space_claims / "session%3Aabc.lock"
    stray.write_bytes(b"holder: target-session:someone\n")
    before = stray.read_bytes()

    r = runner.invoke(cli, ["release", "session:abc", "--force", "--reason", "why"])
    assert r.exit_code == 1, r.output
    assert "nothing released: no claim file at " in r.output
    assert str(stray) in r.output, "the other root's file must be named"
    assert stray.read_bytes() == before, "the other root's file must be untouched"


def test_missing_claim_names_the_path_it_read():
    from fno.claims.cli import cli

    r = runner.invoke(cli, ["release", "walker:gone", "--force", "--reason", "why"])
    assert r.exit_code == 1
    assert "nothing released: no claim file at " in r.output
