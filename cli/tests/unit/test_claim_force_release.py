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

