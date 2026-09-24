"""The cargo test job must trigger for every repository input it reads."""
from __future__ import annotations

import fnmatch
import re
import subprocess
from pathlib import Path

import yaml

_REPO_ROOT = Path(__file__).resolve().parents[3]
_CLI_WORKFLOW = _REPO_ROOT / ".github" / "workflows" / "cli-ci.yml"

_CARGO_MANIFEST_DIR = "CARGO_MANIFEST_DIR"
_RELATIVE_PATH = re.compile(r"[\"']/?((?:\.\./){2,}[^\"']+)[\"']")


def _workflow() -> dict:
    return yaml.safe_load(_CLI_WORKFLOW.read_text(encoding="utf-8"))


def _trigger_paths() -> list[str]:
    workflow = _workflow()
    events = workflow["on"] if "on" in workflow else workflow[True]
    return events["pull_request"]["paths"]


def _rust_cross_tree_inputs() -> set[str]:
    files = subprocess.check_output(
        ["git", "ls-files", "crates/*.rs"], cwd=_REPO_ROOT, text=True
    ).splitlines()
    inputs: set[str] = set()
    for name in files:
        source = (_REPO_ROOT / name).read_text(encoding="utf-8").splitlines()
        manifest_dir = _REPO_ROOT / Path(name).parts[0] / Path(name).parts[1]
        for index, line in enumerate(source):
            if _CARGO_MANIFEST_DIR not in line:
                continue
            window = "\n".join(source[max(0, index - 3) : index + 4])
            for match in _RELATIVE_PATH.finditer(window):
                path = (manifest_dir / match.group(1)).resolve()
                inputs.add(path.relative_to(_REPO_ROOT).as_posix())
    return inputs


def _manifest_paths_diffed_by(run: str) -> set[str]:
    """Run the step's own awk filter over the manifest, so the test reads the paths CI diffs."""
    awk = re.search(r"awk -F'\\t' '(?P<prog>[^']+)'", run)
    assert awk, "the freshness step reads generated-artifacts.tsv without its awk filter"
    out = subprocess.check_output(
        ["awk", "-F\t", awk.group("prog"), "generated-artifacts.tsv"], cwd=_REPO_ROOT, text=True
    )
    return {line for line in out.splitlines() if line}


def _cargo_job_step_inputs() -> set[str]:
    jobs = _workflow()["jobs"]
    inputs: set[str] = set()
    for job_name in ("test-agents", "test-agents-integration", "test-mux", "test"):
        run_blocks = [step.get("run", "") for step in jobs[job_name]["steps"]]
        for run in run_blocks:
            for block in re.findall(r"git diff --exit-code --(?P<paths>.*?)(?:\n\n|\Z)", run, re.S):
                inputs.update(
                    path
                    for line in block.splitlines()
                    if (path := line.strip().lstrip("\\")) and not path.startswith("<")
                )
            if "generated-artifacts.tsv" in run:
                inputs.add("generated-artifacts.tsv")
                inputs.update(_manifest_paths_diffed_by(run))
            inputs.update(re.findall(r"bash ([^\s;&|]+)", run))
    return inputs


def _untriggered(paths: set[str], triggers: list[str]) -> list[str]:
    return sorted(
        path
        for path in paths
        if not any(fnmatch.fnmatch(path, trigger.replace("**", "*")) for trigger in triggers)
    )


def test_every_cargo_job_input_is_a_pull_request_trigger() -> None:
    triggers = _trigger_paths()
    rust_inputs = _rust_cross_tree_inputs()
    step_inputs = _cargo_job_step_inputs()
    assert "schemas/spawn-brevity.json" in rust_inputs
    assert "hooks/agy-target-stop-hook.sh" in rust_inputs
    assert "docs/harnesses/capability-matrix.md" in step_inputs

    untriggered = _untriggered(rust_inputs | step_inputs, triggers)
    assert not untriggered, f"cargo job inputs are not pull_request triggers: {untriggered}"


def test_push_and_pull_request_trigger_paths_match() -> None:
    events = _workflow()["on"] if "on" in _workflow() else _workflow()[True]
    assert events["push"]["paths"] == events["pull_request"]["paths"]
