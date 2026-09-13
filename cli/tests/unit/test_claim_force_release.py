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

from fno.claims.cli import cli
from fno.claims.core import ForceReleaseOutcome, acquire_claim, force_release_claim
from fno.claims.io import claim_path

runner = CliRunner()


@pytest.fixture(autouse=True)
def _two_default_roots(tmp_path, monkeypatch):
    """Global root and space root both redirected under tmp.

    global_claims_root is patched (not $FNO_CLAIMS_ROOT) because the env
    override also captures claims_dir(None), which would collapse the space
    root into the global one and leave nothing "other" to find.
    """
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setattr("fno.claims.io.global_claims_root", lambda: tmp_path / "global")
    monkeypatch.setattr("fno.paths.space_dir", lambda *_a, **_k: tmp_path / "space")


class TestForceReleaseReportsWhatItFound:
    def test_AC4_HP_existing_file_is_archived_and_named(self, tmp_path):
        space_root = tmp_path / "space"
        acquire_claim("walker:x", "walker:x", root=None)
        resolved = claim_path("walker:x", root=None)
        assert resolved.parent == space_root / "claims"

        outcome = force_release_claim("walker:x", "test override", root=None)
        assert isinstance(outcome, ForceReleaseOutcome)
        assert outcome.archived is True
        assert outcome.path == resolved
        assert outcome.previous_holder == "walker:x"
        assert not resolved.exists()
        # archive_claim derives the .expired root as path.parent.parent.parent
        # (<root>/.fno/claims layout), so search the whole tmp for the archive
        # instead of assuming which claims dir received it.
        archived = [p for p in tmp_path.rglob("walker%3Ax.*.lock")]
        assert len(archived) == 1, "the file must sit under a .expired/ archive"
        assert archived[0].parent.name == ".expired"

        # A fresh claim for the CLI render: the API call above already
        # archived walker:x, and a missing file refuses now.
        acquire_claim("walker:z", "walker:z", root=None)
        r = runner.invoke(cli, ["release", "walker:z", "--force", "--reason", "why"])
        assert r.exit_code == 0
        assert "force-released: walker:z (archived " in r.output

    def test_AC4_HP_json_output_names_archived_and_path(self, tmp_path):
        import json as _json

        acquire_claim("walker:y", "walker:y", root=None)
        r = runner.invoke(
            cli, ["release", "walker:y", "--force", "--reason", "why", "--json"]
        )
        assert r.exit_code == 0
        payload = _json.loads(r.output)
        assert payload["archived"] is True
        assert payload["path"] == str(claim_path("walker:y", root=None))

    def test_AC4_ERR_file_only_in_the_other_root_is_a_refusal(self, tmp_path):
        # session:abc routes to the GLOBAL root; the encoded file is written
        # into the SPACE root only, so the resolved path reads empty. The
        # file is handwritten: the refusal path checks existence, never
        # parses, and the byte-identity assertion needs the raw bytes.
        global_root = tmp_path / "global"
        space_claims = tmp_path / "space" / "claims"
        space_claims.mkdir(parents=True)
        stray = space_claims / "session%3Aabc.lock"
        stray.write_bytes(b"holder: target-session:someone\n")
        before = stray.read_bytes()

        r = runner.invoke(cli, ["release", "session:abc", "--force", "--reason", "why"])
        assert r.exit_code == 1
        assert f"nothing released: no claim file at {claim_path('session:abc', root=global_root)}" in r.output
        assert str(stray) in r.output, "the other root's file must be named"
        assert stray.read_bytes() == before, "the other root's file must be untouched"

    def test_missing_file_names_the_path_it_read(self, tmp_path):
        r = runner.invoke(cli, ["release", "walker:gone", "--force", "--reason", "why"])
        assert r.exit_code == 1
        assert (
            f"nothing released: no claim file at {claim_path('walker:gone', root=None)}"
            in r.output
        )
