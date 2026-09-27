//! Node-to-node relatedness over the backlog rows (deterministic v1), ported
//! from `cli/src/fno/graph/relatedness.py` for the filing path: the dedup
//! warning, the rollup candidate ladder, and the fold gate's candidate pool.
//! Pure functions over row values; the sidecar read is the only I/O.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

fn regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("relatedness regex")
}

/// Small stopword set - the words that co-occur in most backlog titles and
/// would inflate every Jaccard score toward noise.
fn stopwords() -> &'static BTreeSet<&'static str> {
    static SET: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are",
            "be", "at", "by", "as", "it", "its", "this", "that", "from", "add", "fix", "update",
            "make", "use", "via", "not", "no", "so",
        ]
        .into_iter()
        .collect()
    })
}

fn token_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex("[a-z0-9]+"))
}

const MIN_SCORE: f64 = 0.15;
const DOMAIN_BONUS: f64 = 0.10;
const EPIC_BONUS: f64 = 0.25;
/// Filing-time dedup floor: above this an existing node is surfaced as a
/// likely duplicate when a new node is born.
const DEDUP_MIN_SCORE: f64 = 0.30;

/// An epic in one of these states is no longer a rollup target.
pub fn is_retired_epic_status(status: Option<&str>) -> bool {
    matches!(status, Some("done") | Some("superseded") | Some("deferred"))
}

fn keep(t: &str) -> bool {
    // Drop stopwords, sub-3-char fragments, and pure-digit tokens: date parts
    // and ids in details are high-frequency noise that would rank nodes by
    // shared dates instead of shared meaning.
    t.len() >= 3 && !t.bytes().all(|b| b.is_ascii_digit()) && !stopwords().contains(t)
}

fn tokens(e: &Value) -> BTreeSet<String> {
    let mut text = String::new();
    for field in ["title", "slug", "details"] {
        if let Some(v) = e.get(field).and_then(Value::as_str) {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(v);
        }
    }
    token_re()
        .find_iter(&text.to_lowercase())
        .map(|m| m.as_str().to_string())
        .filter(|t| keep(t))
        .collect()
}

fn epic_key(e: &Value) -> Option<String> {
    // An epic is a roadmap group or an explicit parent; either shared is a
    // strong relatedness signal.
    for field in ["roadmap_id", "parent"] {
        if let Some(v) = e.get(field).and_then(Value::as_str) {
            if !v.trim().is_empty() {
                return Some(format!("{field}:{v}"));
            }
        }
    }
    None
}

/// Combined relatedness score for a pair + a one-line reason. 0 => drop.
///
/// `include_epic` drops the epic-parent bonus: dedup scoring calls with it
/// false, because two children of one epic are related, not duplicates, and
/// the +0.25 bonus would push an epic-sibling pair past the dedup threshold.
fn score(
    a: &Value,
    b: &Value,
    ta: &BTreeSet<String>,
    tb: &BTreeSet<String>,
    include_epic: bool,
    minimum: f64,
) -> (f64, String) {
    let mut reasons: Vec<String> = Vec::new();
    let mut combined = 0.0;

    if !ta.is_empty() && !tb.is_empty() {
        let inter: BTreeSet<String> = ta.intersection(tb).cloned().collect();
        if !inter.is_empty() {
            let jac = inter.len() as f64 / (ta | tb).len() as f64;
            combined += jac;
            // BTreeSet iteration is sorted, matching Python's sorted(inter).
            let shown: Vec<String> = inter.iter().take(3).cloned().collect();
            reasons.push(format!(
                "{} shared terms ({})",
                inter.len(),
                shown.join(", ")
            ));
        }
    }

    let da = a.get("domain").and_then(Value::as_str);
    let db = b.get("domain").and_then(Value::as_str);
    if let Some(da) = da {
        if !da.is_empty() && Some(da) == db {
            combined += DOMAIN_BONUS;
            reasons.push(format!("shared domain '{da}'"));
        }
    }

    if include_epic {
        let (ea, eb) = (epic_key(a), epic_key(b));
        if let (Some(ea), Some(eb)) = (&ea, &eb) {
            if ea == eb {
                combined += EPIC_BONUS;
                reasons.push(format!("same epic ({ea})"));
            }
        }
    }

    if combined < minimum {
        return (0.0, String::new());
    }
    (round4(combined), reasons.join("; "))
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// The raw relatedness score without applying a caller's floor. Discovery
/// needs the measured score for an FTS-only hit even when that score falls
/// below the filing floor.
pub fn score_pair(a: &Value, b: &Value, include_epic: bool) -> (f64, String) {
    score(a, b, &tokens(a), &tokens(b), include_epic, 0.0)
}

fn id_of(e: &Value) -> Option<&str> {
    e.get("id").and_then(Value::as_str)
}

/// Score `entry` against the live epics only, best-first, top-K. The rollup
/// counterpart to the dedup net: same score, narrowed to candidate parents.
/// Ties break on id so a run is reproducible.
pub fn epic_candidates(
    entry: &Value,
    entries: &[Value],
    k: usize,
    floor: Option<f64>,
) -> Vec<(String, f64, String)> {
    let ta = tokens(entry);
    let nid = id_of(entry);
    let minimum = floor.unwrap_or(MIN_SCORE);
    let mut scored: Vec<(String, f64, String)> = Vec::new();
    for e in entries {
        if e.get("type").and_then(Value::as_str) != Some("epic") {
            continue;
        }
        let Some(eid) = id_of(e) else { continue };
        if Some(eid) == nid {
            continue;
        }
        if is_retired_epic_status(e.get("status").and_then(Value::as_str)) {
            continue;
        }
        let (s, reason) = score(entry, e, &ta, &tokens(e), true, minimum);
        if s > 0.0 {
            scored.push((eid.to_string(), s, reason));
        }
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.truncate(k);
    scored
}

/// Ancestor ids of `entry` following parent links inside `by_id`. Readers
/// that score through the same substrate must not flag a node's own lineage
/// as a match.
pub fn lineage_ids(entry: &Value, by_id: &BTreeMap<String, Value>) -> BTreeSet<String> {
    let mut lineage = BTreeSet::new();
    let mut cur = entry
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string);
    while let Some(c) = cur {
        if !by_id.contains_key(&c) || lineage.contains(&c) {
            break;
        }
        lineage.insert(c.clone());
        cur = by_id
            .get(&c)
            .and_then(|e| e.get("parent"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    lineage
}

/// Score `entry` against every node for filing-time dedup, top-K. The dedup
/// twin of `epic_candidates`: every non-superseded node is a candidate (a
/// shipped done node is the answer to a duplicate filing), the epic bonus is
/// excluded, and the floor is the dedup threshold. Ties break on id. The
/// just-born node's own id and its lineage are excluded so a filing never
/// warns about itself or its rollup parent.
pub fn similar_nodes(
    entry: &Value,
    entries: &[Value],
    k: usize,
    floor: Option<f64>,
) -> Vec<(String, f64, String)> {
    let threshold = floor.unwrap_or(DEDUP_MIN_SCORE);
    let nid = id_of(entry);
    let ta = tokens(entry);
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| id_of(e).map(|i| (i.to_string(), e.clone())))
        .collect();
    let lineage = lineage_ids(entry, &by_id);
    let mut scored: Vec<(String, f64, String)> = Vec::new();
    for e in entries {
        let Some(eid) = id_of(e) else { continue };
        if Some(eid) == nid || lineage.contains(eid) {
            continue;
        }
        if e.get("status").and_then(Value::as_str) == Some("superseded") {
            continue;
        }
        let (s, reason) = score(entry, e, &ta, &tokens(e), false, threshold);
        if s >= threshold {
            scored.push((eid.to_string(), s, reason));
        }
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.truncate(k);
    scored
}

/// Live filing candidates from the last sidecar plus today's delta. A missing
/// or malformed sidecar is an evidence failure, not an empty result: the
/// caller receives every live row and a source marker naming the fallback.
pub fn filing_candidates(entries: &[Value], sidecar: &Path) -> (Vec<Value>, String) {
    let live: Vec<Value> = entries
        .iter()
        .filter(|e| {
            e.get("completed_at").is_none()
                && !matches!(
                    e.get("status").and_then(Value::as_str),
                    Some("done") | Some("superseded") | Some("deferred")
                )
                && id_of(e).is_some()
        })
        .cloned()
        .collect();
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return (live, "fallback:all-live".into());
    };
    let Ok(mapping) = serde_json::from_str::<Value>(&text) else {
        return (live, "fallback:all-live".into());
    };
    let Some(obj) = mapping.as_object() else {
        return (live, "fallback:all-live".into());
    };
    let Ok(groom_mtime) = sidecar.metadata().and_then(|m| m.modified()) else {
        return (live, "fallback:all-live".into());
    };
    let mut snapshot_ids: BTreeSet<String> = BTreeSet::new();
    for (key, edges) in obj {
        snapshot_ids.insert(key.clone());
        if let Some(list) = edges.as_array() {
            for edge in list {
                if let Some(id) = edge.get("id").and_then(Value::as_str) {
                    snapshot_ids.insert(id.to_string());
                }
            }
        }
    }
    let newer_than_groom = |e: &Value| -> bool {
        let raw = e
            .get("touched_at")
            .or_else(|| e.get("created_at"))
            .and_then(Value::as_str);
        let Some(raw) = raw else { return false };
        match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(ts) => {
                let groom: chrono::DateTime<chrono::Local> = groom_mtime.into();
                ts.with_timezone(&chrono::Local) > groom
            }
            Err(_) => false,
        }
    };
    let by_id: BTreeMap<String, Value> = live
        .iter()
        .filter_map(|e| id_of(e).map(|i| (i.to_string(), e.clone())))
        .collect();
    let mut selected: Vec<Value> = Vec::new();
    let mut selected_ids: BTreeSet<String> = BTreeSet::new();
    for nid in &snapshot_ids {
        if let Some(e) = by_id.get(nid) {
            selected_ids.insert(nid.clone());
            selected.push(e.clone());
        }
    }
    for e in &live {
        let id = id_of(e).expect("live rows carry ids").to_string();
        if !selected_ids.contains(&id) && newer_than_groom(e) {
            selected.push(e.clone());
        }
    }
    selected.sort_by(|a, b| {
        id_of(a)
            .unwrap_or_default()
            .cmp(id_of(b).unwrap_or_default())
    });
    (selected, "sidecar+since-groom".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, title: &str, extra: Value) -> Value {
        let mut v = json!({"id": id, "title": title, "domain": "code"});
        if let (Some(a), Some(b)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in b {
                a.insert(k.clone(), val.clone());
            }
        }
        v
    }

    #[test]
    fn token_overlap_and_domain_match_python_scores() {
        // The rollup_linked golden pins 0.70 = 3/5 jac + domain bonus; the
        // same inputs must score identically here.
        let incoming = row(
            "ab-new",
            "Deployment pipeline hardening phase two",
            json!({}),
        );
        let epic = row("x-e0aa0001", "Deployment pipeline hardening", json!({}));
        let (score, reason) = score_pair(&incoming, &epic, true);
        assert_eq!(score, 0.7);
        assert!(
            reason.contains("3 shared terms (deployment, hardening, pipeline)"),
            "{reason}"
        );
        assert!(reason.contains("shared domain 'code'"), "{reason}");
    }

    #[test]
    fn dedup_excludes_the_epic_bonus_and_the_own_lineage() {
        let child = row(
            "ab-child",
            "Fix deployment pipeline flake",
            json!({"parent": "x-epic"}),
        );
        let parent = row(
            "x-epic",
            "Deployment pipeline flake",
            json!({"type": "epic"}),
        );
        let other = row("x-dedp0001", "Fix deployment pipeline flake", json!({}));
        let entries = vec![child.clone(), parent, other];
        let scored = similar_nodes(&child, &entries, 3, None);
        assert_eq!(scored.len(), 1, "{scored:?}");
        assert_eq!(scored[0].0, "x-dedp0001");
    }

    #[test]
    fn retired_epics_and_self_are_not_rollup_candidates() {
        let incoming = row("ab-new", "Docs portal refresh follow-up", json!({}));
        let retired = row(
            "x-old",
            "Docs portal refresh",
            json!({"type": "epic", "status": "done"}),
        );
        let entries = vec![incoming.clone(), retired];
        assert!(epic_candidates(&incoming, &entries, 3, None).is_empty());
    }
}
