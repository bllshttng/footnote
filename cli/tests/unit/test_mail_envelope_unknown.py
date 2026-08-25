from fno.mail.envelope import harness_for_provider


def test_missing_provider_renders_explicit_unknown_harness():
    assert harness_for_provider(None) == "unknown"
