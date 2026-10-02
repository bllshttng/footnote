//! Plan collision detection: file-overlap as a proxy for plan overlap.
//!
//! Ports `cli/src/fno/graph/collision.py` decision for decision for the
//! lane-fill gate (this module's only caller in this PR; the wheel keeps its
//! own legs for triage and doctor until those ports land). A plan's parsed
//! file table is its comparable surface; an empty parse is UNEVALUATED, not
//! clean.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

pub const HIDDEN_SHARED_OUTPUT_ROOTS: &[&str] = &[
    ".fno/",
    ".codex/agents/",
    ".gemini/agents/",
    "docs/",
    "internal/",
];

/// Recognized headings under which the file list lives; "file ownership map"
/// is the section /blueprint writes, without which every blueprint plan
/// parses empty and silently cannot collide.
const FILE_HEADINGS: &[&str] = &[
    "files to modify",
    "files to change",
    "files touched",
    "file ownership map",
    "files",
];

/// Severity thresholds (v1 heuristics). The Pydantic
/// `CollisionThresholdsBlock` defaults, verbatim; a config override rides the
/// settings reader once the triage port moves this module's second caller.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Thresholds {
    #[serde(default = "default_high_count")]
    pub high_count: f64,
    #[serde(default = "default_high_ratio")]
    pub high_ratio: f64,
    #[serde(default = "default_medium_count")]
    pub medium_count: f64,
    #[serde(default = "default_medium_ratio")]
    pub medium_ratio: f64,
}

fn default_high_count() -> f64 {
    3.0
}
fn default_high_ratio() -> f64 {
    0.5
}
fn default_medium_count() -> f64 {
    2.0
}
fn default_medium_ratio() -> f64 {
    0.25
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            high_count: default_high_count(),
            high_ratio: default_high_ratio(),
            medium_count: default_medium_count(),
            medium_ratio: default_medium_ratio(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Coordinate,
    Absorb,
    Supersede,
}

/// One file overlap finding, sorted by severity then node id.
#[derive(Debug, Clone)]
pub struct Collision {
    pub with_node_id: String,
    pub with_node_title: String,
    pub with_plan_path: String,
    pub shared_files: Vec<String>,
    pub candidate_only_files: Vec<String>,
    pub other_only_files: Vec<String>,
    pub severity: Severity,
    pub recommended_action: Action,
    pub rationale: String,
    // Shape parity with the wheel's collision row; the fill classifier reads
    // only severity and with_node_id, the rationale builder takes the raw
    // value as a parameter.
    #[allow(dead_code)]
    other_created_at: String,
}

/// Strip backticks, parenthetical annotations, and trailing line suffixes
/// from one markdown table cell.
fn strip_path(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') {
            s = s[..open].trim_end();
        }
    }
    let s = s.replace('`', "");
    let s = s.trim();
    // Trailing `:42` or `:42-99` line suffix.
    if let Some(colon) = s.rfind(':') {
        let suffix = &s[colon + 1..];
        let numeric = suffix
            .split('-')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
        if numeric && colon > 0 {
            return s[..colon].to_string();
        }
    }
    s.to_string()
}

fn is_separator_row(line: &str) -> bool {
    let body = line.trim_start_matches('|');
    !body.is_empty() && body.chars().all(|c| matches!(c, ' ' | ':' | '-' | '|'))
}

fn is_file_table_row(line: &str) -> bool {
    if !line.starts_with('|') {
        return false;
    }
    !is_separator_row(line)
}

/// Walk lines after a recognized heading until the next `##` heading,
/// collecting column-1 paths of any markdown tables under it.
fn extract_files_from_section(lines: &[&str], heading_index: usize) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    let mut saw_header = false;
    let mut saw_separator = false;
    for line in lines.iter().skip(heading_index + 1) {
        let stripped = line.trim();
        if stripped.starts_with("##") {
            break;
        }
        if !is_file_table_row(stripped) {
            // Blank line after a table: another table may follow under the
            // same heading, so the header state resets.
            if saw_header && saw_separator && stripped.is_empty() {
                saw_header = false;
                saw_separator = false;
            }
            continue;
        }
        if !saw_header {
            saw_header = true;
            continue;
        }
        if !saw_separator {
            saw_separator = true;
            if is_separator_row(stripped) {
                continue;
            }
            // Tolerate plans that omit the separator: treat as passed.
        }
        let cell = stripped.trim_matches('|');
        let first = cell.split('|').next().unwrap_or("");
        let path = strip_path(first);
        if !path.is_empty() {
            files.insert(path);
        }
    }
    files
}

/// Read one markdown file and pull out its files-to-modify set. A read error
/// parses empty (the caller treats "no parseable files" as cannot-collide).
fn scan_one(path: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut found = BTreeSet::new();
    for (idx, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        if !stripped.starts_with("##") {
            continue;
        }
        let heading = stripped.trim_start_matches('#').trim().to_lowercase();
        if FILE_HEADINGS.iter().any(|h| heading.starts_with(h)) {
            found.extend(extract_files_from_section(&lines, idx));
        }
    }
    found
}

/// Extract file paths from the Files-to-Modify tables of a plan. A folder
/// plan spreads its tables across 00-INDEX.md and the phase files.
pub fn parse_files_to_modify(plan_path: &Path) -> BTreeSet<String> {
    if !plan_path.exists() {
        return BTreeSet::new();
    }
    if plan_path.is_dir() {
        let mut found = BTreeSet::new();
        let mut children: Vec<PathBuf> = match std::fs::read_dir(plan_path) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().map(|ext| ext == "md").unwrap_or(false))
                .collect(),
            Err(_) => return found,
        };
        children.sort();
        for child in children {
            found.extend(scan_one(&child));
        }
        return found;
    }
    scan_one(plan_path)
}

/// True when a plan states a surface the collision check can compare. False
/// means UNEVALUATED, not clean.
pub fn has_file_surface(plan_path: &Path) -> bool {
    !parse_files_to_modify(plan_path).is_empty()
}

/// The shared-output root a normalized path falls under, or None.
pub fn match_shared_root(path: &str, roots: &[&str]) -> Option<String> {
    let normalized = path.trim().trim_matches('`');
    let normalized = normalize_rel(normalized);
    if normalized.is_empty() || normalized == "." {
        return None;
    }
    for root in roots {
        let base = root.trim_end_matches('/');
        if normalized == base || normalized.starts_with(&format!("{base}/")) {
            return Some(base.to_string());
        }
    }
    None
}

fn normalize_rel(raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Resolve a stored `plan_path` (absolute, `~`, or repo-relative).
pub fn resolve_plan_path(plan_path: &str, repo_root: &Path) -> PathBuf {
    if plan_path.is_empty() {
        return PathBuf::new();
    }
    if let Some(rest) = plan_path.strip_prefix('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        return PathBuf::from(format!("{home}{rest}"));
    }
    let path = Path::new(plan_path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    repo_root.join(plan_path)
}

/// A node is collision-eligible: planned and not done/deferred/superseded.
fn is_pending_for_collision(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) == Some("roadmap") {
        return false;
    }
    if crate::backlog_ready::truthy(entry.get("completed_at")) {
        return false;
    }
    let status = entry
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("ready");
    if matches!(status, "done" | "deferred" | "superseded") {
        return false;
    }
    !entry
        .get("plan_path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
}

/// (severity, sorted shared files) for two file sets.
fn classify(
    candidate: &BTreeSet<String>,
    other: &BTreeSet<String>,
    t: &Thresholds,
) -> (Severity, Vec<String>) {
    let shared: Vec<String> = candidate.intersection(other).cloned().collect();
    if shared.is_empty() {
        return (Severity::Low, Vec::new());
    }
    let n_shared = shared.len() as f64;
    let min_set = candidate.len().min(other.len()).max(1) as f64;
    let max_set = candidate.len().max(other.len()).max(1) as f64;
    if n_shared >= t.high_count || (n_shared / min_set) >= t.high_ratio {
        return (Severity::High, shared);
    }
    if n_shared >= t.medium_count || (n_shared / max_set) >= t.medium_ratio {
        return (Severity::Medium, shared);
    }
    (Severity::Low, shared)
}

fn infer_action(
    candidate: &BTreeSet<String>,
    other: &BTreeSet<String>,
    other_created_at: &str,
    candidate_created_at: &str,
) -> Action {
    let shared: Vec<&String> = candidate.intersection(other).collect();
    if shared.is_empty() {
        return Action::Coordinate;
    }
    // Half of the WIDER surface is what "50% of both sides" means.
    let widest = candidate.len().max(other.len()).max(1) as f64;
    if (shared.len() as f64 / widest) < 0.5 {
        return Action::Coordinate;
    }
    if candidate.len() < other.len() && candidate.is_subset(other) {
        return Action::Absorb;
    }
    if other.len() < candidate.len() && other.is_subset(candidate) {
        return Action::Supersede;
    }
    if !other_created_at.is_empty()
        && !candidate_created_at.is_empty()
        && other_created_at < candidate_created_at
    {
        return Action::Absorb;
    }
    Action::Coordinate
}

fn build_rationale(
    candidate: &BTreeSet<String>,
    other: &BTreeSet<String>,
    shared: &[String],
    other_id: &str,
    severity: &Severity,
    action: &Action,
) -> String {
    let mut preview: Vec<String> = shared.iter().take(3).cloned().collect();
    let mut base = format!(
        "{} shared files ({}) of {} in this plan and {} in {}",
        shared.len(),
        {
            if shared.len() > 3 {
                preview.push(format!("... ({} total)", shared.len()));
            }
            preview.join(", ")
        },
        candidate.len(),
        other.len(),
        other_id,
    );
    match action {
        Action::Absorb => base += &format!("; {other_id} has wider scope, so absorbing your changes into {other_id} is the cleanest path."),
        Action::Supersede => base += &format!("; this plan covers everything {other_id} touches plus more, so superseding {other_id} is the cleanest path."),
        Action::Coordinate => {
            if *severity == Severity::Low {
                base += "; consider splitting the overlap into a shared dependency rather than two parallel touches.";
            } else {
                base += "; both plans can ship if the second one rebases on the first.";
            }
        }
    }
    base
}

/// Best-effort repo root: `git rev-parse --show-toplevel` then cwd. The
/// python leg memoizes per process; the fill gate runs once per dispatch,
/// so the probe is unconditional here.
pub fn find_repo_root() -> PathBuf {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        if output.status.success() {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !root.is_empty() {
                return PathBuf::from(root);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Group (id, paths) items by normalized shared path (union-find), with the
/// generated-output rule for `shared_roots`. Returns (groups, ids with no
/// usable path). An unevaluated id is its own verdict, never a silent pass.
pub fn partition(
    items: &[(String, BTreeSet<String>)],
    shared_roots: &[&str],
) -> (Vec<BTreeSet<String>>, BTreeSet<String>) {
    let mut parent: BTreeMap<String, String> = BTreeMap::new();
    for (id, _) in items {
        parent.entry(id.clone()).or_insert_with(|| id.clone());
    }
    fn find(parent: &mut BTreeMap<String, String>, node: &str) -> String {
        let mut node = node.to_string();
        while parent[&node] != node {
            let grandparent = parent[&parent[&node]].clone();
            parent.insert(node.clone(), grandparent);
            node = parent[&node].clone();
        }
        node
    }
    fn union(parent: &mut BTreeMap<String, String>, left: &str, right: &str) {
        let left_root = find(parent, left);
        let right_root = find(parent, right);
        if left_root != right_root {
            parent.insert(right_root, left_root);
        }
    }
    let mut path_owner: BTreeMap<String, String> = BTreeMap::new();
    let mut root_owner: BTreeMap<String, String> = BTreeMap::new();
    let mut evaluated: BTreeSet<String> = BTreeSet::new();
    for (id, paths) in items {
        for raw in paths {
            let normalized = normalize_rel(raw.trim().trim_matches('`'));
            if normalized.is_empty() || normalized == "." {
                continue;
            }
            evaluated.insert(id.clone());
            let owner = path_owner
                .entry(normalized.clone())
                .or_insert_with(|| id.clone())
                .clone();
            union(&mut parent, &owner, id);
            if let Some(root) = match_shared_root(&normalized, shared_roots) {
                let root_owner_id = root_owner.entry(root).or_insert_with(|| id.clone()).clone();
                union(&mut parent, &root_owner_id, id);
            }
        }
    }
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut unevaluated: BTreeSet<String> = BTreeSet::new();
    for (id, _) in items {
        let root = find(&mut parent, id);
        groups.entry(root).or_default().insert(id.clone());
        if !evaluated.contains(id) {
            unevaluated.insert(id.clone());
        }
    }
    let mut ordered: Vec<BTreeSet<String>> = groups.into_values().collect();
    ordered.sort();
    (ordered, unevaluated)
}

/// Compare the candidate plan against all pending plans on the graph.
///
/// `self_id` is required: the fill gate always has the candidate's node id,
/// and the self-exclusion default (the plan's own claimed id) stays a
/// wheel-side behavior for the callers that do not. Sorted by severity
/// descending, then node id ascending.
pub fn find_collisions(
    candidate_plan_path: &Path,
    graph: &[Value],
    self_id: &str,
    thresholds: &Thresholds,
) -> Vec<Collision> {
    let mut out: Vec<Collision> = Vec::new();
    let candidate_files = parse_files_to_modify(candidate_plan_path);
    if candidate_files.is_empty() {
        return out;
    }
    let repo_root = find_repo_root();
    let mut comparators: Vec<(&Value, String, BTreeSet<String>)> = Vec::new();
    for entry in graph {
        if !is_pending_for_collision(entry) {
            continue;
        }
        let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        if !self_id.is_empty() && entry_id == self_id {
            continue;
        }
        let Some(other_plan) = entry.get("plan_path").and_then(Value::as_str) else {
            continue;
        };
        let other_path = resolve_plan_path(other_plan, &repo_root);
        let other_files = parse_files_to_modify(&other_path);
        if other_files.is_empty() {
            continue;
        }
        comparators.push((entry, other_plan.to_string(), other_files));
    }

    let candidate_created = graph
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(self_id))
        .and_then(|entry| entry.get("created_at"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // One partition decides who is compared: the candidate's group-mates are
    // exactly the plans sharing a file. Severity still comes from the pair's
    // shared count and thresholds, so the verdict per pair is unchanged.
    let candidate_key = "<candidate>".to_string();
    let mut items: Vec<(String, BTreeSet<String>)> =
        vec![(candidate_key.clone(), candidate_files.clone())];
    for (index, (_, _, files)) in comparators.iter().enumerate() {
        items.push((format!("other:{index}"), files.clone()));
    }
    let (groups, _unevaluated) = partition(&items, &[]);
    for group in groups {
        if !group.contains(&candidate_key) {
            continue;
        }
        for key in &group {
            if *key == candidate_key {
                continue;
            }
            let index: usize = key
                .strip_prefix("other:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(usize::MAX);
            let Some((entry, other_plan, other_files)) = comparators.get(index) else {
                continue;
            };
            let (severity, shared_sorted) = classify(&candidate_files, other_files, thresholds);
            if shared_sorted.is_empty() {
                continue;
            }
            let entry_created = entry
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action = infer_action(
                &candidate_files,
                other_files,
                &entry_created,
                &candidate_created,
            );
            let entry_id = entry
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let rationale = build_rationale(
                &candidate_files,
                other_files,
                &shared_sorted,
                entry_id,
                &severity,
                &action,
            );
            let shared_set: BTreeSet<String> = shared_sorted.iter().cloned().collect();
            out.push(Collision {
                with_node_id: entry_id.to_string(),
                with_node_title: entry
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                with_plan_path: other_plan.clone(),
                shared_files: shared_sorted,
                candidate_only_files: candidate_files.difference(&shared_set).cloned().collect(),
                other_only_files: other_files.difference(&shared_set).cloned().collect(),
                severity: severity.clone(),
                recommended_action: action,
                rationale,
                other_created_at: entry_created,
            });
        }
    }
    out.sort_by(|a, b| {
        let order = |s: &Severity| match s {
            Severity::High => 0,
            Severity::Medium => 1,
            Severity::Low => 2,
        };
        order(&a.severity)
            .cmp(&order(&b.severity))
            .then(a.with_node_id.cmp(&b.with_node_id))
    });
    out
}

/// A node whose acknowledged collision has since shipped: the user accepted
/// the overlap at spec time, and the colliding plan is done or merged. The
/// `__skipped_check__` sentinel names no specific node, so it never
/// reconciles.
#[derive(Debug, Clone)]
pub struct AcknowledgedReconciliation {
    pub node_id: String,
    pub node_title: String,
    pub resolved_via: String,
    pub resolved_via_title: String,
    pub resolved_via_status: String,
}

pub fn find_acknowledged_collisions(graph: &[Value]) -> Vec<AcknowledgedReconciliation> {
    let by_id: BTreeMap<&str, &Value> = graph
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .map(|id| (id, entry))
        })
        .collect();
    let mut out: Vec<AcknowledgedReconciliation> = Vec::new();
    for entry in graph {
        let Some(node_id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(ack) = entry
            .get("collisions_acknowledged")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for ref_value in ack {
            let Some(ref_id) = ref_value.as_str() else {
                continue;
            };
            if ref_id == "__skipped_check__" {
                continue;
            }
            let Some(other) = by_id.get(ref_id) else {
                continue;
            };
            let status = other.get("status").and_then(Value::as_str).unwrap_or("");
            let merge_status = other
                .get("merge_status")
                .and_then(Value::as_str)
                .unwrap_or("");
            if status == "done" || merge_status == "merged" {
                out.push(AcknowledgedReconciliation {
                    node_id: node_id.to_string(),
                    node_title: entry
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    resolved_via: ref_id.to_string(),
                    resolved_via_title: other
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    resolved_via_status: if merge_status == "merged" {
                        "merged".to_string()
                    } else {
                        status.to_string()
                    },
                });
            }
        }
    }
    out.sort_by(|a, b| {
        a.node_id
            .cmp(&b.node_id)
            .then(a.resolved_via.cmp(&b.resolved_via))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn plan_with_files(dir: &Path, name: &str, files: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let rows: String = files
            .iter()
            .map(|f| format!("| `{f}` | modify |\n"))
            .collect();
        fs::write(
            &path,
            format!("# P\n\n## Files to Modify\n\n| File | Action |\n|---|---|\n{rows}"),
        )
        .unwrap();
        path
    }

    #[test]
    fn parser_reads_table_cells_and_strips_annotations() {
        let dir = std::env::temp_dir().join(format!("fno-col-parse-{}", crate::claims::now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.md");
        fs::write(
            &path,
            "# t\n\n## Files to Modify\n\n| File | Action |\n|---|---|\n| `cli/a.py:42` | modify |\n| `docs/b.md` (template) | modify |\n\nprose\n\n| File | Action |\n|---|---|\n| `docs/c.md` | modify |\n",
        )
        .unwrap();
        let files = parse_files_to_modify(&path);
        assert_eq!(
            files,
            BTreeSet::from(["cli/a.py".into(), "docs/b.md".into(), "docs/c.md".into()])
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn high_overlap_classifies_high_and_recommends_supersede() {
        let dir = std::env::temp_dir().join(format!("fno-col-find-{}", crate::claims::now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let candidate = plan_with_files(&dir, "cand.md", &["a.py", "b.py", "c.py", "d.py"]);
        let other = plan_with_files(&dir, "other.md", &["a.py", "b.py", "c.py", "z.py"]);
        let graph = vec![serde_json::json!({
            "id": "ab-other001", "title": "Other", "status": "ready",
            "plan_path": other.to_string_lossy(), "created_at": "2026-01-02",
        })];
        let hits = find_collisions(&candidate, &graph, "ab-cand0001", &Thresholds::default());
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.with_node_id, "ab-other001");
        assert_eq!(hit.recommended_action, Action::Coordinate);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn terminal_and_plan_less_comparators_are_skipped() {
        let dir = std::env::temp_dir().join(format!("fno-col-skip-{}", crate::claims::now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let candidate = plan_with_files(&dir, "cand.md", &["a.py", "b.py", "c.py"]);
        let done_plan = plan_with_files(&dir, "done.md", &["a.py", "b.py", "c.py"]);
        let graph = vec![
            serde_json::json!({
                "id": "ab-done0001", "status": "done",
                "plan_path": done_plan.to_string_lossy(),
            }),
            serde_json::json!({
                "id": "ab-noplan01", "status": "ready",
            }),
        ];
        let hits = find_collisions(&candidate, &graph, "ab-cand0001", &Thresholds::default());
        assert!(hits.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acknowledged_collisions_reconcile_only_when_shipped() {
        let graph = vec![
            serde_json::json!({
                "id": "ab-ack00001", "title": "Ack",
                "collisions_acknowledged": ["ab-shipped", "absent-node", "__skipped_check__"],
            }),
            serde_json::json!({"id": "ab-shipped", "title": "Shipped", "status": "done"}),
            serde_json::json!({"id": "ab-open0001", "title": "Open", "status": "ready"}),
        ];
        let rows = find_acknowledged_collisions(&graph);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node_id, "ab-ack00001");
        assert_eq!(rows[0].resolved_via, "ab-shipped");
        assert_eq!(rows[0].resolved_via_status, "done");
    }

    #[test]
    fn partition_groups_by_shared_path_and_reports_unevaluated() {
        let items = vec![
            (
                "x".to_string(),
                BTreeSet::from(["a.py".into(), "b.py".into()]),
            ),
            (
                "y".to_string(),
                BTreeSet::from(["b.py".into(), "c.py".into()]),
            ),
            ("z".to_string(), BTreeSet::from(["q.py".into()])),
            ("u".to_string(), BTreeSet::new()),
        ];
        let (groups, unevaluated) = partition(&items, &[]);
        // {x,y} merged by b.py; z is its own group; u is a singleton group
        // as well (python: "an unevaluated item is a singleton group as
        // well") AND carries the unevaluated verdict.
        assert_eq!(groups.len(), 3);
        let merged: BTreeSet<String> = groups.iter().find(|g| g.contains("x")).unwrap().clone();
        assert_eq!(merged, BTreeSet::from(["x".into(), "y".into()]));
        assert_eq!(unevaluated, BTreeSet::from(["u".into()]));
    }

    #[test]
    fn shared_hidden_roots_group_disjoint_paths() {
        let items = vec![
            ("x".to_string(), BTreeSet::from(["docs/a.md".into()])),
            ("y".to_string(), BTreeSet::from(["docs/b.md".into()])),
        ];
        let (groups, unevaluated) = partition(&items, HIDDEN_SHARED_OUTPUT_ROOTS);
        assert!(unevaluated.is_empty());
        assert_eq!(groups.len(), 1);
    }
}
