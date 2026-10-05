"""Golden-capture driver: one Python-judge verdict per corpus row.

Reads the event JSON from argv[1] (a file, so the Rust side stays
pipe-free), runs the live `fno.events.validate`, and relays the verdict:
exit 0 valid, exit 1 invalid with the bare one-line diagnostic on
stderr, exit 2 substrate (schema unavailable). The Rust parity test
shells to this only under FNO_CAPTURE_GOLDEN=1.
"""

import json
import sys

try:
    from fno.events import SchemaUnavailableError, ValidationError, validate
except ImportError as exc:
    # The capture driver cannot run here (no project env). Exit 3 is a
    # distinct cannot-run code: the Rust side treats it as absence and
    # skips the row, never as a verdict.
    print(f"capture driver unavailable: {exc}", file=sys.stderr)
    sys.exit(3)


def main() -> int:
    with open(sys.argv[1], "r", encoding="utf-8") as fh:
        event = json.load(fh)
    try:
        validate(event)
    except SchemaUnavailableError as exc:
        print(f"schema unavailable: {exc}", file=sys.stderr)
        return 2
    except ValidationError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
