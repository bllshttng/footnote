"""collision.partition: one file-overlap grouping at node and task grain.

``partition`` is the union-find under ``find_collisions`` (node grain) and
the orchestrator's ``partition_edges`` (task grain): one function, three
consumers.
"""
from __future__ import annotations

from pathlib import Path

from fno.graph.collision import find_collisions, partition


def _plan(tmp_path: Path, name: str, *files: str) -> Path:
    plan = tmp_path / name
    rows = "\n".join(f"| `{f}` | Modify |" for f in files)
    plan.write_text(
        f"# {name}\n\n## Files to Modify\n\n| File | Action |\n|---|---|\n{rows}\n"
    )
    return plan


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

    def test_repeated_item_id_keeps_its_unions(self):
        # Two path batches for one id: the second must extend the first's
        # group, not reset the item back to a singleton.
        groups, unevaluated = partition(
            [("A", {"x.py"}), ("B", {"y.py"}), ("A", {"y.py"})]
        )
        assert groups == [{"A", "B"}]
        assert unevaluated == set()


class TestFindCollisionsThroughPartition:
    def test_each_collision_reports_its_own_plan_path(self, tmp_path):
        # Two comparators, one group: the restructure through partition must
        # not leave with_plan_path pointing at the last-comparator's plan.
        candidate = _plan(tmp_path, "cand.md", "a.py", "b.py")
        one = _plan(tmp_path, "one.md", "a.py")
        two = _plan(tmp_path, "two.md", "b.py")
        graph = [
            {"id": "n-one", "plan_path": str(one), "status": "ready"},
            {"id": "n-two", "plan_path": str(two), "status": "ready"},
        ]
        by_id = {c.with_node_id: c for c in find_collisions(candidate, graph)}
        assert by_id["n-one"].with_plan_path == str(one)
        assert by_id["n-two"].with_plan_path == str(two)
