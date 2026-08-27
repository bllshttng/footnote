"""collision.partition: one file-overlap grouping at node and task grain.

``partition`` is the union-find under ``find_collisions`` (node grain) and
the orchestrator's ``partition_edges`` (task grain). Locked Decision 1 of
x-c06a: one function, three consumers.
"""
from __future__ import annotations

from fno.graph.collision import partition


class TestPartition:
    def test_shared_path_groups_overlap(self):
        # AC1-HP: A and C share x.py; B is disjoint.
        groups, unevaluated = partition(
            [("A", {"x.py"}), ("B", {"y.py"}), ("C", {"x.py", "z.py"})]
        )
        assert groups == [{"A", "C"}, {"B"}]
        assert unevaluated == set()

    def test_empty_path_set_is_singleton_and_unevaluated(self):
        # AC1-ERR: no parseable file list is its own verdict, never a silent pass.
        groups, unevaluated = partition([("D", set()), ("E", {"a.py"})])
        assert groups == [{"D"}, {"E"}]
        assert unevaluated == {"D"}

    def test_transitive_chain_is_one_group(self):
        # A shares with B, B shares with C; union-find closes the chain.
        groups, _ = partition(
            [("A", {"x.py"}), ("B", {"x.py", "y.py"}), ("C", {"y.py"})]
        )
        assert groups == [{"A", "B", "C"}]

    def test_path_normalization_unions_spellings(self):
        groups, _ = partition([("A", {"./src/x.py"}), ("B", {"src/x.py"})])
        assert groups == [{"A", "B"}]

    def test_shared_output_root_groups_disjoint_paths(self):
        groups, _ = partition(
            [("A", {".codex/agents/target.toml"}), ("B", {".codex/agents/review.toml"})],
            shared_roots=(".codex/agents/",),
        )
        assert groups == [{"A", "B"}]

    def test_no_shared_roots_keeps_disjoint_paths_apart(self):
        # Node grain passes no roots: the output-root rule is task-grain only,
        # so find_collisions verdicts stay byte-identical.
        groups, _ = partition(
            [("A", {".codex/agents/target.toml"}), ("B", {".codex/agents/review.toml"})]
        )
        assert groups == [{"A"}, {"B"}]

    def test_groups_are_sorted_and_deterministic(self):
        groups, _ = partition(
            [("Z", {"a.py"}), ("M", {"b.py"}), ("A", {"a.py"})]
        )
        assert groups == [{"A", "Z"}, {"M"}]

    def test_empty_input_returns_empty_partition(self):
        assert partition([]) == ([], set())
