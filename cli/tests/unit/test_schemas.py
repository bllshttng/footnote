"""Tests for fno.schemas - AC1-HP, AC2-ERR, AC3-EDGE."""
import pytest
from pydantic import ValidationError

from fno.schemas import load_schema


def test_ac1_hp_load_target():
    """AC1-HP: load_schema('target') returns a pydantic model class."""
    Schema = load_schema("target")
    assert hasattr(Schema, "model_validate"), "must be a pydantic v2 model"


def test_ac2_err_unknown_type_rejected():
    """AC2-ERR: load_schema('bogus') raises ValueError."""
    with pytest.raises(ValueError, match="unknown state type"):
        load_schema("bogus")


def test_ac3_edge_session_id_invalid_rejected():
    """AC3-EDGE: SessionId regex enforced - 'invalid' must fail validation."""
    Schema = load_schema("target")
    with pytest.raises(ValidationError):
        Schema(session_id="invalid")


def test_ac3_edge_session_id_valid_accepted():
    """AC3-EDGE: Valid SessionId passes."""
    Schema = load_schema("target")
    # Should not raise when session_id matches YYYYMMDDTHHMMSSZ-PID-{6hex}
    instance = Schema(session_id="20260421T093631Z-97817-920dac")
    assert instance.session_id == "20260421T093631Z-97817-920dac"


def test_fno_id_backfilled_from_legacy_session_id():
    """AC1-HP boundary: a pre-rename manifest (session_id only) back-fills fno_id."""
    Schema = load_schema("target")
    instance = Schema(session_id="20260421T093631Z-97817-920dac")
    assert instance.fno_id == "20260421T093631Z-97817-920dac"
    assert instance.session_id == "20260421T093631Z-97817-920dac"


def test_fno_id_wins_when_both_present():
    """fno_id is canonical: an explicit fno_id is never clobbered by session_id."""
    Schema = load_schema("target")
    instance = Schema(
        fno_id="20260421T093631Z-cl97817-920dac",
        session_id="20260101T000000Z-11111-aaaaaa",
    )
    assert instance.fno_id == "20260421T093631Z-cl97817-920dac"


def test_session_id_accepts_canonical_codex_thread_uuid():
    Schema = load_schema("target")
    sid = "019f48e1-e641-7170-9ea9-921f07021967"
    assert Schema(session_id=sid).session_id == sid


@pytest.mark.parametrize(
    "sid",
    [
        "codex-thread",
        "019f48e1-e641-7170-9ea9",
        "019f48e1-e641-7170-9ea9-921f0702196g",
        "019F48E1-E641-7170-9EA9-921F07021967",
    ],
)
def test_session_id_rejects_noncanonical_codex_thread_strings(sid):
    Schema = load_schema("target")
    with pytest.raises(ValidationError):
        Schema(session_id=sid)


@pytest.mark.parametrize(
    "sid",
    [
        "20260630T192705Z-cl52366-8979b6",  # provider infix (self-mint, claude)
        "20260630T000000Z-mw42092-deadbe",  # driver infix (megawalk)
    ],
)
def test_session_id_accepts_provenance_infix(sid):
    """The 2-char provenance infix glued to the pid (segment 2) validates."""
    Schema = load_schema("target")
    assert Schema(session_id=sid).session_id == sid


def test_ac3_edge_status_enum_invalid_rejected():
    """AC3-EDGE: Status must be IN_PROGRESS, COMPLETE, or BLOCKED."""
    Schema = load_schema("target")
    with pytest.raises(ValidationError):
        Schema(status="GARBAGE")


def test_ac3_edge_roadmap_id_invalid_rejected():
    """AC3-EDGE: RoadmapId regex enforced - 'bad' must fail validation."""
    Schema = load_schema("target")
    with pytest.raises(ValidationError):
        Schema(graph_id="bad-format")


def test_ac3_edge_roadmap_id_valid_accepted():
    """AC3-EDGE: Valid RoadmapId passes."""
    Schema = load_schema("target")
    instance = Schema(graph_id="ab-eea09178")
    assert instance.graph_id == "ab-eea09178"


def test_target_default_instantiation():
    """HP: TargetState can be instantiated with no args (all fields have defaults)."""
    Schema = load_schema("target")
    instance = Schema()
    assert instance.status == "IN_PROGRESS"
