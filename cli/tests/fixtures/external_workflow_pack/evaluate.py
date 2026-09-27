#!/usr/bin/env python3
"""The external workflow pack's declared evidence producer.

A qualification run executes this script for real and keeps its actual exit
code and positive marker; a pack that records ``passed`` while this script
fails is a false success, never a qualification.
"""
import sys

POSITIVE_MARKER = "EVALUATOR PASS"


def main() -> int:
    if "--fail" in sys.argv[1:]:
        print("EVALUATOR FAIL")
        return 1
    print(POSITIVE_MARKER)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
