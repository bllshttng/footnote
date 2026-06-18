"""Hatchling build hook: bundle the three ``fno-agents`` Rust binaries into the wheel.

Phase 6 W6 Wave 3; binary-complete in ab-18563bcc (US6). When the compiled
binaries are staged under ``src/fno/_bin/`` (placed there by the release CI's
cargo step, or via a dir override in ``FNO_AGENTS_BIN_DIR``), this hook ships
each as a wheel *script* so ``pip install fno`` lands ``fno-agents``,
``fno-agents-daemon``, and ``fno-agents-worker`` on PATH, and marks the wheel
platform-specific. Staging is all-or-nothing: a partial set (some but not all
three) hard-fails the build (a release wheel must be binary-complete).

When no binary is staged (ordinary source builds, ``sdist``, ``pip install -e``
during development) the hook is a no-op and the pure-Python wheel builds exactly
as before. That graceful degradation is the load-bearing property: the Python
package must never *require* a Rust toolchain to build.
"""
from __future__ import annotations

import os
from collections.abc import Sequence
from pathlib import Path

try:
    from hatchling.builders.hooks.plugin.interface import BuildHookInterface
except ImportError:  # hatchling is the build backend, absent when unit-testing
    BuildHookInterface = object  # type: ignore[assignment,misc]  # the pure helpers below stay importable

#: The three standalone Rust executables a release wheel ships (US6): the client
#: (`fno agents` execs it), the daemon, and the worker. Staging only the client
#: left daemon-backed verbs broken on a pip-only box (the hidden cliff US6
#: closes). They are standalone executables, NOT CPython extensions, so the
#: wheel tag stays `py3-none-<platform>` (see apply_binary_bundle).
_BINARY_BASENAMES = ("fno-agents", "fno-agents-daemon", "fno-agents-worker")
#: Windows appends .exe; os.name reflects the BUILD machine, which is the correct
#: platform for the wheel being produced, so the hook probes the right names on
#: the windows runner (else it no-ops and silently ships a pure-Python wheel).
_EXE = ".exe" if os.name == "nt" else ""
BINARY_NAMES = tuple(f"{base}{_EXE}" for base in _BINARY_BASENAMES)
#: Directory the release CI's cargo step stages the binaries into, relative to
#: the `cli/` build root. Overridable via FNO_AGENTS_BIN_DIR for local/test
#: builds that want to point the hook at an arbitrary staging dir.
BIN_DIR_ENV = "FNO_AGENTS_BIN_DIR"
DEFAULT_BIN_REL_DIR = ("src", "fno", "_bin")


#: CI override for the wheel platform tag. Needed on Linux: PyPI rejects plain
#: ``linux_x86_64`` tags (only ``manylinux*`` is accepted), and auditwheel cannot
#: repair a wheel whose only native payload is a standalone executable rather
#: than a ``.so``. The release workflow builds the binary in a manylinux/musl
#: context and sets this to e.g. ``manylinux_2_17_x86_64``.
PLATFORM_ENV = "FNO_AGENTS_WHEEL_PLATFORM"


def _platform_tag() -> str | None:
    """Wheel platform tag to use: the ``FNO_AGENTS_WHEEL_PLATFORM`` override if
    set, else the most-specific tag for the build machine (e.g.
    ``macosx_11_0_arm64``), or ``None`` if unavailable.

    ``packaging`` is a hatchling dependency, so it is importable at build time.
    """
    override = os.environ.get(PLATFORM_ENV, "").strip()
    if override:
        return override
    try:
        from packaging.tags import platform_tags
    except ImportError:  # pragma: no cover - packaging is a hatchling dep at build time
        # Only the import is tolerated as a soft failure (falls back to
        # infer_tag). A real defect inside platform_tags() must surface as a
        # build error, not silently mis-tag the wheel.
        return None
    return next(iter(platform_tags()), None)


def _bin_dir(root: Path) -> Path:
    """Directory the release binaries are staged in: the ``FNO_AGENTS_BIN_DIR``
    override if set, else ``<root>/src/fno/_bin``."""
    override = os.environ.get(BIN_DIR_ENV, "").strip()
    return Path(override) if override else Path(root, *DEFAULT_BIN_REL_DIR)


def staged_binaries(root: Path) -> tuple[list[Path], list[str]]:
    """Probe the staging dir for the three release binaries.

    Returns ``(present, missing)``: ``present`` is the existing binary paths in
    canonical order, ``missing`` the basenames not staged. A pure filesystem
    probe - the bundle / no-op / hard-fail policy lives in
    ``resolve_binary_bundle`` so it stays unit-testable.
    """
    bin_dir = _bin_dir(root)
    present: list[Path] = []
    missing: list[str] = []
    for name in BINARY_NAMES:
        candidate = bin_dir / name
        if candidate.is_file():
            present.append(candidate)
        else:
            missing.append(name)
    return present, missing


def resolve_binary_bundle(root: Path) -> list[Path] | None:
    """Decide what to bundle, enforcing the all-or-nothing release contract (US6).

    * zero staged -> ``None``: a pure-Python wheel is a valid variant (ordinary
      source / editable / sdist builds where no cargo step ran). Preserves W6's
      graceful-degradation property (AC6-EDGE).
    * all three staged -> the list of paths to bundle (AC6-HP).
    * a partial set (1-2 of 3) -> raise ``FileNotFoundError``: a release-matrix
      wheel that carries some but not all binaries is a staging defect, never a
      valid variant (AC6-ERR), mirroring the schema hard-fail.
    """
    present, missing = staged_binaries(root)
    if not present:
        return None
    if missing:
        raise FileNotFoundError(
            "binary-complete wheel staging is incomplete: found "
            f"{[p.name for p in present]} but missing {missing} in {_bin_dir(root)}. "
            f"A release wheel must carry all three {list(BINARY_NAMES)} (a partial "
            "set is a staging defect, not a valid variant). Stage every binary, or "
            "stage none for a pure-Python wheel."
        )
    return present


def apply_binary_bundle(binaries: Sequence[Path] | None, build_data: dict) -> dict:
    """Pure decision: bundle each binary in ``binaries`` as a wheel script.

    Returns the same dict for convenience. ``None`` or empty -> pure-Python
    wheel (``build_data`` untouched). Factored out of the hook so the
    bundle-vs-no-op decision and the tag choice are unit-testable without
    constructing a hatchling ``BuildHookInterface``.
    """
    if not binaries:
        # No binaries -> pure-Python wheel. Leave build_data untouched.
        return build_data

    # ``shared_scripts`` files install into the environment's bin/ (Scripts/ on
    # Windows) -> on PATH. Key is the source path; value is the scripts-dir name
    # (``binary.name`` so the `.exe` suffix travels on Windows).
    scripts = build_data.setdefault("shared_scripts", {})
    for binary in binaries:
        scripts[str(binary)] = binary.name

    # The wheel now carries native binaries, so it is no longer universal. They
    # are standalone executables, NOT CPython extensions, so they do not depend
    # on the interpreter version or ABI. Tag it ``py3-none-<platform>`` rather
    # than ``infer_tag`` 's ``cp3XX-cp3XX-<platform>`` so one wheel serves every
    # Python 3.x on the platform (no per-version build explosion, no sdist
    # fallback on newer Pythons). CI wheel-repair (delocate / auditwheel) retags
    # the platform part for manylinux as needed.
    build_data["pure_python"] = False
    plat = _platform_tag()
    if plat:
        build_data["infer_tag"] = False
        build_data["tag"] = f"py3-none-{plat}"
    else:  # pragma: no cover - packaging always present at build time
        build_data["infer_tag"] = True
    return build_data


#: Where the events schema lands INSIDE the wheel. The Python validator's
#: in-package fallback resolves `<events-pkg-dir>/_schema.yaml`, so this dest
#: must match events/__init__.py:_resolve_manifest_path. (ab-fe825805 change 3)
#: A ``str`` (not ``Path``) because hatchling force_include VALUES must be
#: forward-slash distribution paths; ``SCHEMA_REPO_REL`` below is a tuple
#: because it is splatted into ``Path.joinpath``.
SCHEMA_REL_DEST = "fno/events/_schema.yaml"
#: Canonical schema location relative to the REPO root (parent of the `cli/`
#: build root) - the direct source build (`uv tool install`, `uv build --wheel`).
SCHEMA_REPO_REL = ("docs", "architecture", "events-schema.yaml")
#: Copy the sdist vendors at its root (pyproject `sdist.force-include`), used
#: when a wheel is built FROM the sdist (`uv build`), where the repo `docs/`
#: tree is absent.
SCHEMA_SDIST_VENDOR = "_schema_vendor.yaml"


def schema_source(root: Path) -> Path | None:
    """Locate the events schema for the current wheel build, or ``None``.

    Two build modes resolve from different places:
      * direct source build: ``<repo>/docs/architecture/events-schema.yaml``,
        one level above the ``cli/`` build ``root``.
      * wheel-from-sdist (``uv build``): the sdist vendored it at the sdist
        root as ``_schema_vendor.yaml`` (the repo ``docs/`` tree is not in the
        sdist).
    First existing candidate wins.
    """
    for candidate in (
        Path(root).parent.joinpath(*SCHEMA_REPO_REL),
        Path(root) / SCHEMA_SDIST_VENDOR,
    ):
        if candidate.is_file():
            return candidate
    return None


def apply_schema_bundle(schema: Path | None, build_data: dict) -> dict:
    """Pure decision: force-include ``schema`` into the wheel as the in-package
    `_schema.yaml`. No-op when ``schema`` is ``None`` (callers turn that into a
    hard build failure via ``resolve_required_schema`` - a schema-less wheel is
    broken, not a valid variant).

    Uses ``setdefault`` so it composes with a pre-existing ``force_include``
    (e.g. another hook's entry) rather than clobbering it. Factored out for
    unit-testability, mirroring ``apply_binary_bundle``.
    """
    if schema is None:
        return build_data
    build_data.setdefault("force_include", {})[str(schema)] = SCHEMA_REL_DEST
    return build_data


def resolve_required_schema(root: Path) -> Path:
    """``schema_source`` but REQUIRED: raise ``FileNotFoundError`` with an
    actionable message instead of returning ``None``.

    Unlike the binary (optional -> pure-Python wheel is a valid variant), the
    schema MUST ship: without it the installed validator raises from a foreign
    cwd. A miss is a build error, not a degraded mode. Factored out of
    ``CustomBuildHook.initialize`` so the loud-failure contract is unit-testable
    without constructing a hatchling ``BuildHookInterface``.
    """
    schema = schema_source(root)
    if schema is None:
        raise FileNotFoundError(
            "events schema not found for wheel bundling: looked for "
            f"<repo>/{'/'.join(SCHEMA_REPO_REL)} (direct build) and "
            f"<sdist-root>/{SCHEMA_SDIST_VENDOR} (wheel-from-sdist). Without it "
            "the wheel ships no fno/events/_schema.yaml and "
            "`import fno.events` fails from a foreign cwd (ab-fe825805)."
        )
    return schema


# -- ab-18563bcc US5: LICENSE + NOTICE bundling --
#
# The wheel must carry the license texts (Apache-2.0 is declared in
# pyproject.toml, but the files themselves must ship). They live at the REPO
# root, one level above the `cli/` build root, so - exactly like the events
# schema - their source differs between build modes: a direct source build
# finds them at <repo>/LICENSE, but `uv build` does sdist-then-wheel-from-sdist
# where the repo root is absent, so the sdist vendors them (pyproject
# sdist.force-include) under _license_vendor/. Both modes are probed; a miss is
# a HARD build failure (a license-less wheel is non-compliant, not a variant).

#: License files that MUST ship in the wheel.
LICENSE_BASENAMES = ("LICENSE", "NOTICE")
#: Where the vendored copies land in the sdist root (pyproject sdist.force-include);
#: read in the wheel-from-sdist build mode.
LICENSE_SDIST_VENDOR_DIR = "_license_vendor"
#: Directory the license files land in INSIDE the wheel (package-internal, so
#: they are unambiguously present in the distribution from any cwd).
LICENSE_REL_DEST_DIR = "fno/_licenses"


def license_sources(root: Path) -> dict[str, Path]:
    """Locate each license file for the current wheel build.

    Returns ``{basename: Path}`` for every file found, probing the direct-build
    location (``<repo>/<name>``, one level above the ``cli/`` build ``root``)
    then the sdist vendor copy (``<root>/_license_vendor/<name>``). First
    existing candidate wins per file; a file found nowhere is absent from the
    mapping (``resolve_required_licenses`` turns that into a hard failure).
    """
    # hatchling passes an absolute build root, but guard the relative case
    # (`.` / `cli`) where `.parent` would not climb to the repo root. Resolve
    # only when relative so an absolute root (incl. tests) is left byte-identical.
    root_path = Path(root)
    if not root_path.is_absolute():
        root_path = root_path.resolve()
    found: dict[str, Path] = {}
    for name in LICENSE_BASENAMES:
        for candidate in (
            root_path.parent / name,
            root_path / LICENSE_SDIST_VENDOR_DIR / name,
        ):
            if candidate.is_file():
                found[name] = candidate
                break
    return found


def apply_license_bundle(licenses: dict[str, Path] | None, build_data: dict) -> dict:
    """Pure decision: force-include each license file into the wheel under
    ``fno/_licenses/``. No-op on a falsy mapping (callers hard-fail via
    ``resolve_required_licenses`` - a license-less wheel is broken, not a
    variant). Uses ``setdefault`` so it composes with a pre-existing
    ``force_include`` (e.g. the schema entry). Mirrors ``apply_schema_bundle``.
    """
    if not licenses:
        return build_data
    force_include = build_data.setdefault("force_include", {})
    for name, path in licenses.items():
        force_include[str(path)] = f"{LICENSE_REL_DEST_DIR}/{name}"
    return build_data


def resolve_required_licenses(root: Path) -> dict[str, Path]:
    """``license_sources`` but REQUIRED: raise ``FileNotFoundError`` naming the
    missing files rather than silently shipping a non-compliant wheel.

    Mirrors ``resolve_required_schema``: a license-less wheel is a build error,
    not a degraded mode (AC5-ERR).
    """
    found = license_sources(root)
    missing = [name for name in LICENSE_BASENAMES if name not in found]
    if missing:
        raise FileNotFoundError(
            f"license files not found for wheel bundling: {missing}. Looked for "
            "<repo>/<name> (direct build) and "
            f"<sdist-root>/{LICENSE_SDIST_VENDOR_DIR}/<name> (wheel-from-sdist). "
            "The wheel must carry LICENSE + NOTICE (Apache-2.0 is declared in "
            "pyproject.toml but the texts must ship); a license-less wheel is "
            "non-compliant, not a valid variant."
        )
    return found


class CustomBuildHook(BuildHookInterface):
    """Place the prebuilt binaries in the wheel's ``.data/scripts/`` dir if
    present, and force-include the events schema + license texts as in-package
    data."""

    def initialize(self, version: str, build_data: dict) -> None:
        # Binaries are optional (zero -> pure-Python wheel) but all-or-nothing
        # when present: a partial set hard-fails inside resolve_binary_bundle.
        apply_binary_bundle(resolve_binary_bundle(Path(self.root)), build_data)
        # Schema is required (see resolve_required_schema): fail the build loud
        # rather than ship a silently-broken wheel.
        apply_schema_bundle(resolve_required_schema(Path(self.root)), build_data)
        # LICENSE + NOTICE are required (see resolve_required_licenses): a
        # license-less wheel is non-compliant, so fail the build loud (US5).
        apply_license_bundle(resolve_required_licenses(Path(self.root)), build_data)
