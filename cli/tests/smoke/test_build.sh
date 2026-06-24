#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet
uv build --quiet
ls dist/*.whl >/dev/null
TMPDIR_INSTALL=$(mktemp -d)
trap "rm -rf $TMPDIR_INSTALL" EXIT
uv venv --quiet "$TMPDIR_INSTALL/venv"
uv pip install --quiet --python "$TMPDIR_INSTALL/venv/bin/python" dist/*.whl
out=$("$TMPDIR_INSTALL/venv/bin/fno" --version 2>&1 || true)
# version-agnostic: the binary must report SOME semver, not a pinned one, so a
# release bump never breaks this smoke test (the build wires the version).
echo "$out" | grep -qE '[0-9]+\.[0-9]+\.[0-9]+'
echo "PASS: uv build produces installable wheel"

# ab-fe825805 change 3: the events schema must SHIP inside the wheel as
# `_schema.yaml` (force-included from docs/architecture/events-schema.yaml),
# so the Python validator's in-package fallback resolves from an installed
# artifact with no `docs/` tree and no env var. Regression: a clean build
# that ships no schema makes `import fno.events` raise from a foreign cwd.
PKG_SCHEMA=$("$TMPDIR_INSTALL/venv/bin/python" -c \
  "import fno.events as e, os; print(os.path.join(os.path.dirname(e.__file__), '_schema.yaml'))")
test -f "$PKG_SCHEMA" || { echo "FAIL: wheel did not ship fno/events/_schema.yaml"; exit 1; }
echo "PASS: wheel ships in-package _schema.yaml"

# ab-18563bcc US5: the wheel must carry LICENSE + NOTICE (Apache-2.0 is declared
# in pyproject.toml, but the texts must ship in the distribution). The build
# hook force-includes them under fno/_licenses/ from the repo root (direct
# build) or the sdist vendor copy (wheel-from-sdist). Regression: a build that
# ships no license text is non-compliant.
PKG_LIC_DIR=$("$TMPDIR_INSTALL/venv/bin/python" -c \
  "import fno, os; print(os.path.join(os.path.dirname(fno.__file__), '_licenses'))")
for f in LICENSE NOTICE; do
  test -f "$PKG_LIC_DIR/$f" || { echo "FAIL: wheel did not ship fno/_licenses/$f"; exit 1; }
done
echo "PASS: wheel ships LICENSE + NOTICE under fno/_licenses/"

# Import + schema-load from an EMPTY cwd with every schema/project env var
# unset - exercises ONLY the in-package fallback, not a dev-tree walk-up.
# Wrapped in `if (...)` rather than a bare subshell + `rc=$?`: under `set -e`
# a failing standalone subshell aborts the script before the capture, so the
# custom FAIL diagnostic would never print on a regression.
EMPTY_CWD=$(mktemp -d)
if ( cd "$EMPTY_CWD"
     env -u FNO_REPO_ROOT -u CLAUDE_PLUGIN_ROOT -u EVENTS_SCHEMA_PATH \
       "$TMPDIR_INSTALL/venv/bin/python" -c \
       "import fno.events as e; assert e.SCHEMA is not None, e._schema_load_error; assert e.SCHEMA.get('event_types'), 'empty schema'" ); then
  rm -rf "$EMPTY_CWD"
  echo "PASS: fno.events loads in-package schema from empty cwd"
else
  rm -rf "$EMPTY_CWD"
  echo "FAIL: import fno.events did not load schema from empty cwd"
  exit 1
fi
