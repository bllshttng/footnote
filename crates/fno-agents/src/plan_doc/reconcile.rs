//! The reconcile-status sweep, ported 1:1 from `cli/src/fno/plan/reconcile_status.py`.
//!
//! Three tiers over every `*.md` in a plans dir: Tier 1 rewrites drift
//! synonyms, Tier 2 classifies blank/unknown tokens by the linked node's
//! closed-ness, Tier 3 recomputes a canonical-but-stale status from the node's
//! derived status (forward-only). Dry-run unless `apply`. The Python sweep's
//! injectable `signal_for`/`status_map` test seams are gone: the keeper builds
//! the status map and hands it in as data, so an empty map IS absent evidence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;

use super::codec::{self, Fields, Value};
use super::lock::PlanDocLock;
use super::status::project_plan_status;

/// Tier 1: pure synonym rewrite (no node signal needed). The sweep touches
/// plan `status:` only and never writes the graph.
const TIER1: &[(&str, &str)] = &[
    ("designed", "design"),
    ("draft", "design"),
    ("planned", "design"),
    ("pending", "design"),
    ("ready-for-blueprint", "design"),
    ("design-locked", "ready"),
    ("reviewing", "in_review"),
    ("shipping", "in_review"),
    ("superseded-by-implementation", "superseded"),
];

fn front_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)\A(---\n)(?P<fm>.*?)(\n---)(?P<rest>.*)\z").expect("front regex")
    })
}

fn status_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^(?P<indent>[ \t]*)status[ \t]*:.*$").expect("status regex"))
}

fn done_at_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^[ \t]*done_at[ \t]*:.*$").expect("done_at regex"))
}

/// A raw frontmatter value as a scalar token, or None when the value is not a
/// scalar (a list/mapping status is Tier-2 drift either way).
fn scalar_of(raw: Option<&Value>) -> Option<&str> {
    match raw {
        Some(Value::Scalar(s)) => Some(s),
        _ => None,
    }
}

/// Bare lowercase token from a raw frontmatter status value.
fn norm(raw: Option<&Value>) -> String {
    scalar_of(raw)
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .to_lowercase()
}

fn is_known(s: &str) -> bool {
    super::status::known_statuses().iter().any(|k| *k == s)
}

/// The node id a plan links to: `node`, then `claims`, then `graph_node_id`
/// (first non-empty wins). A one-item list unwraps; any other shape reads as
/// unlinked, since no single node owns the plan's status.
pub fn plan_link_id(fields: &Fields) -> Option<String> {
    for key in ["node", "claims", "graph_node_id"] {
        let Some(v) = fields.get(key) else {
            continue;
        };
        let empty = match v {
            Value::Scalar(s) => s.is_empty(),
            Value::List(l) | Value::BlockList(l) => l.is_empty(),
            Value::Raw(_) => false,
        };
        if empty {
            continue;
        }
        return match v {
            Value::Scalar(s) => Some(s.clone()),
            Value::List(l) | Value::BlockList(l) if l.len() == 1 => Some(l[0].clone()),
            _ => None,
        };
    }
    None
}

/// Canonical status a drifted `raw` should become, or None to leave it alone.
/// `signal` is the linked node's closed-ness, evaluated only for Tier 2.
pub fn target_status(raw: Option<&Value>, signal: bool) -> Option<String> {
    let s = norm(raw);
    if is_known(&s) {
        return None; // already canonical - the sweep corrects drift only
    }
    if let Some((_, to)) = TIER1.iter().find(|(from, _)| *from == s) {
        return Some((*to).to_string());
    }
    // Tier 2: blank or any unrecognized token.
    Some(if signal { "done" } else { "superseded" }.to_string())
}

/// Return *text* with the frontmatter `status:` scalar set to *new_status*:
/// the first frontmatter block only, the first `status:` line rewritten
/// (or the key inserted as the first frontmatter line), double-quoted and
/// single-line, the body byte-for-byte unchanged. None when there is no
/// parseable frontmatter block.
pub fn rewrite_status(text: &str, new_status: &str) -> Option<String> {
    let caps = front_re().captures(text)?;
    let fm = &caps["fm"];
    let line = format!("status: \"{new_status}\"");
    let new_fm = if status_line_re().is_match(fm) {
        status_line_re()
            .replacen(fm, 1, |c: &regex::Captures| {
                format!("{}{}", &c["indent"], line)
            })
            .into_owned()
    } else if fm.is_empty() {
        line
    } else {
        format!("{line}\n{fm}")
    };
    Some(format!(
        "{}{}{}{}",
        &caps[1], new_fm, &caps[3], &caps["rest"]
    ))
}

/// Append a `done_at: "<ts>"` line to the frontmatter if absent (first-write
/// only): a sweep that promotes a plan to `done` must stamp the completion
/// timestamp. Byte-preserving.
pub fn ensure_done_at(text: &str, ts: &str) -> String {
    let Some(caps) = front_re().captures(text) else {
        return text.to_string();
    };
    let fm = &caps["fm"];
    if done_at_line_re().is_match(fm) {
        return text.to_string();
    }
    let line = format!("done_at: \"{ts}\"");
    let new_fm = if fm.is_empty() {
        line
    } else {
        format!("{fm}\n{line}")
    };
    format!("{}{}{}{}", &caps[1], new_fm, &caps[3], &caps["rest"])
}

/// Canonical-but-stale -> the node's forward projection, or None to leave it.
/// An empty status map disables Tier 3 (never rewrite on absent evidence). An
/// unlinked plan is skipped; a link resolving to no node in a readable map is
/// treated as unlinked and warned.
fn tier3_target(
    fields: &Fields,
    current: &str,
    status_map: &HashMap<String, String>,
    warnings: &mut Vec<String>,
    name: &str,
) -> Option<String> {
    if status_map.is_empty() {
        return None;
    }
    let link = plan_link_id(fields)?;
    match status_map.get(&link) {
        Some(node_status) => project_plan_status(Some(current), node_status),
        None => {
            warnings.push(format!(
                "tier3 skip (link {link} not in graph or archive): {name}"
            ));
            None
        }
    }
}

/// Counters and the change list for one sweep pass.
#[derive(Default)]
pub struct SweepResult {
    /// Rewritten to a non-terminal canonical status.
    pub normalized: u32,
    /// Rewritten to `superseded`.
    pub superseded: u32,
    /// Already canonical, no frontmatter, unparseable, or locked.
    pub skipped: u32,
    /// Drift token left alone because no node status was available.
    pub stood_down: u32,
    /// (path, old, new) per rewrite.
    pub changes: Vec<(String, String, String)>,
    pub warnings: Vec<String>,
}

impl SweepResult {
    /// A stand-down is reported separately: folded into `skipped` it would be
    /// shaped exactly like a healthy idempotent run, which is how a wedged
    /// graph reads as "nothing to do" to the unattended --apply callers.
    pub fn summary(&self) -> String {
        let base = format!(
            "{} normalized, {} superseded, {} skipped",
            self.normalized, self.superseded, self.skipped
        );
        if self.stood_down > 0 {
            format!("{base}, {} stood down", self.stood_down)
        } else {
            base
        }
    }
}

/// Scan every `*.md` in *plans_dir*, classify + (if apply) rewrite drift.
/// Each file's read-decide-write runs under its sidecar `PlanDocLock`, so a
/// concurrent projection is never clobbered mid-rewrite.
pub fn sweep(plans_dir: &Path, apply: bool, status_map: &HashMap<String, String>) -> SweepResult {
    let mut res = SweepResult::default();
    if !plans_dir.is_dir() {
        res.warnings
            .push(format!("plans dir not found: {}", plans_dir.display()));
        return res;
    }

    // An empty map is absent evidence: Tier 2 treats "not closed" as a terminal
    // write, so an unreadable graph would stamp `superseded` onto every live
    // drift-token plan - unattended, since both --apply callers discard output.
    // Tier 3 already refuses on the empty map; this gives Tier 2 the same
    // stance. Tier 1 is a pure synonym rewrite and still runs.
    let tier2_blind = status_map.is_empty();
    if tier2_blind {
        res.warnings
            .push("tier2 off (no node status available): drift tokens left as-is".to_string());
    }

    let mut paths: Vec<PathBuf> = std::fs::read_dir(plans_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    paths.sort();

    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let guard = match PlanDocLock::acquire(&path, Duration::from_secs(2)) {
            Ok(g) => g,
            Err(_) => {
                res.skipped += 1;
                res.warnings.push(format!("skip (locked): {name}"));
                continue;
            }
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                res.skipped += 1;
                res.warnings
                    .push(format!("skip (unparseable): {name}: {e}"));
                continue;
            }
        };
        let parsed = match codec::parse_frontmatter(&text) {
            Ok(p) => p,
            Err(e) => {
                res.skipped += 1;
                res.warnings
                    .push(format!("skip (unparseable): {name}: {e}"));
                continue;
            }
        };

        let raw = parsed.fields.get("status");
        let s = norm(raw);
        let new = if is_known(&s) {
            // Tier 3: a canonical status may still be stale vs its node.
            tier3_target(&parsed.fields, &s, status_map, &mut res.warnings, &name)
        } else if tier2_blind && TIER1.iter().all(|(from, _)| *from != s) {
            // Only the signal-gated tier stands down; Tier 1 still runs.
            res.stood_down += 1;
            continue;
        } else {
            let signal = plan_link_id(&parsed.fields)
                .is_some_and(|l| status_map.get(&l).is_some_and(|st| st == "done"));
            target_status(raw, signal)
        };
        let Some(new) = new else {
            res.skipped += 1;
            continue;
        };

        let Some(rewritten) = rewrite_status(&text, &new) else {
            res.skipped += 1;
            res.warnings.push(format!("skip (no frontmatter): {name}"));
            continue;
        };

        // A promotion to `done` must carry a first-write done_at, else later
        // sweeps/projections see done == done and never backfill it.
        let rewritten = if new == "done" {
            ensure_done_at(&rewritten, &super::now_stamp())
        } else {
            rewritten
        };

        if apply {
            if let Err(e) = codec::atomic_write(&path, &rewritten) {
                res.warnings.push(format!("write failed: {name}: {e}"));
                drop(guard);
                continue;
            }
        }
        res.changes.push((
            path.to_string_lossy().into_owned(),
            if s.is_empty() {
                "(none)".to_string()
            } else {
                s
            },
            new.clone(),
        ));
        if new == "superseded" {
            res.superseded += 1;
        } else {
            res.normalized += 1;
        }
    }

    res
}

/// The keeper's `plan_docs op=reconcile_status` handler: builds the status map
/// from the keeper's strict entries plus an optional archive read-through,
/// then runs the sweep. Served by `plan_doc::keeper`'s dispatch.
pub(crate) fn handle_reconcile_status_op(
    state: &crate::graph_keeper::StoreState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, crate::graph_store::StoreError> {
    let Some(plans_dir) = params.get("plans_dir").and_then(serde_json::Value::as_str) else {
        return Err(crate::graph_store::StoreError::Invalid(
            "plan_docs reconcile_status needs plans_dir".to_string(),
        ));
    };
    let apply = params
        .get("apply")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // Strict read: corruption must reach the empty-map stand-down, not be
    // topped up by the archive into a non-empty map that covers no live node.
    // On a read error the map stays empty and the archive is not consulted:
    // absent evidence, not archive-only truth.
    let read = crate::graph_keeper::cached_entries(state, false, true);
    let mut rows: Vec<serde_json::Value> = match &read {
        Ok(e) => e.as_ref().clone(),
        Err(_) => Vec::new(),
    };
    if read.is_ok() {
        if let Some(archive) = params
            .get("archive_path")
            .and_then(serde_json::Value::as_str)
        {
            rows = crate::graph_store::entries_with_archive(&rows, Path::new(archive));
        }
    }
    let status_map: HashMap<String, String> = rows
        .iter()
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?.to_string();
            let status = e.get("status")?.as_str()?.to_string();
            Some((id, status))
        })
        .collect();
    let res = sweep(
        &super::keeper::caller_path(params, plans_dir),
        apply,
        &status_map,
    );
    Ok(serde_json::json!({
        "normalized": res.normalized,
        "superseded": res.superseded,
        "skipped": res.skipped,
        "stood_down": res.stood_down,
        "changes": res.changes.iter().map(|(p, o, n)| serde_json::json!([p, o, n])).collect::<Vec<_>>(),
        "warnings": res.warnings,
        "summary": res.summary(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-plan-reconcile-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `---\nnode: x-1\n{status_line}\n---` + body.
    fn plan_doc(status_line: &str) -> String {
        let fm = if status_line.is_empty() {
            "---\nnode: x-1\n---".to_string()
        } else {
            format!("---\nnode: x-1\n{status_line}\n---")
        };
        format!("{fm}\n# Title\n\nbody text\n")
    }

    fn linked_plan(status: &str, node: &str) -> String {
        format!("---\nnode: {node}\nstatus: {status}\n---\n# T\n\nbody\n")
    }

    fn write_doc(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn map(rows: &[(&str, &str)]) -> HashMap<String, String> {
        rows.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ------------------------------------------------------------------
    // classification (target_status)
    // ------------------------------------------------------------------

    #[test]
    fn tier1_synonyms_rewrite() {
        for (raw, expected) in [
            ("PENDING", "design"),
            ("draft", "design"),
            ("designed", "design"),
            ("design-locked", "ready"),
            ("reviewing", "in_review"),
            ("shipping", "in_review"),
            ("superseded-by-implementation", "superseded"),
        ] {
            assert_eq!(
                target_status(Some(&Value::Scalar(raw.into())), false),
                Some(expected.into())
            );
        }
    }

    #[test]
    fn canonical_status_left_alone() {
        for s in crate::plan_doc::status::known_statuses().iter().copied() {
            assert_eq!(target_status(Some(&Value::Scalar(s.into())), true), None);
        }
    }

    #[test]
    fn tier2_blank_superseded_without_signal() {
        assert_eq!(
            target_status(Some(&Value::Scalar("".into())), false),
            Some("superseded".into())
        );
        assert_eq!(target_status(None, false), Some("superseded".into()));
    }

    #[test]
    fn tier2_done_with_signal() {
        assert_eq!(
            target_status(Some(&Value::Scalar("implemented".into())), true),
            Some("done".into())
        );
        assert_eq!(
            target_status(Some(&Value::Scalar("".into())), true),
            Some("done".into())
        );
    }

    #[test]
    fn quoting_and_case_normalized_before_lookup() {
        assert_eq!(
            target_status(Some(&Value::Scalar("\"design\"".into())), true),
            None
        );
        // `idea` is a canonical rung, so the sweep leaves it; reaching that
        // verdict from "Idea" proves the case fold ran.
        assert_eq!(
            target_status(Some(&Value::Scalar("Idea".into())), false),
            None
        );
    }

    #[test]
    fn stub_is_canonical_not_swept_to_superseded() {
        // A decompose scaffold is left alone, not archived out from under its
        // node: `stub` aliases to `idea`, a known status.
        assert_eq!(
            target_status(Some(&Value::Scalar("stub".into())), false),
            None
        );
        assert_eq!(
            target_status(Some(&Value::Scalar("stub".into())), true),
            None
        );
    }

    // ------------------------------------------------------------------
    // in-place rewrite (rewrite_status / ensure_done_at)
    // ------------------------------------------------------------------

    #[test]
    fn rewrite_replaces_status_line_body_untouched() {
        let original = plan_doc("status: PENDING");
        let out = rewrite_status(&original, "design").unwrap();
        assert!(out.contains("status: \"design\""));
        assert!(!out.contains("PENDING"));
        assert!(out.ends_with("\n# Title\n\nbody text\n"));
    }

    #[test]
    fn rewrite_inserts_status_when_absent() {
        let original = plan_doc("");
        let out = rewrite_status(&original, "superseded").unwrap();
        assert!(out.contains("status: \"superseded\""));
        assert!(out.contains("node: x-1"));
    }

    #[test]
    fn rewrite_returns_none_without_frontmatter() {
        assert_eq!(
            rewrite_status("# just a body\nno frontmatter\n", "design"),
            None
        );
    }

    #[test]
    fn done_promotion_stamps_done_at_once() {
        let text = plan_doc("status: PENDING");
        let rewritten = rewrite_status(&text, "done").unwrap();
        let stamped = ensure_done_at(&rewritten, "2026-10-01T00:00:00Z");
        assert_eq!(stamped.matches("done_at:").count(), 1);
        assert!(stamped.contains("done_at: \"2026-10-01T00:00:00Z\""));
        // Idempotent: a second stamp does not add a second line.
        let again = ensure_done_at(&stamped, "2026-10-01T01:00:00Z");
        assert_eq!(again.matches("done_at:").count(), 1);
    }

    // ------------------------------------------------------------------
    // sweep (end to end over a tmp plans dir)
    // ------------------------------------------------------------------

    #[test]
    fn sweep_dry_run_reports_without_writing() {
        let dir = tmp_dir("dry");
        write_doc(&dir, "a.md", &plan_doc("status: PENDING"));
        let res = sweep(&dir, false, &map(&[]));
        assert_eq!(res.normalized, 1);
        assert!(read_doc(&dir.join("a.md")).contains("PENDING"));
    }

    #[test]
    fn sweep_apply_normalizes_and_summarizes() {
        let dir = tmp_dir("apply");
        write_doc(&dir, "a.md", &plan_doc("status: PENDING"));
        write_doc(&dir, "b.md", &plan_doc(""));
        write_doc(&dir, "c.md", &plan_doc("status: in_review"));
        // x-1 absent from the map: b takes the honest superseded, c warns.
        let res = sweep(&dir, true, &map(&[("x-other", "ready")]));
        assert_eq!(res.summary(), "1 normalized, 1 superseded, 1 skipped");
        assert!(read_doc(&dir.join("a.md")).contains("status: \"design\""));
        assert!(read_doc(&dir.join("b.md")).contains("status: \"superseded\""));
    }

    #[test]
    fn sweep_skips_malformed_body_intact() {
        let dir = tmp_dir("malformed");
        // An indented continuation line is the codec's parse error.
        let bad = "---\nnode: x-1\nstatus: idea\n  wrapped\n---\nbody\n";
        let p = write_doc(&dir, "bad.md", bad);
        let res = sweep(&dir, true, &map(&[]));
        assert_eq!(res.skipped, 1);
        assert_eq!(res.normalized, 0);
        assert_eq!(read_doc(&p), bad);
    }

    #[test]
    fn sweep_idempotent_and_non_regressing() {
        let dir = tmp_dir("idempotent");
        let p = write_doc(&dir, "a.md", &plan_doc(""));
        sweep(&dir, true, &map(&[("x-other", "ready")]));
        assert!(read_doc(&p).contains("status: \"superseded\""));
        // Human re-activates it.
        let reactivated = read_doc(&p).replace("status: \"superseded\"", "status: \"design\"");
        std::fs::write(&p, reactivated).unwrap();
        let res = sweep(&dir, true, &map(&[("x-other", "ready")]));
        assert_eq!(res.normalized, 0);
        assert_eq!(res.superseded, 0);
        assert_eq!(res.skipped, 1);
        assert!(read_doc(&p).contains("status: \"design\""));
    }

    #[test]
    fn sweep_tier2_uses_signal() {
        let dir = tmp_dir("tier2");
        write_doc(&dir, "closed.md", &plan_doc("status: implemented"));
        let res = sweep(&dir, true, &map(&[("x-1", "done")]));
        assert_eq!(res.normalized, 1);
        assert_eq!(res.superseded, 0);
        assert!(read_doc(&dir.join("closed.md")).contains("status: \"done\""));
    }

    // ------------------------------------------------------------------
    // Tier 3: canonical-but-stale -> node projection
    // ------------------------------------------------------------------

    #[test]
    fn tier3_fixes_stale_canonical() {
        let dir = tmp_dir("t3fix");
        let p = write_doc(&dir, "a.md", &linked_plan("design", "x-1"));
        let res = sweep(&dir, true, &map(&[("x-1", "done")]));
        assert_eq!(res.normalized, 1);
        let text = read_doc(&p);
        assert!(text.contains("status: \"done\""));
        assert_eq!(text.matches("done_at:").count(), 1);
    }

    #[test]
    fn tier3_disabled_when_graph_absent() {
        let dir = tmp_dir("t3off");
        let p = write_doc(&dir, "a.md", &linked_plan("design", "x-1"));
        let res = sweep(&dir, true, &map(&[]));
        assert_eq!(res.skipped, 1);
        assert_eq!(res.normalized, 0);
        assert!(read_doc(&p).contains("status: design"));
    }

    #[test]
    fn tier3_forward_only() {
        let dir = tmp_dir("t3fwd");
        let p = write_doc(&dir, "a.md", &linked_plan("shipped", "x-1"));
        let res = sweep(&dir, true, &map(&[("x-1", "in_progress")]));
        assert_eq!(res.skipped, 1);
        assert!(read_doc(&p).contains("status: shipped"));
    }

    #[test]
    fn tier3_unlinked_plan_skipped() {
        let dir = tmp_dir("t3unlinked");
        let p = write_doc(&dir, "a.md", "---\nstatus: design\n---\n# T\n\nbody\n");
        let res = sweep(&dir, true, &map(&[("x-1", "done")]));
        assert_eq!(res.skipped, 1);
        assert!(read_doc(&p).contains("status: design"));
    }

    #[test]
    fn tier3_link_missing_from_graph_warns() {
        let dir = tmp_dir("t3ghost");
        write_doc(&dir, "a.md", &linked_plan("design", "x-ghost"));
        let res = sweep(&dir, true, &map(&[("x-1", "done")]));
        assert_eq!(res.skipped, 1);
        assert!(res.warnings.iter().any(|w| w.contains("x-ghost")));
    }

    // ------------------------------------------------------------------
    // link normalization (plan_link_id)
    // ------------------------------------------------------------------

    #[test]
    fn plan_link_id_unwraps_single_element_list() {
        let f = |kv: &str| fields_of(kv);
        assert_eq!(
            plan_link_id(&f("claims: [t-1d91]")),
            Some("t-1d91".to_string())
        );
        assert_eq!(
            plan_link_id(&f("node: [t-aa95]")),
            Some("t-aa95".to_string())
        );
    }

    #[test]
    fn plan_link_id_prefers_node_then_claims_then_graph_node_id() {
        // codex PR#149: plans link the node via `claims:` or `node:`; a
        // truthy `node` outranks a later key.
        assert_eq!(
            plan_link_id(&fields_of("node: x-a\nclaims: x-closed")),
            Some("x-a".to_string())
        );
        assert_eq!(
            plan_link_id(&fields_of("claims: x-closed\ngraph_node_id: x-old")),
            Some("x-closed".to_string())
        );
    }

    #[test]
    fn plan_link_id_returns_none_for_unusable_link_shapes() {
        assert_eq!(plan_link_id(&fields_of("claims: [t-1d91, t-aa95]")), None);
        assert_eq!(plan_link_id(&fields_of("claims: []")), None);
        assert_eq!(plan_link_id(&fields_of("claims:")), None);
    }

    #[test]
    fn a_mapping_link_reads_as_unlinked() {
        // An opaque nested block (claims with child lines) is truthy but not a
        // string: unlinked, matching the never-rewrite-on-ambiguous-evidence
        // stance.
        let text = "---\nclaims:\n  id: x-1\nstatus: design\n---\nbody\n";
        let parsed = codec::parse_frontmatter(text).unwrap();
        assert_eq!(plan_link_id(&parsed.fields), None);
    }

    // ------------------------------------------------------------------
    // absent evidence must not manufacture a terminal status
    // ------------------------------------------------------------------

    #[test]
    fn tier2_stands_down_when_no_node_status_is_available() {
        let dir = tmp_dir("standdown");
        let p = write_doc(&dir, "a.md", &linked_plan("implemented", "x-1"));
        let before = read_doc(&p);
        let res = sweep(&dir, true, &map(&[]));
        assert_eq!(res.superseded, 0);
        assert_eq!(res.stood_down, 1);
        assert_eq!(res.skipped, 0);
        assert!(res.summary().contains("1 stood down"));
        assert_eq!(read_doc(&p), before);
        assert!(res.warnings.iter().any(|w| w.contains("tier2 off")));
    }

    #[test]
    fn tier1_synonyms_still_rewrite_without_a_graph() {
        let dir = tmp_dir("t1nograph");
        let p = write_doc(&dir, "a.md", &linked_plan("draft", "x-1"));
        let res = sweep(&dir, true, &map(&[]));
        assert_eq!(res.normalized, 1);
        assert!(read_doc(&p).contains("status: \"design\""));
    }

    #[test]
    fn sweep_leaves_both_vocabularies_untouched() {
        // Mid-migration: one doc on each spelling. Both are known, so
        // the sweep rewrites neither - a retired spelling is valid input, not
        // drift.
        let dir = tmp_dir("vocab");
        let old = write_doc(&dir, "old.md", &plan_doc("status: shipped"));
        let new = write_doc(&dir, "new.md", &plan_doc("status: in_review"));
        let before = (read_doc(&old), read_doc(&new));
        let res = sweep(&dir, true, &map(&[]));
        assert_eq!(res.skipped, 2);
        assert_eq!(res.normalized, 0);
        assert_eq!(res.superseded, 0);
        assert_eq!((read_doc(&old), read_doc(&new)), before);
    }

    #[test]
    fn body_rule_dash_does_not_end_the_frontmatter() {
        let dir = tmp_dir("dashrule");
        let p = write_doc(
            &dir,
            "a.md",
            "---\nnode: x-1\nstatus: PENDING\n---\n# T\n\n---\n\nbody\n",
        );
        let res = sweep(&dir, true, &map(&[("x-1", "done")]));
        assert_eq!(res.normalized, 1);
        assert!(read_doc(&p).contains("status: \"design\""));
    }

    fn read_doc(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    fn fields_of(kv: &str) -> Fields {
        codec::parse_frontmatter(&format!("---\n{kv}\n---\n"))
            .unwrap()
            .fields
    }
}
