"""_registry_self_proof: the resolve-owned-identity proof policy (x-a0cd).

Measured live on a daemon-hosted codex thread: the same shell resolves
``single`` when the verb runs directly and ``ambiguous``-with-own-row-collision
when it runs through init's ``$( )`` subshell, because the psutil walk degrades
in the subshell topology and collision-elimination then rejects the worker's
OWN spawn-minted row. These tests pin the registry-as-prover escalation and
its multi-family boundary."""

from fno.target_cli import _registry_self_proof


def _prove(**kwargs):
    kwargs.setdefault("true_harness", None)
    kwargs.setdefault("own_binding", None)
    kwargs.setdefault("present_families", set())
    kwargs.setdefault("owning_row_harness", lambda sid: None)
    return _registry_self_proof("codex", "thread-1", **kwargs)


def test_tree_proof_wins_when_harness_matches():
    assert _prove(true_harness="codex") is True


def test_tree_proof_contradicts_other_harness():
    assert _prove(true_harness="claude") is False


def test_own_binding_stamp_backed_by_same_harness_row_proves_self():
    assert _prove(
        own_binding=("codex", "thread-1"),
        owning_row_harness=lambda sid: "codex",
    ) is True


def test_own_binding_row_harness_disagreement_stays_unproven():
    assert _prove(
        own_binding=("codex", "thread-1"),
        owning_row_harness=lambda sid: "claude",
    ) is None


def test_single_family_row_agreement_proves_self_without_stamp():
    """The x-a0cd branch: no canonical stamp (daemon-hosted thread), tree
    silent, one family, and that family's live row holds the id - self."""
    assert _prove(
        present_families={"codex"},
        owning_row_harness=lambda sid: "codex",
    ) is True


def test_single_family_without_backing_row_stays_unproven():
    assert _prove(present_families={"codex"}) is None


def test_multi_family_never_self_proves_from_registry():
    """A foreign marker arrives BESIDE this session's own: the codex id in a
    poisoned claude env must stay unproven so collision-elimination rejects
    it (the x-b57a shape)."""
    assert _prove(
        present_families={"codex", "claude"},
        owning_row_harness=lambda sid: "codex",
    ) is None


def test_other_family_row_agreement_does_not_prove():
    assert _prove(
        present_families={"claude"},
        owning_row_harness=lambda sid: "codex",
    ) is None
