"""Tests for fno.schemas - AC1-HP, AC2-ERR, AC3-EDGE."""
import pytest
from pydantic import ValidationError

from fno.schemas import load_schema


def test_ac1_hp_load_target():
    """AC1-HP: load_schema('target') returns a pydantic model class."""
    Schema = load_schema("target")
    assert hasattr(Schema, "model_validate"), "must be a pydantic v2 model"


def test_ac1_hp_load_megawalk():
    """AC1-HP: load_schema('megawalk') returns a pydantic model class."""
    Schema = load_schema("megawalk")
    assert hasattr(Schema, "model_validate")


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


def test_megawalk_has_campaign_fields():
    """HP: MegawalkState has campaign_id, tick_count, last_reality_check_at."""
    Schema = load_schema("megawalk")
    instance = Schema()
    assert hasattr(instance, "campaign_id")
    assert hasattr(instance, "tick_count")
    assert hasattr(instance, "last_reality_check_at")
