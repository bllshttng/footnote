"""Schema loader for fno state types.

Usage:
    from fno.schemas import load_schema
    Schema = load_schema("target")       # -> TargetState
"""
from __future__ import annotations

from typing import Type

from pydantic import BaseModel


def load_schema(type_: str) -> Type[BaseModel]:
    """Return the pydantic model class for the given state type.

    Args:
        type_: "target"

    Returns:
        Pydantic model class with model_validate method.

    Raises:
        ValueError: if type_ is not recognized.
    """
    if type_ == "target":
        from fno.schemas.target import TargetState
        return TargetState
    raise ValueError(f"unknown state type: {type_!r}")


__all__ = ["load_schema"]
