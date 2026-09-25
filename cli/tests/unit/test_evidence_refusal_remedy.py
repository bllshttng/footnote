from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app


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

    for command, options in (
        (("inbox", "decide"), ("--read", "--question-id")),
        (("inbox", "law", "set"), ("--read",)),
    ):
        result = runner.invoke(app, [*command, "--help"])
        assert result.exit_code == 0, result.output
        for option in options:
            assert option in result.output, f"fno {' '.join(command)} must expose {option}"

    clear = runner.invoke(app, ["inbox", "outstanding", "clear", "--help"])
    assert clear.exit_code == 0, clear.output
    assert "--read" not in clear.output, "fno inbox outstanding clear does not take --read"
