//! Pure field logic shared by the backlog write verbs: the work-difficulty
//! band vocabulary, the canonical difficulty write, tag normalization, and
//! the dispatch-override write gate. The Python twins live in
//! `cli/src/fno/graph/_constants.py` and `cli/src/fno/backlog/dispatch_overrides.py`;
//! each pair gets a dual-implementation-inventory row at cut-over.

use serde_json::{json, Map, Value};

use super::settings;
use crate::naming;

/// `("low", "medium", "high")`.
pub const DIFFICULTY_BANDS: [&str; 3] = ["low", "medium", "high"];

/// A difficulty band, normalized: trimmed, lowercased, in the vocabulary.
pub fn normalize_difficulty(value: &str) -> Result<String, String> {
    let band = value.trim().to_lowercase();
    if !DIFFICULTY_BANDS.contains(&band.as_str()) {
        return Err(format!(
            "invalid difficulty '{value}'; must be one of: low, medium, high"
        ));
    }
    Ok(band)
}

/// The ONE canonical difficulty write: pop the retired spelling, write the
/// band, attribute via history. `history_on`: "change" (update - a
/// same-band revision is a history no-op) or "always" (claim -
/// confirmations count too). The claim lane's twin stays Python-owned until
/// the claim-side port deletes it.
pub fn write_canonical_difficulty(
    obj: &mut Map<String, Value>,
    band: Option<&str>,
    source: &str,
    ts: &str,
    history_on: &str,
) {
    let prior = obj
        .get("difficulty")
        .and_then(Value::as_str)
        .map(str::to_string);
    obj.remove("model_tier");
    if band.is_some() || obj.contains_key("difficulty") {
        obj.insert(
            "difficulty".into(),
            band.map(|b| json!(b)).unwrap_or(Value::Null),
        );
    }
    if history_on == "always" || band != prior.as_deref() {
        let mut history = match obj.get("difficulty_history") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        history.push(json!({"value": band, "source": source, "ts": ts}));
        obj.insert("difficulty_history".into(), Value::Array(history));
    }
}

/// Lowercase-trim a tag and validate its charset: `[a-z0-9-]+`.
pub fn normalize_tag(raw: &str) -> Result<String, String> {
    let tag = raw.trim().to_lowercase();
    let ok = !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(format!(
            "invalid tag '{raw}': tags must be lowercase-kebab [a-z0-9-] (letters, digits, hyphens)"
        ));
    }
    Ok(tag)
}

/// The write-side dispatch-verb gate: `(refusal, warning)`. Three checks in
/// order - shape (one bare token), the static name vocabulary, then the
/// configured allowlist/registry. A refusal exits 2 and aborts the write;
/// a warning rides back to the caller, who echoes it once after the write.
pub fn dispatch_verb_gate(value: &str) -> (Option<String>, Option<String>) {
    let accepted_static = || -> Vec<String> { naming::word_code_words() };
    if value.trim() != value || value.chars().any(char::is_whitespace) {
        let accepted = accepted_static().join(", ");
        return (
            Some(format!(
                "Error: --dispatch-verb '{value}' refused at write: not one bare verb word. \
                 The field takes a single verb token; a verb with an argument fails every \
                 drain at name mint. accepted: {accepted} (bare or '/fno:'-prefixed)"
            )),
            None,
        );
    }
    if crate::naming::verb_code_for(Some(value)).is_ok() {
        return (None, None);
    }
    let (allowed, registry) = settings::dispatch_verbs_from_config();
    let chosen = canonical_verb_key(value);
    if allowed.iter().any(|a| a == &chosen) || registry.iter().any(|r| r == &chosen) {
        return (
            None,
            Some(format!(
                "warning: dispatch verb '{value}' resolves only in \
                 config.dispatch.allowed_verbs/verb_registry; the drain's worker-name mint \
                 knows only {} and may refuse the dispatch",
                accepted_static().join(", ")
            )),
        );
    }
    let mut accepted: Vec<String> = accepted_static();
    accepted.extend(allowed);
    accepted.extend(registry);
    accepted.sort();
    accepted.dedup();
    (
        Some(format!(
            "Error: --dispatch-verb '{value}' refused at write: unknown dispatch verb. \
             The field takes one bare verb word; accepted here: {} \
             (bare or '/fno:'-prefixed), extend config.dispatch.allowed_verbs or \
             verb_registry for an outside verb",
            accepted.join(", "),
        )),
        None,
    )
}

/// Leading `/`, `/fno:x`, `$fno:x` and `$x` -> `/x`; keys the parser rejects
/// keep the legacy strip.
pub fn canonical_verb_key(key: &str) -> String {
    if let Some((body, _)) = crate::provider::parse_verb_token(key.trim()) {
        return format!("/{body}");
    }
    let k = key.trim().strip_prefix('/').unwrap_or(key.trim());
    let k = k.strip_prefix("fno:").unwrap_or(k);
    if k.is_empty() {
        k.to_string()
    } else {
        format!("/{k}")
    }
}

/// Store the dispatch overrides on the row, returning the warnings the
/// caller echoes once after the write lands. A gate refusal returns Err and
/// aborts the write.
pub fn apply_dispatch_overrides(
    obj: &mut Map<String, Value>,
    dispatch_verb: Option<&str>,
    dispatch_brief: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut warnings: Vec<String> = Vec::new();
    if let Some(raw) = dispatch_verb {
        if raw.eq_ignore_ascii_case("null") {
            obj.insert("dispatch_verb".into(), Value::Null);
        } else {
            let (refusal, warning) = dispatch_verb_gate(raw);
            if let Some(message) = refusal {
                return Err(message);
            }
            if let Some(w) = warning {
                warnings.push(w);
            }
            let mut verb_val = raw.to_string();
            if let Some((body, namespaced)) = crate::provider::parse_verb_token(raw) {
                if namespaced {
                    verb_val = format!("/fno:{body}");
                }
            }
            obj.insert("dispatch_verb".into(), json!(verb_val));
        }
    }
    if let Some(raw) = dispatch_brief {
        if raw.eq_ignore_ascii_case("null") {
            obj.insert("dispatch_brief".into(), Value::Null);
        } else {
            obj.insert("dispatch_brief".into(), json!(raw));
            let n_bytes = raw.len();
            if n_bytes > 8192 {
                warnings.push(format!(
                    "warning: dispatch brief is {n_bytes} bytes, over the 8192-byte (8 KB) \
                     env budget; shorten it (no silent truncation) - spawn will refuse it"
                ));
            }
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_bands_normalize_and_refuse() {
        assert_eq!(normalize_difficulty("HIGH").unwrap(), "high");
        assert!(normalize_difficulty("heroic").is_err());
    }

    #[test]
    fn canonical_write_drains_model_tier_and_attributes_change() {
        let mut obj = Map::new();
        obj.insert("difficulty".into(), json!("medium"));
        obj.insert("model_tier".into(), json!("team"));
        write_canonical_difficulty(&mut obj, Some("high"), "update", "<TS>", "change");
        assert_eq!(obj["difficulty"], json!("high"));
        assert!(obj.get("model_tier").is_none());
        let history = obj["difficulty_history"].as_array().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["source"], json!("update"));

        // A same-band revision is a history no-op in "change" mode.
        let before = obj["difficulty_history"].clone();
        write_canonical_difficulty(&mut obj, Some("high"), "update", "<TS>", "change");
        assert_eq!(obj["difficulty_history"], before);
    }

    #[test]
    fn tags_normalize_and_refuse_the_charset() {
        assert_eq!(normalize_tag("Big-Idea").unwrap(), "big-idea");
        assert!(normalize_tag("Bad_Tag!").is_err());
        assert!(normalize_tag("  ").is_err());
    }

    #[test]
    fn the_verb_gate_refuses_a_not_bare_token() {
        let (refusal, _) = dispatch_verb_gate("target now");
        assert!(refusal.expect("refusal").contains("not one bare verb word"));
    }
}
