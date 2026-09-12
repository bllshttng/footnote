"""apply() returns the over-budget warning; it never prints. The caller runs
inside locked_mutate_graph, which re-runs the mutator on contention, so a
print there would repeat per retry and still fire when all retries fail."""

from fno.backlog.dispatch_overrides import apply


def test_over_budget_brief_returns_warning_and_prints_nothing(capsys):
    node = {}
    warning = apply(node, None, "x" * 8193)
    assert warning is not None
    assert "dispatch brief is 8193 bytes, over the 8192-byte (8 KB) env budget" in warning
    assert node["dispatch_brief"] == "x" * 8193
    assert capsys.readouterr().err == ""


def test_within_budget_returns_none_and_prints_nothing(capsys):
    node = {}
    assert apply(node, None, "x" * 8192) is None
    assert node["dispatch_brief"] == "x" * 8192
    assert capsys.readouterr().err == ""


def test_null_clears_and_returns_none():
    node = {"dispatch_brief": "old"}
    assert apply(node, None, "null") is None
    assert node["dispatch_brief"] is None
