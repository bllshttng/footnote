//! One human-written duration, parsed the same way everywhere.

/// Parse a `<number><s|m|h|d>` duration into seconds (`24h`, `90m`, `30s`,
/// `7d`). `None` on anything else - an unparsable window is a usage error,
/// never "everything".
pub fn parse_duration_secs(raw: &str) -> Option<u64> {
    let (digits, unit) = raw.split_at(raw.len().saturating_sub(1));
    let multiplier = match unit {
        "s" => 1u64,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
}
