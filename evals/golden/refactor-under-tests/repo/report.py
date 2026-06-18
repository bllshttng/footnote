"""Report builder - parses, formats and summarises CSV-like line data."""


def build_report(text: str) -> str:
    """Build a human-readable report from a block of ``name,count`` lines.

    This function does too many things. Refactor it into ``parse_rows``,
    ``format_row``, and ``summarize`` helpers while keeping this API intact.
    """
    lines = [ln.strip() for ln in text.strip().splitlines() if ln.strip()]
    rows = []
    for line in lines:
        parts = line.split(",", 1)
        name = parts[0].strip()
        count = int(parts[1].strip()) if len(parts) > 1 else 0
        rows.append((name, count))

    formatted = []
    for name, count in rows:
        formatted.append(f"  {name}: {count}")

    total = sum(count for _, count in rows)
    report_lines = ["Report:"] + formatted + [f"Total: {total}"]
    return "\n".join(report_lines)
