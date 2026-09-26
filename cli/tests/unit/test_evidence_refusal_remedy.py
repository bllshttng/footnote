import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

from tests.unit._front_dev import front_dev_binary


runner = CliRunner()
REPO_ROOT = Path(__file__).resolve().parents[3]


def test_unmeasured_refusal_names_only_remedies_the_verbs_support():
    evidence = REPO_ROOT / "crates/fno-agents/src/evidence.rs"
    source = evidence.read_text(encoding="utf-8")
    start = source.index('the ruling asserts a code fact')
    end = source.index("Pair a zero with a control", start)
    refusal = source[start:end]

    assert "fno inbox decide" in refusal
    assert "--question-id" in refusal
    assert "fno inbox law set" in refusal
    assert "fno inbox outstanding clear" in refusal
    assert "with no --answer" in refusal

    result = runner.invoke(app, ["inbox", "decide", "--help"])
    assert result.exit_code == 0, result.output
    for option in ("--read", "--question-id"):
        assert option in result.output, f"fno inbox decide must expose {option}"

    clear = runner.invoke(app, ["inbox", "outstanding", "clear", "--help"])
    assert clear.exit_code == 0, clear.output
    assert "--read" not in clear.output, "fno inbox outstanding clear does not take --read"


@pytest.mark.skipif(
    front_dev_binary() is None,
    reason="compiled fno front binary not present (build with `cargo build --manifest-path crates/fno/Cargo.toml --bin fno)`",
)
def test_law_set_remedy_is_served_by_the_native_front():
    """`fno inbox law set` is native on the Rust front; its --help is the
    remedy surface the refusal names."""
    front = front_dev_binary()
    assert front is not None
    result = subprocess.run(
        [str(front), "inbox", "law", "set", "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert "--read" in result.stdout, "fno inbox law set must expose --read"
