"""Codex workflow skills must announce their executable runtime posture."""
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
POSTURES = {
    "target": "codex posture: target uses the native Stop loop on the main thread; delegated work uses spawn_agent; bg dispatch is Claude-only.",
    "do": "codex posture: do uses spawn_agent for wave tasks when available, with main-thread sequential fallback.",
    "think": "codex posture: think uses this Codex conversation as the source; dispatch defaults to Claude bg; explicit non-Claude providers are refused.",
    "blueprint": "codex posture: blueprint plans natively in this thread; auto-launch is Claude bg only, otherwise the node is visibly parked.",
}


def test_workflow_skills_have_codex_runtime_posture_instructions() -> None:
    for skill, posture in POSTURES.items():
        text = (ROOT / "skills" / skill / "SKILL.md").read_text(encoding="utf-8")
        assert "$CODEX_THREAD_ID` is nonblank" in text, skill
        assert "Print exactly once:" in text, skill
        assert f"`{posture}`" in text, skill
