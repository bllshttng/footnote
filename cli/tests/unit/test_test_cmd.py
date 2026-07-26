"""Unit tests for fno.test_cmd smoke discovery.

discover_shell_harnesses() walks the owned shell-harness trees and returns the
standalone harnesses, so a new tests/hooks/test_x.sh runs with zero registry
edits (the two-test-trees lesson: a green subset is not proof everything ran).
It excludes libraries (scripts/lib/), the orchestrated cli/tests/smoke/
subtree, files without a shell shebang, and source-only helpers.
"""
from __future__ import annotations

from pathlib import Path

from fno.test_cmd import discover_shell_harnesses


def _write(path: Path, body: str, *, executable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    if executable:
        path.chmod(0o755)


def test_discovers_new_harness_and_excludes_non_harnesses(tmp_path: Path) -> None:
    root = tmp_path

    # AC4-EDGE: a brand-new harness dropped into an owned tree is discovered
    # with no registry edit (the consolidation's core win).
    _write(root / "tests/zzz-probe.sh", "#!/usr/bin/env bash\necho probe\n", executable=True)
    _write(root / "tests/hooks/test_new_probe.sh",
           "#!/usr/bin/env bash\necho hi\n", executable=True)
    # A harness in scripts/tests/ and one under skills/*/tests/.
    _write(root / "scripts/tests/test_one.sh",
           "#!/usr/bin/env bash\necho one\n", executable=True)
    _write(root / "skills/agent/tests/test_one.sh",
           "#!/usr/bin/env bash\necho skill\n", executable=True)

    # A source-only helper: a sibling sources it -> helper, not a harness.
    _write(root / "tests/hooks/test_helper.sh",
           "#!/usr/bin/env bash\n# sourced by a sibling, never standalone\n",
           executable=True)
    _write(root / "tests/hooks/test_uses_helper.sh",
           "#!/usr/bin/env bash\nsource ./test_helper.sh\necho ok\n", executable=True)
    # A helper sourced via the conventional variable form ($SCRIPT_DIR/...):
    # the resolver falls back to the basename under the sourcer's dir.
    _write(root / "tests/hooks/test_var_helper.sh",
           "#!/usr/bin/env bash\n# sourced via $SCRIPT_DIR\n", executable=True)
    _write(root / "tests/hooks/test_uses_var_helper.sh",
           '#!/usr/bin/env bash\nSCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"\n'
           'source "$SCRIPT_DIR/test_var_helper.sh"\necho ok\n', executable=True)

    # Excluded classes.
    _write(root / "scripts/lib/libthing.sh",       # library, out of owned scope
           "#!/usr/bin/env bash\necho lib\n", executable=True)
    _write(root / "cli/tests/smoke/test_smoke_one.sh",  # orchestrated subtree
           "#!/usr/bin/env bash\necho smoke\n", executable=True)
    _write(root / "tests/hooks/not_shell.sh",       # no shell shebang
           "#!/usr/bin/env python3\nprint('no')\n", executable=True)

    found = set(discover_shell_harnesses(root))

    assert "tests/zzz-probe.sh" in found                  # AC4-EDGE
    assert "tests/hooks/test_new_probe.sh" in found
    assert "scripts/tests/test_one.sh" in found
    assert "skills/agent/tests/test_one.sh" in found
    assert "tests/hooks/test_uses_helper.sh" in found     # real harness that sources a helper
    assert "tests/hooks/test_helper.sh" not in found      # source-only helper excluded
    assert "tests/hooks/test_uses_var_helper.sh" in found  # real harness, variable source ref
    assert "tests/hooks/test_var_helper.sh" not in found   # $SCRIPT_DIR-sourced helper excluded
    assert "scripts/lib/libthing.sh" not in found         # library excluded
    assert "cli/tests/smoke/test_smoke_one.sh" not in found  # orchestrated subtree excluded
    assert "tests/hooks/not_shell.sh" not in found        # non-shell shebang excluded
