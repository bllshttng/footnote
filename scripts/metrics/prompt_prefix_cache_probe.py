#!/usr/bin/env python3
"""Measure prompt-cache reuse from Claude or Codex JSONL transcripts."""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class UsageSample:
    entry: str
    cache_read: int
    cache_creation: int | None
    input_tokens: int
    route: str


def extract_usage(path: Path) -> list[UsageSample]:
    samples: list[UsageSample] = []
    seen_claude_messages: set[str] = set()
    codex_model = "codex-unknown"
    with path.open(encoding="utf-8") as transcript:
        for line_number, line in enumerate(transcript, start=1):
            try:
                entry = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(
                    f"{path}:{line_number}: invalid JSON: {error.msg}"
                ) from error

            if entry.get("type") == "turn_context":
                codex_model = entry.get("payload", {}).get("model") or codex_model
                continue

            if entry.get("type") == "assistant":
                entry_name = entry.get("uuid") or f"line {line_number}"
                message = entry.get("message") or {}
                message_id = message.get("id") or entry_name
                usage = message.get("usage")
                if usage is None:
                    raise ValueError(
                        f"{path}:{line_number}: entry {entry_name} missing usage"
                    )
                required = (
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                    "input_tokens",
                )
                missing = [field for field in required if field not in usage]
                if missing:
                    raise ValueError(
                        f"{path}:{line_number}: entry {entry_name} missing usage fields: "
                        + ", ".join(missing)
                    )
                if message_id in seen_claude_messages:
                    continue
                seen_claude_messages.add(message_id)
                samples.append(
                    UsageSample(
                        entry=entry_name,
                        cache_read=int(usage["cache_read_input_tokens"]),
                        cache_creation=int(usage["cache_creation_input_tokens"]),
                        input_tokens=int(usage["input_tokens"]),
                        route=message.get("model") or "claude-unknown",
                    )
                )
                continue

            payload = entry.get("payload") or {}
            if entry.get("type") != "event_msg" or payload.get("type") != "token_count":
                continue
            entry_name = payload.get("turn_id") or f"line {line_number}"
            info = payload.get("info")
            if info is None:
                continue
            usage = info.get("last_token_usage")
            if usage is None:
                raise ValueError(
                    f"{path}:{line_number}: entry {entry_name} missing usage"
                )
            required = ("cached_input_tokens", "input_tokens")
            missing = [field for field in required if field not in usage]
            if missing:
                raise ValueError(
                    f"{path}:{line_number}: entry {entry_name} missing usage fields: "
                    + ", ".join(missing)
                )
            samples.append(
                UsageSample(
                    entry=entry_name,
                    cache_read=int(usage["cached_input_tokens"]),
                    cache_creation=(
                        int(usage["cache_write_input_tokens"])
                        if "cache_write_input_tokens" in usage
                        else None
                    ),
                    input_tokens=int(usage["input_tokens"]),
                    route=codex_model,
                )
            )
    if not samples:
        raise ValueError(
            f"{path}: no Claude assistant usage or Codex token_count entries"
        )
    return samples


def classify_pair(samples: list[UsageSample], stable_prefix_tokens: int) -> str:
    if len(samples) != 2:
        raise ValueError(f"expected exactly 2 usage samples, got {len(samples)}")
    if stable_prefix_tokens <= 0:
        raise ValueError("stable prefix tokens must be positive")
    delta = samples[1].cache_read - samples[0].cache_read
    return "HIT" if delta >= stable_prefix_tokens else "MISS"


def _sample_dict(scenario: str, request_index: int, sample: UsageSample) -> dict:
    return {
        "scenario": scenario,
        "request_index": request_index,
        "entry": sample.entry,
        "route": sample.route,
        "cache_read": sample.cache_read,
        "cache_creation": sample.cache_creation,
        "input_tokens": sample.input_tokens,
    }


def _emit_measurement(
    scenario: str,
    samples: list[UsageSample],
    stable_prefix_tokens: int,
    raw_out: Path | None = None,
) -> str:
    rows = [
        _sample_dict(scenario, index, sample) for index, sample in enumerate(samples, 1)
    ]
    for row in rows:
        print(json.dumps(row, sort_keys=True))
    result = classify_pair(samples, stable_prefix_tokens)
    delta = samples[1].cache_read - samples[0].cache_read
    print(
        "| scenario | route | request 1 cache read | request 2 cache read | delta | result |"
    )
    print("|---|---|---:|---:|---:|---|")
    print(
        f"| {scenario} | {samples[1].route} | {samples[0].cache_read} | "
        f"{samples[1].cache_read} | {delta} | {result} |"
    )
    if raw_out is not None:
        raw_out.write_text(json.dumps(rows, indent=2) + "\n", encoding="utf-8")
    return result


def _common_prefix_tokens(first: list[str], second: list[str]) -> int:
    count = 0
    for left, right in zip(first, second):
        if left != right:
            break
        count += 1
    return count


def _synthetic_prefix_scenario(
    first_early: str, second_early: str, stable_prefix_tokens: int
) -> list[UsageSample]:
    stable_tail = [f"stable-{index}" for index in range(stable_prefix_tokens - 1)]
    first_prompt = [first_early, *stable_tail]
    second_prompt = [second_early, *stable_tail]
    second_cache_read = _common_prefix_tokens(first_prompt, second_prompt)
    return [
        UsageSample(
            f"early-block={first_early}",
            0,
            len(first_prompt),
            len(first_prompt),
            "exact-prefix-simulator",
        ),
        UsageSample(
            f"early-block={second_early}",
            second_cache_read,
            len(second_prompt) - second_cache_read,
            len(second_prompt),
            "exact-prefix-simulator",
        ),
    ]


def instrument_control(stable_prefix_tokens: int) -> None:
    stable = _synthetic_prefix_scenario(
        "request-id=constant", "request-id=constant", stable_prefix_tokens
    )
    varying = _synthetic_prefix_scenario(
        "request-id=one", "request-id=two", stable_prefix_tokens
    )
    stable_result = _emit_measurement(
        "instrument-stable-prefix", stable, stable_prefix_tokens
    )
    varying_result = _emit_measurement(
        "instrument-varying-early", varying, stable_prefix_tokens
    )
    if stable_result != "HIT" or varying_result != "MISS":
        raise AssertionError(
            f"instrument control failed: stable={stable_result} varying={varying_result}"
        )
    print("CONTROL: PASS stable_prefix=HIT varying_early=MISS")


def _write_jsonl(path: Path, rows: list[dict]) -> None:
    path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")


def selftest() -> None:
    stable_control = _synthetic_prefix_scenario("constant", "constant", 8)
    varying_control = _synthetic_prefix_scenario("request-one", "request-two", 8)
    assert stable_control[1].cache_read == 8
    assert varying_control[1].cache_read == 0

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        claude = root / "claude.jsonl"
        _write_jsonl(
            claude,
            [
                {
                    "type": "assistant",
                    "uuid": "claude-entry-1",
                    "message": {
                        "id": "message-1",
                        "model": "claude-test",
                        "usage": {
                            "cache_read_input_tokens": 0,
                            "cache_creation_input_tokens": 8192,
                            "input_tokens": 7,
                        },
                    },
                },
                {
                    "type": "assistant",
                    "uuid": "claude-entry-2",
                    "message": {
                        "id": "message-2",
                        "model": "claude-test",
                        "usage": {
                            "cache_read_input_tokens": 8192,
                            "cache_creation_input_tokens": 64,
                            "input_tokens": 8,
                        },
                    },
                },
            ],
        )
        samples = extract_usage(claude)
        assert len(samples) == 2
        assert classify_pair(samples, 8192) == "HIT"
        print("PASS AC1-HP: shared stable prefix reports HIT")

        codex = root / "codex.jsonl"
        _write_jsonl(
            codex,
            [
                {
                    "type": "turn_context",
                    "payload": {"model": "gpt-test"},
                },
                {
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "cached_input_tokens": 4096,
                                "input_tokens": 8192,
                            }
                        },
                    },
                },
            ],
        )
        codex_samples = extract_usage(codex)
        assert codex_samples[0].cache_creation is None
        print("PASS AC1-HP: Codex cache creation may be unavailable")

        varying = [
            UsageSample("varying-1", 0, 8192, 7, "synthetic-control"),
            UsageSample("varying-2", 0, 8192, 8, "synthetic-control"),
        ]
        assert classify_pair(varying, 8192) == "MISS"
        print("PASS AC2-HP: varying early block reports MISS")

        malformed = root / "malformed.jsonl"
        _write_jsonl(
            malformed,
            [
                {
                    "type": "assistant",
                    "uuid": "missing-usage-entry",
                    "message": {"id": "missing-usage-message", "model": "claude-test"},
                }
            ],
        )
        try:
            extract_usage(malformed)
        except ValueError as error:
            assert "missing-usage-entry" in str(error)
            failure = str(error)
        else:
            raise AssertionError("missing usage fields did not fail")
        print(f"PASS AC3-ERR: {failure}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--instrument-control", action="store_true")
    parser.add_argument("--scenario")
    parser.add_argument("--transcript", type=Path)
    parser.add_argument(
        "--indices",
        default="-2,-1",
        help="two comma-separated Python indices into extracted usage entries",
    )
    parser.add_argument(
        "--entries",
        help="two comma-separated transcript entry ids; overrides --indices",
    )
    parser.add_argument("--stable-prefix-tokens", type=int, default=1024)
    parser.add_argument("--raw-out", type=Path)
    args = parser.parse_args()
    if args.selftest:
        selftest()
        return 0
    if args.instrument_control:
        instrument_control(args.stable_prefix_tokens)
        return 0
    if args.scenario and args.transcript:
        try:
            all_samples = extract_usage(args.transcript)
            if args.entries:
                entry_ids = args.entries.split(",")
                if len(entry_ids) != 2:
                    raise ValueError("--entries requires exactly two values")
                by_entry = {sample.entry: sample for sample in all_samples}
                missing = [entry for entry in entry_ids if entry not in by_entry]
                if missing:
                    raise ValueError(
                        "unknown transcript entries: " + ", ".join(missing)
                    )
                selected = [by_entry[entry] for entry in entry_ids]
            else:
                indices = [int(value) for value in args.indices.split(",")]
                if len(indices) != 2:
                    raise ValueError("--indices requires exactly two values")
                selected = [all_samples[index] for index in indices]
            _emit_measurement(
                args.scenario,
                selected,
                args.stable_prefix_tokens,
                args.raw_out,
            )
        except (IndexError, OSError, ValueError) as error:
            print(f"ERROR: {error}", file=sys.stderr)
            return 2
        return 0
    parser.error("one mode is required")


if __name__ == "__main__":
    raise SystemExit(main())
