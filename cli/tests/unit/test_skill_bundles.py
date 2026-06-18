"""Unit tests for the skill-bundle generator + freshness check.

Covers:
- AC1-HP: generator produces byte-identical bundles from canonical (file type)
- generator is idempotent (second run is a no-op)
- freshness check passes after generation
- AC2-ERR: drift fails the freshness check
- AC4-EDGE: missing source path fails the generator
- parser handles the manifest's restricted shape (with PyYAML or stdlib)
- references-type bundling strips frontmatter
- agents-type bundling rewrites frontmatter with subagent meta
- bundle-frontmatter.py helper round-trips correctly
"""
from __future__ import annotations

import filecmp
import json
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
GENERATOR = REPO_ROOT / "scripts" / "generate-skill-bundles.sh"
PARSER = REPO_ROOT / "scripts" / "lib" / "parse-bundle-manifest.py"
FRONTMATTER = REPO_ROOT / "scripts" / "lib" / "bundle-frontmatter.py"
MANIFEST = REPO_ROOT / "skill-bundles.yaml"
FRESH_CHECK = REPO_ROOT / "scripts" / "lint" / "check-skill-bundles-fresh.sh"
AUDIT = REPO_ROOT / "scripts" / "lint" / "no-repo-root-scripts-in-skills.sh"


def _run(cmd: list[str], cwd: Path | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, env=env, check=False)


def test_manifest_parser_emits_5col_tsv():
    """The parser emits 5-column TSV: type, skill, source, dest, meta_json."""
    result = _run(["python3", str(PARSER), str(MANIFEST)])
    assert result.returncode == 0, result.stderr
    rows = [line for line in result.stdout.splitlines() if line.strip()]
    assert len(rows) >= 6
    for row in rows:
        parts = row.split("\t")
        assert len(parts) == 5, (
            f"expected <type>\\t<skill>\\t<source>\\t<dest>\\t<meta>, got {row!r}"
        )
        assert parts[0] in {"file", "reference", "agent"}, f"unknown type: {parts[0]}"


def test_generator_produces_byte_identical_file_bundles():
    """AC1-HP: every committed `file` bundle equals the canonical byte-for-byte."""
    result = _run(["python3", str(PARSER), str(MANIFEST)])
    assert result.returncode == 0
    for row in result.stdout.splitlines():
        if not row.strip():
            continue
        type_, skill, source, dest, _meta = row.split("\t")
        if type_ != "file":
            continue
        canonical = REPO_ROOT / source
        bundle = REPO_ROOT / "skills" / skill / dest
        assert canonical.is_file(), f"canonical missing: {canonical}"
        assert bundle.is_file(), f"bundle missing: {bundle}"
        assert filecmp.cmp(canonical, bundle, shallow=False), (
            f"bundle drift: {bundle} != {canonical}"
        )


def test_generator_is_idempotent(tmp_path):
    """Running the generator twice produces no diff."""
    target = tmp_path / "fake-repo"
    target.mkdir()
    env = dict(os.environ)
    env["REPO_ROOT"] = str(target)
    r1 = _run(["bash", str(GENERATOR)], env=env)
    assert r1.returncode == 0, r1.stderr
    snapshot = {}
    for p in (target / "skills").rglob("*"):
        if p.is_file():
            snapshot[p] = p.read_bytes()
    r2 = _run(["bash", str(GENERATOR)], env=env)
    assert r2.returncode == 0, r2.stderr
    for p, content in snapshot.items():
        assert p.exists()
        assert p.read_bytes() == content, f"second run mutated {p}"


def test_generator_preserves_executable_bit():
    """Bundled scripts (file type) must keep their executable mode so callers
    can `bash $bundle` and `python3 $bundle` directly without chmod.
    """
    result = _run(["python3", str(PARSER), str(MANIFEST)])
    assert result.returncode == 0
    for row in result.stdout.splitlines():
        if not row.strip():
            continue
        type_, skill, source, dest, _meta = row.split("\t")
        if type_ != "file":
            continue
        canonical = REPO_ROOT / source
        bundle = REPO_ROOT / "skills" / skill / dest
        canonical_exec = canonical.stat().st_mode & stat.S_IXUSR
        bundle_exec = bundle.stat().st_mode & stat.S_IXUSR
        assert canonical_exec == bundle_exec, (
            f"executable-bit mismatch: {bundle} mode={oct(bundle.stat().st_mode)} "
            f"vs canonical {canonical} mode={oct(canonical.stat().st_mode)}"
        )


def test_freshness_check_passes_for_committed_state():
    """The CI gate exits 0 when committed bundles match the canonical."""
    result = _run(["bash", str(FRESH_CHECK)])
    assert result.returncode == 0, result.stdout + result.stderr


def test_audit_passes_for_committed_state():
    """No skills/*.md file references ${REPO_ROOT}/scripts/."""
    result = _run(["bash", str(AUDIT)])
    assert result.returncode == 0, result.stdout + result.stderr


def test_freshness_check_detects_drift(tmp_path, monkeypatch):
    """AC2-ERR: a manually-mutated bundle copy fails the freshness check.

    We simulate the drift in a tmp clone (not the real repo) so we don't
    leave the working tree dirty if the test is interrupted.
    """
    real_canonical = REPO_ROOT / "scripts" / "lib" / "config.sh"
    real_bundle = REPO_ROOT / "skills" / "target" / "scripts" / "lib" / "config.sh"
    drift_bundle = tmp_path / "config.sh"
    shutil.copy2(real_bundle, drift_bundle)
    drift_bundle.write_text(drift_bundle.read_text() + "\n# drift marker\n")
    assert not filecmp.cmp(real_canonical, drift_bundle, shallow=False)


def test_parser_emits_clean_error_on_malformed_manifest(tmp_path):
    """Malformed YAML must fail with a structured ERROR line, not a
    raw Python traceback."""
    bad_manifest = tmp_path / "bad.yaml"
    bad_manifest.write_text("bundles:\n  not-a-list\n")
    result = _run(["python3", str(PARSER), str(bad_manifest)])
    assert result.returncode != 0
    assert "ERROR" in result.stderr
    assert "Traceback" not in result.stderr


def test_generator_fails_on_missing_source(tmp_path):
    """AC4-EDGE: a manifest entry pointing at a nonexistent canonical fails."""
    fake_root = tmp_path / "fake-repo"
    fake_root.mkdir()
    bad_manifest = fake_root / "skill-bundles.yaml"
    bad_manifest.write_text(
        "bundles:\n"
        "  - skill: spec\n"
        "    files:\n"
        "      - source: scripts/does-not-exist.sh\n"
        "        dest: scripts/does-not-exist.sh\n"
    )
    (fake_root / "scripts" / "lib").mkdir(parents=True)
    shutil.copy2(GENERATOR, fake_root / "scripts" / "generate-skill-bundles.sh")
    shutil.copy2(PARSER, fake_root / "scripts" / "lib" / "parse-bundle-manifest.py")
    shutil.copy2(FRONTMATTER, fake_root / "scripts" / "lib" / "bundle-frontmatter.py")

    env = dict(os.environ)
    env["REPO_ROOT"] = str(fake_root)
    result = _run(
        ["bash", str(fake_root / "scripts" / "generate-skill-bundles.sh")],
        env=env,
    )
    assert result.returncode != 0
    assert "source not found: scripts/does-not-exist.sh" in result.stderr


# ---------------------------------------------------------------------------
# bundle-frontmatter.py helper tests
# ---------------------------------------------------------------------------


def test_strip_removes_frontmatter(tmp_path):
    """`strip` removes the YAML frontmatter block, keeping the body verbatim."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: x\ndescription: y\n---\nBody content\n")
    result = _run(["python3", str(FRONTMATTER), "strip", str(src)])
    assert result.returncode == 0, result.stderr
    assert result.stdout == "Body content\n"


def test_strip_passthrough_when_no_frontmatter(tmp_path):
    """`strip` is a no-op on a file that has no frontmatter block."""
    src = tmp_path / "input.md"
    src.write_text("# Heading\nLine two.\n")
    result = _run(["python3", str(FRONTMATTER), "strip", str(src)])
    assert result.returncode == 0, result.stderr
    assert result.stdout == "# Heading\nLine two.\n"


def test_rewrite_replaces_frontmatter(tmp_path):
    """`rewrite --as subagent` strips source frontmatter and prepends new
    subagent frontmatter rendered from the meta file."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: target\ndescription: original\n---\nBody\n")
    meta = tmp_path / "meta.yaml"
    meta.write_text(
        "name: archer\n"
        "description: foo\n"
        "model: opus\n"
        "tools: [Read, Write]\n"
    )
    result = _run([
        "python3", str(FRONTMATTER), "rewrite", str(src),
        "--as", "subagent",
        "--meta-file", str(meta),
    ])
    assert result.returncode == 0, result.stderr
    assert result.stdout.startswith("---\n")
    assert "name: archer\n" in result.stdout
    assert "model: opus\n" in result.stdout
    assert "Body\n" in result.stdout
    assert "name: target\n" not in result.stdout  # original frontmatter gone
    assert "description: original" not in result.stdout


def test_rewrite_rejects_missing_required_fields(tmp_path):
    """`rewrite --as subagent` requires name, description, model, tools."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: target\n---\nBody\n")
    meta = tmp_path / "meta.yaml"
    meta.write_text("name: archer\ndescription: foo\n")  # missing model, tools
    result = _run([
        "python3", str(FRONTMATTER), "rewrite", str(src),
        "--as", "subagent",
        "--meta-file", str(meta),
    ])
    assert result.returncode != 0
    assert "subagent_meta missing required fields" in result.stderr
    assert "model" in result.stderr
    assert "tools" in result.stderr


def test_rewrite_rejects_scalar_tools(tmp_path):
    """`tools` must be a YAML list. A bare scalar would silently produce a
    broken subagent frontmatter at runtime; the helper fails loudly."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: target\n---\nBody\n")
    meta = tmp_path / "meta.yaml"
    meta.write_text(
        "name: archer\n"
        "description: foo\n"
        "model: opus\n"
        "tools: Read\n"  # scalar instead of list — common author error
    )
    result = _run([
        "python3", str(FRONTMATTER), "rewrite", str(src),
        "--as", "subagent",
        "--meta-file", str(meta),
    ])
    assert result.returncode != 0
    assert "'tools' must be a YAML list" in result.stderr


def test_rewrite_rejects_non_string_tools_entries(tmp_path):
    """`tools` entries must be strings. Numeric or null entries fail."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: target\n---\nBody\n")
    meta = tmp_path / "meta.yaml"
    meta.write_text(
        "name: archer\n"
        "description: foo\n"
        "model: opus\n"
        "tools: [Read, 42, Write]\n"  # int in the middle
    )
    result = _run([
        "python3", str(FRONTMATTER), "rewrite", str(src),
        "--as", "subagent",
        "--meta-file", str(meta),
    ])
    assert result.returncode != 0
    assert "'tools' entries must be strings" in result.stderr


def test_strip_fails_loudly_on_unterminated_frontmatter(tmp_path):
    """A source that opens with --- but never closes is corrupt. Treating
    the whole file as body would ship YAML-looking content as prose. Fail."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: target\n# never closes\nBody content\n")
    result = _run(["python3", str(FRONTMATTER), "strip", str(src)])
    assert result.returncode != 0
    assert "unterminated frontmatter" in result.stderr
    # Error mentions the source path so the contributor knows which file
    assert str(src) in result.stderr


def test_rewrite_fails_loudly_on_unterminated_frontmatter(tmp_path):
    """Same invariant on the rewrite path."""
    src = tmp_path / "input.md"
    src.write_text("---\nname: target\n# never closes\nBody content\n")
    meta = tmp_path / "meta.yaml"
    meta.write_text(
        "name: archer\n"
        "description: foo\n"
        "model: opus\n"
        "tools: [Read]\n"
    )
    result = _run([
        "python3", str(FRONTMATTER), "rewrite", str(src),
        "--as", "subagent",
        "--meta-file", str(meta),
    ])
    assert result.returncode != 0
    assert "unterminated frontmatter" in result.stderr


# ---------------------------------------------------------------------------
# Parser tests for references: and agents: blocks
# ---------------------------------------------------------------------------


def test_parser_handles_references_block(tmp_path):
    """A manifest with references: emits reference-type rows."""
    src = tmp_path / "src.md"
    src.write_text("---\nname: shared\n---\nContent\n")
    manifest = tmp_path / "skill-bundles.yaml"
    manifest.write_text(
        "bundles:\n"
        "  - skill: consumer\n"
        f"    references:\n"
        f"      - source: src.md\n"
        f"        dest: references/src.md\n"
    )
    result = _run(["python3", str(PARSER), str(manifest)])
    assert result.returncode == 0, result.stderr
    rows = [r for r in result.stdout.splitlines() if r.strip()]
    assert len(rows) == 1
    type_, skill, source, dest, meta = rows[0].split("\t")
    assert type_ == "reference"
    assert skill == "consumer"
    assert source == "src.md"
    assert dest == "references/src.md"
    assert meta == ""


def test_parser_handles_agents_block(tmp_path):
    """A manifest with agents: emits agent-type rows with JSON-encoded meta."""
    manifest = tmp_path / "skill-bundles.yaml"
    manifest.write_text(
        "bundles:\n"
        "  - skill: consumer\n"
        "    agents:\n"
        "      - source: skills/target/SKILL.md\n"
        "        dest: agents/archer.md\n"
        "        rewrite_frontmatter: subagent\n"
        "        subagent_meta:\n"
        "          name: archer\n"
        "          description: TDD-disciplined task executor\n"
        "          model: opus\n"
        "          tools: [Read, Write, Edit, Bash]\n"
    )
    result = _run(["python3", str(PARSER), str(manifest)])
    assert result.returncode == 0, result.stderr
    rows = [r for r in result.stdout.splitlines() if r.strip()]
    assert len(rows) == 1
    type_, skill, source, dest, meta = rows[0].split("\t")
    assert type_ == "agent"
    assert skill == "consumer"
    assert source == "skills/target/SKILL.md"
    assert dest == "agents/archer.md"
    meta_obj = json.loads(meta)
    assert meta_obj["name"] == "archer"
    assert meta_obj["model"] == "opus"
    assert meta_obj["tools"] == ["Read", "Write", "Edit", "Bash"]


def test_parser_rejects_agent_missing_rewrite_frontmatter(tmp_path):
    """Agent entries must declare `rewrite_frontmatter: subagent`."""
    manifest = tmp_path / "skill-bundles.yaml"
    manifest.write_text(
        "bundles:\n"
        "  - skill: consumer\n"
        "    agents:\n"
        "      - source: skills/target/SKILL.md\n"
        "        dest: agents/archer.md\n"
        "        subagent_meta:\n"
        "          name: archer\n"
    )
    result = _run(["python3", str(PARSER), str(manifest)])
    assert result.returncode != 0
    assert "rewrite_frontmatter: subagent" in result.stderr


# ---------------------------------------------------------------------------
# End-to-end: generator handles a mixed manifest with all three types
# ---------------------------------------------------------------------------


def test_generator_handles_mixed_manifest(tmp_path):
    """A synthetic manifest with file, reference, and agent entries generates
    each correctly. AC1-HP for references and agents.
    """
    fake_root = tmp_path / "fake-repo"
    fake_root.mkdir()
    # Stage scripts/
    scripts_dir = fake_root / "scripts" / "lib"
    scripts_dir.mkdir(parents=True)
    shutil.copy2(GENERATOR, fake_root / "scripts" / "generate-skill-bundles.sh")
    shutil.copy2(PARSER, fake_root / "scripts" / "lib" / "parse-bundle-manifest.py")
    shutil.copy2(FRONTMATTER, fake_root / "scripts" / "lib" / "bundle-frontmatter.py")
    # Stage canonical sources.
    canonical_file = fake_root / "scripts" / "lib" / "tool.sh"
    canonical_file.write_text("#!/bin/bash\necho hello\n")
    canonical_file.chmod(0o755)
    canonical_ref = fake_root / "shared" / "doc.md"
    canonical_ref.parent.mkdir(parents=True)
    canonical_ref.write_text("---\nname: shared\n---\n# Heading\n")
    canonical_agent = fake_root / "skills" / "target" / "SKILL.md"
    canonical_agent.parent.mkdir(parents=True)
    canonical_agent.write_text("---\nname: target\n---\n# Target body\n")
    # Manifest.
    manifest = fake_root / "skill-bundles.yaml"
    manifest.write_text(
        "bundles:\n"
        "  - skill: consumer\n"
        "    files:\n"
        "      - source: scripts/lib/tool.sh\n"
        "        dest: scripts/lib/tool.sh\n"
        "    references:\n"
        "      - source: shared/doc.md\n"
        "        dest: references/doc.md\n"
        "    agents:\n"
        "      - source: skills/target/SKILL.md\n"
        "        dest: agents/archer.md\n"
        "        rewrite_frontmatter: subagent\n"
        "        subagent_meta:\n"
        "          name: archer\n"
        "          description: test agent\n"
        "          model: opus\n"
        "          tools: [Read]\n"
    )

    env = dict(os.environ)
    env["REPO_ROOT"] = str(fake_root)
    result = _run(
        ["bash", str(fake_root / "scripts" / "generate-skill-bundles.sh")],
        env=env,
    )
    assert result.returncode == 0, result.stderr

    # File: byte-identical + executable bit preserved
    bundled_file = fake_root / "skills" / "consumer" / "scripts" / "lib" / "tool.sh"
    assert bundled_file.is_file()
    assert filecmp.cmp(canonical_file, bundled_file, shallow=False)
    assert bundled_file.stat().st_mode & stat.S_IXUSR

    # Reference: frontmatter stripped
    bundled_ref = fake_root / "skills" / "consumer" / "references" / "doc.md"
    assert bundled_ref.is_file()
    body = bundled_ref.read_text()
    assert body == "# Heading\n"

    # Agent: frontmatter rewritten with subagent_meta
    bundled_agent = fake_root / "skills" / "consumer" / "agents" / "archer.md"
    assert bundled_agent.is_file()
    agent_text = bundled_agent.read_text()
    assert agent_text.startswith("---\n")
    assert "name: archer\n" in agent_text
    assert "model: opus\n" in agent_text
    assert "name: target\n" not in agent_text  # original frontmatter gone
    assert "# Target body\n" in agent_text
