//! The graph-to-doc converger, ported 1:1 from `cli/src/fno/plan/_project.py`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::codec::{self, Value as Fv};
use super::node_accessors as na;
use super::rollup::{self};
use super::status::{canonical_status, project_plan_status};
use super::write_or_warn;

/// Graph-authoritative fields mirrored into frontmatter.
pub const MIRROR_KEYS: &[&str] = &[
    "priority",
    "blocks_everything",
    "blocked_by",
    "tags",
    "project",
    "size",
    "parent",
    "parent_slug",
    "parent_link",
    "related_links",
];

/// Mirror keys that are always lists; an empty list is meaningful (it clears a
/// stale mirror).
pub const LIST_MIRROR_KEYS: &[&str] = &["blocked_by", "tags"];

/// Mirror keys whose graph value can legitimately be cleared to None. For
/// these, an explicit None means "clear the stale doc mirror", not "skip".
/// Parent and link mirrors clear when their graph relation or target disappears.
pub const CLEARABLE_KEYS: &[&str] = &[
    "size",
    "parent",
    "parent_slug",
    "parent_link",
    "related_links",
];

/// One per-node projection. `mirror_keys` opts the NAMED node into writing
/// extra keys beyond MIRROR_KEYS; `clear_keys` names keys to delete; `force`
/// names the legitimate backward moves (unsupersede, reopen).
pub struct ProjectOpts<'a> {
    pub mirror_keys: &'a std::collections::BTreeSet<String>,
    pub force_status_off_terminal: bool,
    pub clear_keys: &'a std::collections::BTreeSet<String>,
}

/// Upsert the mirror fields from `node` into `plan_path`'s frontmatter.
/// Returns true when the file was rewritten. Never fails the caller: read
/// failures go to the warnings list. `write_or_warn` serializes the write
/// under the sidecar lock.
pub fn project_node(
    node: &Value,
    plan_path: &Path,
    opts: &ProjectOpts<'_>,
    warnings: &mut Vec<String>,
) -> bool {
    let (target, mut fields, rest) = match codec::read_plan_file(plan_path) {
        Ok(ok) => ok,
        Err(e) => {
            warnings.push(format!(
                "warning: plan projection skipped, cannot read {}: {e}",
                plan_path.display()
            ));
            return false;
        }
    };
    let mut changed = false;
    let mut keys: Vec<String> = MIRROR_KEYS.iter().map(|s| s.to_string()).collect();
    let mut mirror_opt: Vec<String> = opts.mirror_keys.iter().cloned().collect();
    mirror_opt.sort();
    let mut clear_opt: Vec<String> = opts.clear_keys.iter().cloned().collect();
    clear_opt.sort();
    // Order: MIRROR_KEYS, then sorted opt-ins, then sorted clears, deduped -
    // dict.fromkeys over the Python concatenation.
    for k in mirror_opt.into_iter().chain(clear_opt) {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    for key in &keys {
        if opts.clear_keys.contains(key) {
            if fields.contains_key(key) {
                fields.remove(key);
                changed = true;
            }
            continue;
        }
        let Some(raw) = node.get(key) else {
            continue;
        };
        if raw.is_null() {
            // A clearable key set to None means the graph dropped its value
            // (de-orphan / --size null): remove the stale doc mirror. Any
            // other None is a partial dict and must never clobber the doc.
            if CLEARABLE_KEYS.contains(&key.as_str()) && fields.contains_key(key) {
                fields.remove(key);
                changed = true;
            }
            continue;
        }
        let value = match raw {
            Value::Array(items) => {
                let items: Vec<String> = items.iter().map(na::py_str).collect();
                Fv::List(items)
            }
            v => Fv::Scalar(na::py_str(v)),
        };
        if !fields
            .get(key)
            .is_some_and(|current| mirror_values_equal(current, &value))
        {
            fields.insert(key, value);
            changed = true;
        }
    }

    // Epic rollup counters, the epic `waves_total` summary, and a child's
    // `wave` stratum: computed views, repainted every projection. Present =>
    // write the str form (int counters still serialize bareword); None =>
    // delete if present; absent => leave alone.
    for key in rollup::ROLLUP_KEYS
        .iter()
        .copied()
        .chain(["waves_total", "wave"])
    {
        let Some(raw) = node.get(key) else {
            continue;
        };
        if raw.is_null() {
            if fields.contains_key(key) {
                fields.remove(key);
                changed = true;
            }
            continue;
        }
        let s = na::py_str(raw);
        if fields.get(key) != Some(&Fv::Scalar(s.clone())) {
            fields.insert(key, Fv::Scalar(s));
            changed = true;
        }
    }

    // Heal the docs the old shared-key projection damaged, and ONLY those:
    // the old int was written only on the epic branch, and the value must look
    // like that int.
    let stale_waves = fields.get("waves").cloned();
    if node.get("waves_total").is_some_and(|v| !v.is_null())
        && matches!(&stale_waves, Some(Fv::Scalar(s)) if s.trim().chars().all(|c| c.is_ascii_digit()) && !s.trim().is_empty())
    {
        fields.remove("waves");
        changed = true;
    }

    // Status projection: forward-only, stamps done_at on the terminal write.
    let graph_status = na::s_field(node, "status").map(str::to_string);
    if let Some(graph_status) = graph_status {
        let current_status = match fields.get("status") {
            Some(Fv::Scalar(s)) => Some(s.clone()),
            _ => None,
        };
        if opts.force_status_off_terminal
            && ["superseded", "done"]
                .contains(&canonical_status(current_status.as_deref()).as_str())
            && graph_status != canonical_status(current_status.as_deref())
        {
            let forced = if graph_status == "done" || graph_status == "in_review" {
                graph_status.clone()
            } else {
                "design".to_string()
            };
            if forced != "superseded" && current_status.as_deref() != Some(forced.as_str()) {
                fields.insert("status", Fv::Scalar(forced.clone()));
                changed = true;
                if forced == "done" && !fields.contains_key("done_at") {
                    fields.insert("done_at", Fv::Scalar(super::now_stamp()));
                }
            }
        } else if let Some(projected) =
            project_plan_status(current_status.as_deref(), &graph_status)
        {
            // Count waves, not PRs: a done projection onto a plan with
            // unshipped waves waits, like the graduate gate beside it.
            let held = projected == "done" && super::stamp::unshipped_waves(&fields, plan_path);
            if !held && current_status.as_deref() != Some(projected.as_str()) {
                fields.insert("status", Fv::Scalar(projected.clone()));
                changed = true;
                if projected == "done" && !fields.contains_key("done_at") {
                    fields.insert("done_at", Fv::Scalar(super::now_stamp()));
                }
            }
        }
    }

    if changed {
        write_or_warn(&target, &fields, &rest, warnings);
    }
    changed
}

/// Project each named node's mirror fields onto its linked plan. Best-effort
/// and per-node isolated; returns (docs rewritten, warnings). `root`
/// absolutizes relative plan paths; a relative path with no root is a warning
/// (intake absolutizes, so this is defensive only).
pub fn project_graph_nodes(
    entries: &[Value],
    node_ids: &[String],
    root: Option<&str>,
    mirror_keys_for: Option<(String, Vec<String>)>,
    force_status_off_terminal_for: Option<String>,
    clear_keys_for: Option<(String, Vec<String>)>,
) -> (usize, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for id in node_ids {
        if !id.is_empty() && seen.insert(id.clone()) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        return (0, warnings);
    }

    // Parent-repaint hop: a child mutation also repaints its parent epic's
    // doc so its rollup counters stay live. Walk up two hops with siblings.
    expand_repaint_targets(entries, &mut ids);

    let slug_by_id: std::collections::HashMap<&str, Option<&str>> = entries
        .iter()
        .filter(|n| n.is_object())
        .map(|n| (na::s_field(n, "id").unwrap_or(""), na::s_field(n, "slug")))
        .collect();
    let mut doc_stem_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();

    let mut rewritten = 0usize;
    for nid in &ids {
        let Some(node) = na::find_node(entries, nid) else {
            continue;
        };
        let Some(plan_path) = na::s_field(node, "plan_path") else {
            continue;
        };
        if plan_path.is_empty() {
            continue;
        }
        let mut p = PathBuf::from(plan_path);
        if !p.is_absolute() {
            let Some(root) = root else {
                warnings.push(format!(
                    "warning: plan projection skipped for {nid}: relative plan_path with no root"
                ));
                continue;
            };
            p = PathBuf::from(root).join(p);
        }
        p = codec::resolve_plan_path(&p);
        if !p.is_file() {
            continue;
        }
        let mut augmented =
            with_parent_fields(node, &slug_by_id, entries, root, &mut doc_stem_cache);
        if na::s_field(node, "type") == Some("epic") {
            let r = rollup::compute_rollup(nid, entries);
            let (_map, max_wave) = rollup::compute_waves(nid, entries);
            augmented["children_total"] = json!(r.total.to_string());
            augmented["children_done"] = json!(r.done.to_string());
            augmented["children_in_flight"] = json!(r.in_flight.to_string());
            augmented["children_blocked"] = json!(r.blocked.to_string());
            augmented["progress"] = json!(r.progress);
            augmented["waves_total"] = json!((max_wave + 1).to_string());
        } else {
            for k in rollup::ROLLUP_KEYS.iter().chain(["waves_total"].iter()) {
                augmented[k] = Value::Null;
            }
        }
        // A node's own stratum within its parent epic; None (=> cleared) when
        // it has no epic parent.
        let wave_val: Value = match na::s_field(node, "parent") {
            Some(parent_id) => match na::find_node(entries, parent_id) {
                Some(parent) if na::s_field(parent, "type") == Some("epic") => {
                    let (wave_map, _) = rollup::compute_waves(parent_id, entries);
                    match wave_map.get(nid) {
                        Some(w) => json!(w.to_string()),
                        None => Value::Null,
                    }
                }
                _ => Value::Null,
            },
            None => Value::Null,
        };
        augmented["wave"] = wave_val;

        let mirror_keys = mirror_keys_for
            .as_ref()
            .filter(|(id, _)| id == nid)
            .map(|(_, keys)| keys.iter().cloned().collect())
            .unwrap_or_default();
        let clear_keys = clear_keys_for
            .as_ref()
            .filter(|(id, _)| id == nid)
            .map(|(_, keys)| keys.iter().cloned().collect())
            .unwrap_or_default();
        let force = force_status_off_terminal_for.as_deref() == Some(nid.as_str());
        let opts = ProjectOpts {
            mirror_keys: &mirror_keys,
            force_status_off_terminal: force,
            clear_keys: &clear_keys,
        };
        if project_node(&augmented, &p, &opts, &mut warnings) {
            rewritten += 1;
        }
    }
    (rewritten, warnings)
}

/// Add each projected node's ancestors AND its siblings so a child transition
/// repaints the epic + mission rollup (two hops) and every sibling's derived
/// wave. Order-preserving, deduped; sibling expansion only at hop 0 (at the
/// mission hop the "siblings" are other epics sharing neither strata nor
/// rollup).
fn expand_repaint_targets(entries: &[Value], ids: &mut Vec<String>) {
    let by_id: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter(|n| n.is_object())
        .filter_map(|n| na::s_field(n, "id").map(|id| (id, n)))
        .collect();
    let mut children_by_parent: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for n in entries.iter().filter(|n| n.is_object()) {
        if let (Some(nid), Some(pid)) = (na::s_field(n, "id"), na::s_field(n, "parent")) {
            children_by_parent.entry(pid).or_default().push(nid);
        }
    }
    let mut seen: std::collections::HashSet<String> = ids.iter().cloned().collect();
    let snapshot = ids.clone();
    for nid in &snapshot {
        let mut cur = by_id.get(nid.as_str()).copied();
        let mut hops = 0;
        while let Some(node) = cur {
            if hops >= 2 {
                break;
            }
            let Some(parent) = na::s_field(node, "parent") else {
                break;
            };
            let parent = parent.to_string();
            if seen.insert(parent.clone()) {
                ids.push(parent.clone());
            }
            if hops == 0 {
                for sib in children_by_parent
                    .get(parent.as_str())
                    .into_iter()
                    .flatten()
                    .copied()
                {
                    if seen.insert(sib.to_string()) {
                        ids.push(sib.to_string());
                    }
                }
            }
            cur = by_id.get(parent.as_str()).copied();
            hops += 1;
        }
    }
}

/// A shallow copy with parent-derived fields and links to existing plan files.
fn with_parent_fields(
    node: &Value,
    slug_by_id: &std::collections::HashMap<&str, Option<&str>>,
    entries: &[Value],
    root: Option<&str>,
    doc_stem_cache: &mut std::collections::HashMap<String, Option<String>>,
) -> Value {
    let mut copy = node.clone();
    let Some(obj) = copy.as_object_mut() else {
        return copy;
    };

    let parent_id = na::s_field(node, "parent");
    let slug = parent_id.and_then(|pid| slug_by_id.get(pid).copied().flatten());
    obj.insert(
        "parent_slug".to_string(),
        slug.map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
    );
    let parent_link = parent_id.and_then(|pid| {
        doc_stem_for_id(pid, entries, root, doc_stem_cache)
            .map(|stem| Value::String(format!("[[{stem}|{pid}]]")))
    });
    obj.insert(
        "parent_link".to_string(),
        parent_link.unwrap_or(Value::Null),
    );

    let mut seen = std::collections::HashSet::new();
    let related_links: Vec<String> = node
        .get("related")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| seen.insert(*id))
        .filter_map(|id| {
            doc_stem_for_id(id, entries, root, doc_stem_cache)
                .map(|stem| format!("[[{stem}|{id}]]"))
        })
        .collect();
    obj.insert(
        "related_links".to_string(),
        if related_links.is_empty() {
            Value::Null
        } else {
            json!(related_links)
        },
    );
    copy
}

/// Return the stem for an existing linked plan, resolving relative paths at the projection root.
fn doc_stem_for_id(
    node_id: &str,
    entries: &[Value],
    root: Option<&str>,
    cache: &mut std::collections::HashMap<String, Option<String>>,
) -> Option<String> {
    if let Some(cached) = cache.get(node_id) {
        return cached.clone();
    }
    let stem = na::find_node(entries, node_id)
        .and_then(|node| na::s_field(node, "plan_path"))
        .and_then(|plan_path| {
            let path = PathBuf::from(plan_path);
            let path = if path.is_absolute() {
                path
            } else {
                PathBuf::from(root?).join(path)
            };
            let path = codec::resolve_plan_path(&path);
            if !path.is_file() {
                return None;
            }
            path.file_stem()?.to_str().map(str::to_string)
        });
    cache.insert(node_id.to_string(), stem.clone());
    stem
}

/// List formatting is presentation; equal list values keep their existing YAML form.
fn mirror_values_equal(current: &Fv, projected: &Fv) -> bool {
    match (current, projected) {
        (Fv::List(a) | Fv::BlockList(a), Fv::List(b) | Fv::BlockList(b)) => a == b,
        _ => current == projected,
    }
}

#[cfg(test)]
mod tests {
    use super::codec::Fields;
    use super::*;

    const PLAN: &str = "---\nnode: x-child\nstatus: ready\npriority: p2\nsize: M\ntype: feature\nkill_criteria:\n  - name: iteration_ceiling\n    predicate: iteration > 15\n    reason: too many\n---\n\n# child plan\n";

    const EPIC_PLAN: &str = "---\nnode: x-epic\nstatus: ready\ntype: epic\n---\n\n# epic plan\n";

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-plan-doc-project-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_plan(dir: &PathBuf, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn fields_of(path: &PathBuf) -> Fields {
        codec::read_plan_file(path).unwrap().1
    }

    fn s<'a>(f: &'a Fields, key: &str) -> &'a str {
        match f.get(key) {
            Some(Fv::Scalar(s)) => s,
            other => panic!("expected scalar {key}, got {other:?}"),
        }
    }

    fn list<'a>(f: &'a Fields, key: &str) -> &'a [String] {
        match f.get(key) {
            Some(Fv::List(items)) | Some(Fv::BlockList(items)) => items,
            other => panic!("expected list {key}, got {other:?}"),
        }
    }

    fn project(entries: Vec<Value>, ids: &[&str], root: &PathBuf) -> usize {
        let id_strings: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        let (rewritten, warnings) = project_graph_nodes(
            &entries,
            &id_strings,
            Some(root.to_str().unwrap()),
            None,
            None,
            None,
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        rewritten
    }

    fn child_doc(nid: &str) -> String {
        format!("---\nnode: {nid}\nstatus: ready\ntype: feature\n---\n\n# {nid}\n")
    }

    #[test]
    fn slug_rows() {
        let dir = tmp_dir("slug");
        let plan = write_plan(&dir, "child.md", PLAN);
        let entries = vec![
            json!({"id": "x-epic", "slug": "the-epic", "plan_path": null}),
            json!({"id": "x-child", "slug": "the-child", "plan_path": plan.to_string_lossy(), "priority": "p0", "parent": "x-epic", "size": "M", "status": "ready"}),
        ];
        assert_eq!(project(entries, &["x-child"], &dir), 1);
        let f = fields_of(&plan);
        assert_eq!(s(&f, "priority"), "p0");
        assert_eq!(s(&f, "parent"), "x-epic");
        assert_eq!(s(&f, "parent_slug"), "the-epic");
        assert_eq!(s(&f, "size"), "M");

        let dir = tmp_dir("links");
        write_plan(
            &dir,
            "20260801-epic-parent.md",
            &EPIC_PLAN.replace("x-epic", "x-parent"),
        );
        let related_a = write_plan(&dir, "20260802-related-a.md", &child_doc("x-a"));
        let related_b = write_plan(&dir, "20260803-related-b.md", &child_doc("x-b"));
        let child = write_plan(
            &dir,
            "child.md",
            "---\nnode: x-child\nstatus: ready\ntype: feature\nrelated: [manual, note]\n---\n\n# child\n",
        );
        let entries = vec![
            json!({"id": "x-parent", "slug": "parent-slug", "plan_path": "20260801-epic-parent.md", "type": "epic", "status": "ready"}),
            json!({"id": "x-a", "slug": "related-a", "plan_path": related_a.to_string_lossy(), "type": "feature", "status": "ready"}),
            json!({"id": "x-b", "slug": "related-b", "plan_path": related_b.to_string_lossy(), "type": "feature", "status": "ready"}),
            json!({"id": "x-missing", "slug": "missing", "plan_path": null}),
            json!({"id": "x-child", "slug": "child", "plan_path": child.to_string_lossy(), "parent": "x-parent", "related": ["x-a", "x-missing", "x-b", "x-a"], "status": "ready", "priority": "p2", "type": "feature"}),
        ];

        assert!(project(entries, &["x-child"], &dir) > 0);
        let f = fields_of(&child);
        assert_eq!(s(&f, "parent"), "x-parent");
        assert_eq!(s(&f, "parent_slug"), "parent-slug");
        assert_eq!(s(&f, "parent_link"), "[[20260801-epic-parent|x-parent]]");
        assert_eq!(
            list(&f, "related_links"),
            &[
                "[[20260802-related-a|x-a]]".to_string(),
                "[[20260803-related-b|x-b]]".to_string(),
            ]
        );
        assert_eq!(
            list(&f, "related"),
            &["manual".to_string(), "note".to_string()]
        );

        let dir = tmp_dir("group-fragment-links");
        write_plan(&dir, "parent.md", &child_doc("x-parent"));
        let child = write_plan(&dir, "child.md", &child_doc("x-child"));
        let entries = vec![
            json!({"id": "x-parent", "slug": "parent", "plan_path": "parent.md#group-parent", "status": "ready", "type": "feature"}),
            json!({"id": "x-child", "slug": "child", "plan_path": "child.md#group-child", "parent": "x-parent", "status": "ready", "type": "feature"}),
        ];

        assert_eq!(project(entries, &["x-child"], &dir), 1);
        assert_eq!(s(&fields_of(&child), "parent_link"), "[[parent|x-parent]]");

        let dir = tmp_dir("dangling");
        let plan = write_plan(&dir, "child.md", PLAN);
        let entries = vec![
            json!({"id": "x-child", "slug": "the-child", "plan_path": plan.to_string_lossy(), "priority": "p1", "parent": "x-gone"}),
        ];
        assert_eq!(project(entries, &["x-child"], &dir), 1);
        let f = fields_of(&plan);
        assert_eq!(s(&f, "parent"), "x-gone");
        assert!(!f.contains_key("parent_slug"));

        let dir = tmp_dir("noplan");
        let entries = vec![json!({"id": "x-a", "slug": "a", "plan_path": null, "priority": "p0"})];
        assert_eq!(project(entries, &["x-a"], &dir), 0);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        let dir = tmp_dir("rel");
        write_plan(&dir, "rel.md", PLAN);
        let entries =
            vec![json!({"id": "x-r", "slug": "r", "plan_path": "rel.md", "priority": "p0"})];
        assert_eq!(project(entries, &["x-r"], &dir), 1);
    }

    #[test]
    fn link_rows() {
        let dir = tmp_dir("stale-links");
        let child = write_plan(
            &dir,
            "child.md",
            "---\nnode: x-child\nstatus: ready\ntype: feature\nparent_link: \"[[old-parent|x-parent]]\"\nrelated_links: [\"[[old-related|x-related]]\"]\n---\n\n# child\n",
        );
        let entries = vec![
            json!({"id": "x-parent", "slug": "parent", "plan_path": null, "type": "feature"}),
            json!({"id": "x-related", "slug": "related", "plan_path": null, "type": "feature"}),
            json!({"id": "x-child", "slug": "child", "plan_path": child.to_string_lossy(), "parent": "x-parent", "related": ["x-related"], "status": "ready", "type": "feature"}),
        ];

        assert_eq!(project(entries, &["x-child"], &dir), 1);
        let f = fields_of(&child);
        assert!(!f.contains_key("parent_link"));
        assert!(!f.contains_key("related_links"));

        let dir = tmp_dir("block-links");
        let parent = write_plan(&dir, "parent.md", &child_doc("x-parent"));
        let related = write_plan(&dir, "20260802-related.md", &child_doc("x-related"));
        let child = write_plan(&dir, "child.md", &child_doc("x-child"));
        let entries = vec![
            json!({"id": "x-parent", "slug": "parent", "plan_path": parent.to_string_lossy(), "status": "ready", "type": "feature"}),
            json!({"id": "x-related", "slug": "related", "plan_path": related.to_string_lossy(), "status": "ready", "type": "feature"}),
            json!({"id": "x-child", "slug": "child", "plan_path": child.to_string_lossy(), "parent": "x-parent", "related": ["x-related"], "status": "ready", "type": "feature"}),
        ];

        assert_eq!(project(entries.clone(), &["x-child"], &dir), 1);
        assert_eq!(
            list(&fields_of(&child), "related_links"),
            &["[[20260802-related|x-related]]".to_string()]
        );

        let current = std::fs::read_to_string(&child).unwrap();
        assert_eq!(project(entries.clone(), &["x-child"], &dir), 0);
        assert_eq!(std::fs::read_to_string(&child).unwrap(), current);

        let mut found = false;
        let mut lines = Vec::new();
        for line in current.lines() {
            if line.starts_with("related_links:") {
                lines.push("related_links:".to_string());
                lines.push("  - \"[[20260802-related|x-related]]\"".to_string());
                found = true;
            } else {
                lines.push(line.to_string());
            }
        }
        assert!(
            found,
            "projection must write related_links before formatting"
        );
        std::fs::write(&child, format!("{}\n", lines.join("\n"))).unwrap();
        let block_form = std::fs::read_to_string(&child).unwrap();

        assert_eq!(project(entries, &["x-child"], &dir), 0);
        assert_eq!(std::fs::read_to_string(&child).unwrap(), block_form);

        let dir = tmp_dir("single-quoted-list");
        let child = write_plan(
            &dir,
            "child.md",
            "---\nnode: x-child\nstatus: ready\ntype: feature\npriority: p2\nclose_probes: ['grep -c ''cargo admission: taking over'' \"$(command -v fno-agents)\"']\n---\n\n# child\n",
        );
        let entries = vec![
            json!({"id": "x-child", "slug": "child", "plan_path": child.to_string_lossy(), "priority": "p0", "status": "ready", "type": "feature"}),
        ];

        assert_eq!(project(entries, &["x-child"], &dir), 1);
        let f = fields_of(&child);
        assert_eq!(
            list(&f, "close_probes"),
            &[r#"grep -c 'cargo admission: taking over' "$(command -v fno-agents)""#.to_string()]
        );

        let dir = tmp_dir("isolated");
        let good = write_plan(&dir, "good.md", PLAN);
        let entries = vec![
            json!({"id": "x-gone", "slug": "g", "plan_path": dir.join("missing.md").to_string_lossy(), "priority": "p0"}),
            json!({"id": "x-good", "slug": "g2", "plan_path": good.to_string_lossy(), "priority": "p0"}),
        ];
        assert_eq!(project(entries, &["x-gone", "x-good"], &dir), 1);
        assert_eq!(s(&fields_of(&good), "priority"), "p0");

        let dir = tmp_dir("empty");
        assert_eq!(project(vec![], &[], &dir), 0);

        let dir = tmp_dir("idem");
        let plan = write_plan(&dir, "child.md", PLAN);
        let entries = vec![
            json!({"id": "x-c", "slug": "c", "plan_path": plan.to_string_lossy(), "priority": "p0", "status": "ready"}),
        ];
        assert_eq!(project(entries.clone(), &["x-c"], &dir), 1);
        assert_eq!(project(entries, &["x-c"], &dir), 0);
    }

    #[test]
    fn rollup_rows() {
        let dir = tmp_dir("rollup");
        let epic_plan = write_plan(&dir, "epic.md", EPIC_PLAN);
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_plan.to_string_lossy(), "status": "ready"}),
            json!({"id": "c1", "parent": "x-epic", "status": "done"}),
            json!({"id": "c2", "parent": "x-epic", "status": "ready"}),
        ];
        assert_eq!(project(entries.clone(), &["x-epic"], &dir), 1);
        let f = fields_of(&epic_plan);
        assert_eq!(s(&f, "children_total"), "2");
        assert_eq!(s(&f, "children_done"), "1");
        assert_eq!(s(&f, "progress"), "1/2");
        assert_eq!(project(entries, &["x-epic"], &dir), 0);

        let dir = tmp_dir("repaint");
        let epic_plan = write_plan(&dir, "epic.md", EPIC_PLAN);
        let child_plan = write_plan(&dir, "child.md", PLAN);
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_plan.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-child", "slug": "child", "parent": "x-epic", "plan_path": child_plan.to_string_lossy(), "status": "done"}),
        ];
        assert_eq!(project(entries, &["x-child"], &dir), 2);
        let f = fields_of(&epic_plan);
        assert_eq!(s(&f, "children_done"), "1");
        assert_eq!(s(&f, "progress"), "1/1");

        let dir = tmp_dir("leaf");
        let plan = write_plan(&dir, "child.md", PLAN);
        let entries = vec![
            json!({"id": "x-c", "slug": "c", "type": "feature", "plan_path": plan.to_string_lossy(), "priority": "p0", "status": "ready"}),
        ];
        assert_eq!(project(entries, &["x-c"], &dir), 1);
        let f = fields_of(&plan);
        assert_eq!(s(&f, "priority"), "p0");
        assert!(!f.contains_key("children_total"));
        assert!(!f.contains_key("progress"));

        let dir = tmp_dir("waves");
        let epic_doc = write_plan(&dir, "epic.md", EPIC_PLAN);
        let a = write_plan(&dir, "a.md", &child_doc("a"));
        let b = write_plan(&dir, "b.md", &child_doc("b"));
        let d = write_plan(&dir, "d.md", &child_doc("d"));
        let epic = |p: &PathBuf| json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": p.to_string_lossy(), "status": "ready"});
        let entries = vec![
            epic(&epic_doc),
            json!({"id": "x-a", "slug": "a", "type": "feature", "parent": "x-epic", "plan_path": a.to_string_lossy(), "blocked_by": []}),
            json!({"id": "x-b", "slug": "b", "type": "feature", "parent": "x-epic", "plan_path": b.to_string_lossy(), "blocked_by": ["x-a"]}),
            json!({"id": "x-d", "slug": "d", "type": "feature", "parent": "x-epic", "plan_path": d.to_string_lossy(), "blocked_by": ["x-b"]}),
        ];
        project(entries, &["x-b"], &dir);
        assert_eq!(s(&fields_of(&a), "wave"), "0");
        assert_eq!(s(&fields_of(&b), "wave"), "1");
        assert_eq!(s(&fields_of(&d), "wave"), "2");
        assert_eq!(s(&fields_of(&epic_doc), "waves_total"), "3");

        let dir = tmp_dir("restrat");
        let epic_doc = write_plan(&dir, "epic.md", EPIC_PLAN);
        let a = write_plan(&dir, "a.md", &child_doc("a"));
        let d = write_plan(&dir, "d.md", &child_doc("d"));
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-a", "slug": "a", "type": "feature", "parent": "x-epic", "plan_path": a.to_string_lossy(), "blocked_by": []}),
            json!({"id": "x-d", "slug": "d", "type": "feature", "parent": "x-epic", "plan_path": d.to_string_lossy(), "blocked_by": ["x-a"]}),
        ];
        project(entries.clone(), &["x-d"], &dir);
        assert_eq!(s(&fields_of(&d), "wave"), "1");
        let entries = vec![
            entries[0].clone(),
            entries[1].clone(),
            json!({"id": "x-d", "slug": "d", "type": "feature", "parent": "x-epic", "plan_path": d.to_string_lossy(), "blocked_by": []}),
        ];
        project(entries, &["x-d"], &dir);
        assert_eq!(s(&fields_of(&d), "wave"), "0");
        assert_eq!(s(&fields_of(&epic_doc), "waves_total"), "1");

        let dir = tmp_dir("orphan");
        let mut seeded = child_doc("c");
        seeded.push_str("wave: 2\n");
        let child_doc_path = write_plan(&dir, "child.md", &seeded);
        let epic_doc = write_plan(&dir, "epic.md", EPIC_PLAN);
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": child_doc_path.to_string_lossy(), "blocked_by": []}),
        ];
        project(entries.clone(), &["x-c"], &dir);
        assert_eq!(s(&fields_of(&child_doc_path), "wave"), "0");
        let entries = vec![
            entries[0].clone(),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": Value::Null, "plan_path": child_doc_path.to_string_lossy(), "blocked_by": []}),
        ];
        project(entries, &["x-c"], &dir);
        assert!(!fields_of(&child_doc_path).contains_key("wave"));

        let dir = tmp_dir("demote");
        let epic_doc = write_plan(&dir, "epic.md", EPIC_PLAN);
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": null, "blocked_by": []}),
        ];
        project(entries.clone(), &["x-epic"], &dir);
        let f = fields_of(&epic_doc);
        assert_eq!(s(&f, "waves_total"), "1");
        assert_eq!(s(&f, "children_total"), "1");
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "feature", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": null, "blocked_by": []}),
        ];
        project(entries, &["x-epic"], &dir);
        let f = fields_of(&epic_doc);
        assert!(!f.contains_key("waves_total"));
        assert!(!f.contains_key("children_total"));
        assert!(!f.contains_key("progress"));
    }

    const WAVES_BLOCK: &str =
        "waves:\n  - wave: 1\n    mode: parallel\n  - wave: 2\n    mode: sequential\n";

    #[test]
    fn wave_rows() {
        let dir = tmp_dir("authored");
        let plan = write_plan(
            &dir,
            "child.md",
            &PLAN.replace("---\n\n", &format!("{WAVES_BLOCK}---\n\n")),
        );
        let entries = vec![
            json!({"id": "x-c", "slug": "c", "type": "feature", "plan_path": plan.to_string_lossy(), "priority": "p0", "status": "ready"}),
        ];
        assert_eq!(project(entries, &["x-c"], &dir), 1);
        let text = std::fs::read_to_string(&plan).unwrap();
        assert!(text.contains("wave: 1") && text.contains("wave: 2"));
        assert_eq!(s(&fields_of(&plan), "priority"), "p0");

        let dir = tmp_dir("heal");
        let epic_doc = write_plan(
            &dir,
            "epic.md",
            &EPIC_PLAN.replace("---\n\n", "waves: 3\n---\n\n"),
        );
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": null, "blocked_by": []}),
        ];
        project(entries, &["x-epic"], &dir);
        let f = fields_of(&epic_doc);
        assert!(!f.contains_key("waves"));
        assert_eq!(s(&f, "waves_total"), "1");

        let dir = tmp_dir("heal2");
        let plan = write_plan(
            &dir,
            "child.md",
            &PLAN.replace("---\n\n", "waves: 7\n---\n\n"),
        );
        let entries = vec![
            json!({"id": "x-c", "slug": "c", "type": "feature", "plan_path": plan.to_string_lossy(), "priority": "p0", "status": "ready"}),
        ];
        assert_eq!(project(entries, &["x-c"], &dir), 1);
        assert_eq!(s(&fields_of(&plan), "waves"), "7");

        let dir = tmp_dir("heal3");
        let epic_doc = write_plan(
            &dir,
            "epic.md",
            &EPIC_PLAN.replace("---\n\n", "waves: tbd\n---\n\n"),
        );
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": null, "blocked_by": []}),
        ];
        project(entries, &["x-epic"], &dir);
        assert_eq!(s(&fields_of(&epic_doc), "waves"), "tbd");

        let dir = tmp_dir("epiclist");
        let epic_doc = write_plan(
            &dir,
            "epic.md",
            &EPIC_PLAN.replace("---\n\n", &format!("{WAVES_BLOCK}---\n\n")),
        );
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": epic_doc.to_string_lossy(), "status": "ready"}),
            json!({"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": null, "blocked_by": []}),
        ];
        assert_eq!(project(entries, &["x-epic"], &dir), 1);
        let text = std::fs::read_to_string(&epic_doc).unwrap();
        assert!(text.contains("wave: 1") && text.contains("wave: 2"));
        assert_eq!(s(&fields_of(&epic_doc), "waves_total"), "1");
    }

    #[test]
    fn mirror_rows() {
        let dir = tmp_dir("scope");
        let me = write_plan(
            &dir,
            "me.md",
            &child_doc("me").replace("type: feature", "type: bug"),
        );
        let sib = write_plan(
            &dir,
            "sib.md",
            &child_doc("sib").replace("type: feature", "type: bug"),
        );
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": null, "status": "ready"}),
            json!({"id": "x-me", "slug": "me", "type": "epic", "parent": "x-epic", "plan_path": me.to_string_lossy(), "blocked_by": [], "status": "ready"}),
            json!({"id": "x-sib", "slug": "sib", "type": "feature", "parent": "x-epic", "plan_path": sib.to_string_lossy(), "blocked_by": [], "status": "ready"}),
        ];
        let (rewritten, _w) = project_graph_nodes(
            &entries,
            &["x-me".to_string()],
            Some(dir.to_str().unwrap()),
            Some(("x-me".to_string(), vec!["type".to_string()])),
            None,
            None,
        );
        // The named node AND its sibling repaint (sibling fan-out mirrors the
        // parent_slug), but only the named node takes the `type` opt-in.
        assert_eq!(rewritten, 2);
        assert_eq!(s(&fields_of(&me), "type"), "epic");
        assert_eq!(s(&fields_of(&sib), "type"), "bug");

        let dir = tmp_dir("mirror");
        let plan = write_plan(&dir, "plan.md", PLAN);
        let node = json!({"priority": "p1", "type": "feature", "blocked_by": ["x-1", "x-2"], "project": "fno"});
        assert!(direct(&plan, node.clone()));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "priority"), "p1");
        assert_eq!(list(&f, "blocked_by"), ["x-1", "x-2"]);
        assert!(!direct(&plan, node));

        let dir = tmp_dir("noneskip");
        let plan = write_plan(&dir, "plan.md", PLAN);
        let node = json!({"priority": null, "blocked_by": ["x-9"], "project": null});
        assert!(direct(&plan, node));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "priority"), "p2"); // None never overwrites the doc value
        assert!(!f.contains_key("project"));
        assert_eq!(list(&f, "blocked_by"), ["x-9"]);

        let dir = tmp_dir("clearlist");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("size: M", "size: M\nblocked_by: [x-old]"),
        );
        assert!(direct(&plan, one("blocked_by", json!([]))));
        assert_eq!(list(&fields_of(&plan), "blocked_by"), [] as [&str; 0]);

        let dir = tmp_dir("band");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("size: M", "size: M\ndifficulty: high"),
        );
        assert!(!direct(&plan, one("difficulty", Value::Null)));
        assert_eq!(s(&fields_of(&plan), "difficulty"), "high");

        let dir = tmp_dir("cleardiff");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("size: M", "size: M\ndifficulty: high"),
        );
        assert!(direct_full(&plan, json!({}), &[], false, &["difficulty"]));
        assert!(!fields_of(&plan).contains_key("difficulty"));

        let dir = tmp_dir("repaint");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("size: M", "size: M\ndifficulty: medium"),
        );
        assert!(direct(
            &plan,
            json!({"difficulty": "low", "priority": "p1"})
        ));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "difficulty"), "medium");
        assert_eq!(s(&f, "priority"), "p1");

        let dir = tmp_dir("band");
        let me = write_plan(
            &dir,
            "me.md",
            &child_doc("me").replace("type: feature", "type: feature\ndifficulty: medium"),
        );
        let sib = write_plan(&dir, "sib.md", &child_doc("sib"));
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": null, "status": "ready"}),
            json!({"id": "x-me", "slug": "me", "type": "feature", "parent": "x-epic", "plan_path": me.to_string_lossy(), "difficulty": "low", "blocked_by": [], "status": "ready"}),
            json!({"id": "x-sib", "slug": "sib", "type": "feature", "parent": "x-epic", "plan_path": sib.to_string_lossy(), "difficulty": "low", "blocked_by": [], "status": "ready"}),
        ];
        project(entries, &["x-sib"], &dir);
        assert_eq!(s(&fields_of(&me), "difficulty"), "medium");

        let dir = tmp_dir("missing");
        assert!(!direct(
            &dir.join("does-not-exist.md"),
            one("priority", "p0")
        ));

        let dir = tmp_dir("diffopt");
        let me = write_plan(
            &dir,
            "me.md",
            &child_doc("me").replace("type: feature", "type: feature\ndifficulty: high"),
        );
        let sib = write_plan(
            &dir,
            "sib.md",
            &child_doc("sib").replace("type: feature", "type: feature\ndifficulty: high"),
        );
        let entries = vec![
            json!({"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": null, "status": "ready"}),
            json!({"id": "x-me", "slug": "me", "type": "feature", "parent": "x-epic", "plan_path": me.to_string_lossy(), "difficulty": "low", "blocked_by": [], "status": "ready"}),
            json!({"id": "x-sib", "slug": "sib", "type": "feature", "parent": "x-epic", "plan_path": sib.to_string_lossy(), "difficulty": "low", "blocked_by": [], "status": "ready"}),
        ];
        let (rewritten, _w) = project_graph_nodes(
            &entries,
            &["x-me".to_string()],
            Some(dir.to_str().unwrap()),
            Some(("x-me".to_string(), vec!["difficulty".to_string()])),
            None,
            None,
        );
        // The sibling repaints (parent_slug mirror); only the named node takes
        // the difficulty opt-in.
        assert_eq!(rewritten, 2);
        assert_eq!(s(&fields_of(&me), "difficulty"), "low");
        assert_eq!(s(&fields_of(&sib), "difficulty"), "high");
    }

    // ---- project_node direct cases (from test_project_mirror.py) ----

    fn one(key: &str, v: impl serde::Serialize) -> Value {
        let mut m = serde_json::Map::new();
        m.insert(
            key.to_string(),
            serde_json::to_value(v).expect("serializable"),
        );
        Value::Object(m)
    }

    fn direct(plan: &PathBuf, node: Value) -> bool {
        let opts = ProjectOpts {
            mirror_keys: &Default::default(),
            force_status_off_terminal: false,
            clear_keys: &Default::default(),
        };
        let mut warnings = Vec::new();
        project_node(&node, plan, &opts, &mut warnings)
    }

    fn direct_full(
        plan: &PathBuf,
        node: Value,
        mirror: &[&str],
        force: bool,
        clear: &[&str],
    ) -> bool {
        let opts = ProjectOpts {
            mirror_keys: &mirror.iter().map(|s| s.to_string()).collect(),
            force_status_off_terminal: force,
            clear_keys: &clear.iter().map(|s| s.to_string()).collect(),
        };
        let mut warnings = Vec::new();
        project_node(&node, plan, &opts, &mut warnings)
    }

    #[test]
    fn status_rows() {
        let dir = tmp_dir("unowned");
        let plan = write_plan(&dir, "plan.md", PLAN);
        direct(&plan, one("priority", "p3"));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "status"), "ready");
        assert_eq!(s(&f, "node"), "x-child");
        assert!(
            f.contains_key("kill_criteria")
                || std::fs::read_to_string(&plan)
                    .unwrap()
                    .contains("kill_criteria")
        );
        assert!(std::fs::read_to_string(&plan)
            .unwrap()
            .contains("kill_criteria"));

        for doc_status in ["ready", "design"] {
            for graph_status in ["claimed", "in_progress"] {
                let dir = tmp_dir("claim");
                let plan = write_plan(
                    &dir,
                    "plan.md",
                    &PLAN.replace("status: ready", &format!("status: {doc_status}")),
                );
                assert!(!direct(&plan, one("status", graph_status)));
                assert_eq!(s(&fields_of(&plan), "status"), doc_status);
            }
        }

        let dir = tmp_dir("force");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace(
                "status: ready",
                "status: done\ndone_at: 2026-09-01T00:00:00Z",
            ),
        );
        assert!(direct_full(
            &plan,
            one("status", "in_progress"),
            &[],
            true,
            &[]
        ));
        assert_eq!(s(&fields_of(&plan), "status"), "design");

        let dir = tmp_dir("doneat");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("status: ready", "status: shipped"),
        );
        assert!(direct(&plan, one("status", "done")));
        let first = s(&fields_of(&plan), "done_at").to_string();
        assert!(!first.is_empty());
        assert!(!direct(&plan, one("status", "done")));
        assert_eq!(s(&fields_of(&plan), "done_at"), first);

        let dir = tmp_dir("backward");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("status: ready", "status: shipped"),
        );
        assert!(!direct(&plan, one("status", "claimed")));
        assert_eq!(s(&fields_of(&plan), "status"), "shipped");

        let dir = tmp_dir("gated");
        let plan = write_plan(&dir, "plan.md", PLAN);
        for gated in ["blocked", "deferred"] {
            assert!(!direct(&plan, one("status", gated)));
        }
        assert_eq!(s(&fields_of(&plan), "status"), "ready");
        let dir = tmp_dir("waveshold");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace(
                "---\n\n# child plan\n",
                &format!(
                    "---\n\n# child plan\n\n## Execution Strategy\n\n```yaml\nexecution_mode: sequential\nwaves:\n  - wave: 1\n    tasks: ['1.1']\n  - wave: 2\n    tasks: ['2.1']\n  - wave: 3\n    tasks: ['3.1']\ntasks:\n  - id: '1.1'\n    surface: ['a.rs']\n    verify: 'true'\n  - id: '2.1'\n    surface: ['b.rs']\n    verify: 'true'\n  - id: '3.1'\n    surface: ['c.rs']\n    verify: 'true'\n```\n"
                ),
            ),
        );
        // One merged wave is not a done plan: the projection holds.
        assert!(!direct(&plan, one("status", "done")));
        assert_eq!(s(&fields_of(&plan), "status"), "ready");
        // Every wave shipped: the projection lands done with a done_at.
        let (target, mut fields, rest) = codec::read_plan_file(&plan).unwrap();
        let urls = vec!["u1".to_string(), "u2".to_string(), "u3".to_string()];
        fields.insert("urls", codec::Value::List(urls));
        codec::write_plan_file(&target, &fields, &rest).unwrap();
        assert!(direct(&plan, one("status", "done")));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "status"), "done");
        assert!(f.get("done_at").is_some());
    }

    #[test]
    fn scalar_rows() {
        let dir = tmp_dir("sizeparent");
        let plan = write_plan(&dir, "plan.md", &PLAN.replace("size: M", "size: S"));
        assert!(direct(
            &plan,
            json!({"size": "M", "parent": "x-parent", "parent_slug": "epic-slug"})
        ));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "size"), "M");
        assert_eq!(s(&f, "parent"), "x-parent");
        assert_eq!(s(&f, "parent_slug"), "epic-slug");

        let dir = tmp_dir("nullparent");
        let plan = write_plan(&dir, "plan.md", PLAN);
        assert!(direct(&plan, json!({"parent": Value::Null, "size": "L"})));
        let f = fields_of(&plan);
        assert!(!f.contains_key("parent"));
        assert!(!f.contains_key("parent_slug"));
        assert_eq!(s(&f, "size"), "L");

        let dir = tmp_dir("cleared");
        let seeded = PLAN.replace("size: M", "size: M\nparent: x-old\nparent_slug: old-epic");
        let plan = write_plan(&dir, "plan.md", &seeded);
        assert!(direct(
            &plan,
            json!({"parent": Value::Null, "parent_slug": Value::Null, "size": Value::Null})
        ));
        let f = fields_of(&plan);
        assert!(!f.contains_key("parent"));
        assert!(!f.contains_key("parent_slug"));
        assert!(!f.contains_key("size"));

        let dir = tmp_dir("nonclearable");
        let plan = write_plan(
            &dir,
            "plan.md",
            &PLAN.replace("type: feature", "type: feature\npriority: p1"),
        );
        assert!(!direct(&plan, one("priority", Value::Null)));
        assert_eq!(s(&fields_of(&plan), "priority"), "p1");

        let dir = tmp_dir("tags");
        let plan = write_plan(&dir, "plan.md", PLAN);
        assert!(direct(&plan, one("tags", json!(["mux"]))));
        assert_eq!(list(&fields_of(&plan), "tags"), ["mux"]);
        let plan2 = write_plan(
            &dir,
            "plan2.md",
            &PLAN.replace("size: M", "size: M\ntags: [old]"),
        );
        assert!(direct(&plan2, one("tags", json!([]))));
        assert_eq!(list(&fields_of(&plan2), "tags"), [] as [&str; 0]);
    }

    #[test]
    fn type_rows() {
        let dir = tmp_dir("notype");
        let plan = write_plan(&dir, "plan.md", &PLAN.replace("type: feature", "type: bug"));
        let before = std::fs::read_to_string(&plan).unwrap();
        assert!(!direct(&plan, one("type", "feature")));
        assert_eq!(std::fs::read_to_string(&plan).unwrap(), before);

        let dir = tmp_dir("typekeep");
        let plan = write_plan(&dir, "plan.md", &PLAN.replace("type: feature", "type: bug"));
        assert!(direct(
            &plan,
            json!({"type": "feature", "priority": "p1", "blocked_by": ["x-1"], "size": "L"})
        ));
        let f = fields_of(&plan);
        assert_eq!(s(&f, "type"), "bug");
        assert_eq!(s(&f, "priority"), "p1");
        assert_eq!(list(&f, "blocked_by"), ["x-1"]);
        assert_eq!(s(&f, "size"), "L");

        let dir = tmp_dir("noadd");
        let plan = write_plan(&dir, "plan.md", &PLAN.replace("type: feature\n", ""));
        assert!(!direct(&plan, one("type", "epic")));
        assert!(!fields_of(&plan).contains_key("type"));

        let dir = tmp_dir("optintype");
        let plan = write_plan(&dir, "plan.md", &PLAN.replace("type: feature", "type: bug"));
        assert!(direct_full(
            &plan,
            one("type", "epic"),
            &["type"],
            false,
            &[]
        ));
        assert_eq!(s(&fields_of(&plan), "type"), "epic");

        let dir = tmp_dir("finalized");
        let text = "---\nnode: x-node\nstatus: ready\ncreated: 2026-07-08\ndifficulty: medium\nsize: M\ntype: feature\ndone_probes:\n  - fno x --json | grep -oE 'scope: [a-z]+'\n---\n\n# A plan\n\nbody text\n";
        let plan = write_plan(&dir, "plan.md", text);
        assert!(direct(
            &plan,
            json!({"status": "in_progress", "difficulty": "low", "priority": "p1"})
        ));
        let body = std::fs::read_to_string(&plan).unwrap();
        let parsed = codec::parse_frontmatter(&body).unwrap();
        assert_eq!(s(&parsed.fields, "status"), "ready");
        assert_eq!(s(&parsed.fields, "difficulty"), "medium");
        assert_eq!(s(&parsed.fields, "priority"), "p1");
        assert_eq!(
            list(&parsed.fields, "done_probes"),
            ["fno x --json | grep -oE 'scope: [a-z]+'"]
        );
    }
}
