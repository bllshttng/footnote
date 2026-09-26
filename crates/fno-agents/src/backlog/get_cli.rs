//! `fno backlog get`'s single-id ladder, ported from the Python command the
//! grouped dispatcher routes here. Owns: the deterministic resolution tiers
//! (exact id, exact slug, bare-hex re-prefix), the store read, the
//! `_resolved_cwd` work-map annotation, the field / grouped / pretty JSON
//! render ladder, the archive read-through, and the two refusals (exit 3
//! unreadable, exit 1 miss with the served path named).
//!
//! The default output IS `json.dumps(row, indent=2)` of the store row, so
//! the renderer is a byte contract (see render.rs); the goldens were
//! captured from the Python surface before its deletion.

use serde_json::{json, Value};

use super::render::{py_json_compact, py_json_pretty, render_grouped};

/// The unreadable-store exit: click reserves 2 for usage, so 3 is the first
/// free code. Distinct from exit 1 ("read cleanly, node absent").
pub const GRAPH_UNREADABLE_EXIT: i32 = 3;

/// The render arms of the single get: `--field`, `--grouped`, plain.
#[derive(Default)]
struct Render {
    field: Option<String>,
    grouped: bool,
    strict: bool,
}

/// The parsed single-get invocation. `None` is the forward shape: an unknown
/// flag or a second positional.
struct GetArgs<'a> {
    id: &'a str,
    render: Render,
}

impl GetArgs<'_> {
    fn parse(tail: &[String]) -> Option<GetArgs<'_>> {
        let mut id: Option<&str> = None;
        let mut render = Render::default();
        let mut i = 0;
        while i < tail.len() {
            match tail[i].as_str() {
                "--field" => {
                    i += 1;
                    render.field = Some(tail.get(i)?.clone());
                }
                "--grouped" => render.grouped = true,
                "--strict" => render.strict = true,
                other => {
                    if other.starts_with('-') {
                        return None;
                    }
                    if id.is_some() {
                        return None;
                    }
                    id = Some(other);
                }
            }
            i += 1;
        }
        id.map(|id| GetArgs { id, render })
    }
}

/// Deterministic tiers 1 to 3, exact only: exact id, exact slug
/// (case insensitive), bare 4 to 8 lowercase hex re-prefixed by the
/// configured prefix then the legacy `ab-`.
fn resolve_tiers<'a>(entries: &'a [Value], query: &str) -> Option<&'a Value> {
    for e in entries {
        if e.get("id").and_then(Value::as_str) == Some(query) {
            return Some(e);
        }
    }
    let q_lc = query.to_lowercase();
    for e in entries {
        let slug = e.get("slug").and_then(Value::as_str);
        if slug.is_some_and(|s| !s.is_empty()) && slug == Some(q_lc.as_str()) {
            return Some(e);
        }
    }
    let bare_hex = query.len() >= 4
        && query.len() <= 8
        && query
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if bare_hex {
        let mut prefixes = vec![super::settings::node_id_prefix()];
        prefixes.push("ab-".to_string());
        prefixes.dedup();
        for p in &prefixes {
            let cand = format!("{p}{query}");
            if let Some(hit) = entries
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(cand.as_str()))
            {
                return Some(hit);
            }
        }
    }
    None
}

/// The `--field` arm: `_status` keeps its pre-rename alias, a missing field
/// prints `null`, containers print compact JSON, scalars print raw.
fn render_field(row: &Value, field: &str) -> String {
    let name = if field == "_status" { "status" } else { field };
    match row.get(name) {
        None | Some(Value::Null) => "null".to_string(),
        Some(v @ (Value::Array(_) | Value::Object(_))) => py_json_compact(v),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "true" } else { "false" }.to_string(),
        Some(Value::Number(n)) => n.to_string(),
    }
}

/// The `_resolved_cwd` display annotation: the work map's path for the
/// node's project, first mapping wins; else the row's own cwd.
fn resolved_cwd(row: &Value) -> Value {
    if let Some(p) = row
        .get("project")
        .and_then(Value::as_str)
        .and_then(super::settings::project_root)
    {
        return json!(p);
    }
    row.get("cwd").cloned().unwrap_or(Value::Null)
}

/// Stamp the row's `_resolved_cwd` and return the stamped clone.
fn stamped(row: &Value) -> Value {
    let mut out = row.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("_resolved_cwd".to_string(), resolved_cwd(row));
    }
    out
}

/// The archive read-through on a working-graph miss: the tiers, else a
/// `previous_id` hit; the row stamps `_archived`.
fn archive_hit(entries: &[Value], query: &str) -> Option<Value> {
    if let Some(hit) = resolve_tiers(entries, query) {
        let mut row = hit.clone();
        if let Some(obj) = row.as_object_mut() {
            obj.insert("_archived".to_string(), Value::Bool(true));
        }
        return Some(row);
    }
    for e in entries {
        if e.get("previous_id").and_then(Value::as_str) == Some(query) {
            let mut row = e.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("_archived".to_string(), Value::Bool(true));
            }
            return Some(row);
        }
    }
    None
}

/// The whole ladder. `run` returns the process exit code.
pub fn run(tail: &[String]) -> i32 {
    let Some(args) = GetArgs::parse(tail) else {
        return super::cli::forward_to_python("get", tail);
    };
    // The external backend selection leaves the local store non
    // authoritative; that branch's tracker read is Python's client, so the
    // shape forwards with the original argv.
    if crate::graph_get::external_backend_selected() {
        return super::cli::forward_to_python("get", tail);
    }
    let graph_path = super::settings::graph_path();
    // The store's own rows, served verbatim the way the keeper's read_ids
    // answers: no defaults pass, no re-ordering - the row is the binary's
    // typed export, and the golden bytes pin that shape.
    let mut entries = match crate::backlog::read_entries(&graph_path) {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!(
                "Could not read the graph cleanly, so '{}' cannot be resolved: {}",
                args.id, err
            );
            return GRAPH_UNREADABLE_EXIT;
        }
    };
    // The defaults tail every stored row serves with (missing columns read
    // as their typed defaults, appended in place), then the read-time
    // readiness overlay (status + blocked_reason derive against the whole
    // graph). Archived residents fall to the read-through: they live in the
    // SAME store now (the `archived_at` column), so the read-through
    // partitions this read instead of opening a sidecar file.
    crate::graph_store::apply_defaults(&mut entries, false);
    crate::graph_store::apply_readiness_overlay(&mut entries);
    let live: Vec<Value> = entries
        .iter()
        .filter(|r| r.get("archived_at").is_none())
        .cloned()
        .collect();
    if let Some(hit) = resolve_tiers(&live, args.id) {
        let row = stamped_annotated(&hit, false);
        println!("{}", render_out(&row, &args.render));
        return 0;
    }
    let archived: Vec<Value> = entries
        .iter()
        .filter(|r| r.get("archived_at").is_some())
        .cloned()
        .collect();
    if let Some(row) = archive_hit(&archived, args.id) {
        let out = stamped_annotated(&row, true);
        println!("{}", render_out(&out, &args.render));
        return 0;
    }
    let served = served_store_path(&graph_path);
    eprintln!(
        "No node matching '{}' (id/slug/bare-hex) in {}",
        args.id,
        served.display()
    );
    1
}

/// The render ladder: field, grouped, or the pretty JSON default.
fn render_out(row: &Value, render: &Render) -> String {
    if let Some(field) = &render.field {
        return render_field(row, field);
    }
    if render.grouped {
        return render_grouped(row);
    }
    py_json_pretty(row)
}

/// The served row's annotations: the `_resolved_cwd` work-map stamp, then
/// the reading marker. An archived stamp lands before the annotation,
/// matching the captured Python bytes.
fn stamped_annotated(hit: &Value, archived: bool) -> Value {
    let mut row = hit.clone();
    if archived {
        if let Some(obj) = row.as_object_mut() {
            obj.insert("_archived".to_string(), Value::Bool(true));
        }
    }
    let cwd = resolved_cwd(&row);
    if let Some(obj) = row.as_object_mut() {
        obj.insert("_resolved_cwd".to_string(), cwd);
    }
    let mut out = vec![row];
    crate::node_reading::attach_reading(&mut out);
    out.remove(0)
}

/// The store file the served read came from: the sqlite mirror when one
/// exists beside the json path, else the json path itself.
fn served_store_path(graph_path: &std::path::Path) -> std::path::PathBuf {
    let db = graph_path.with_extension("db");
    if db.exists() {
        db
    } else {
        graph_path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_entries() -> Vec<Value> {
        vec![
            json!({"id": "ab-aaaaaaaa", "slug": "alpha-node", "title": "Alpha node"}),
            json!({"id": "ab-bbbbbbbb", "slug": "beta-node", "title": "Beta node"}),
        ]
    }

    #[test]
    fn tiers_exact_id_slug_and_bare_hex() {
        // The `ab-` fixture keeps the bare-hex tier hermetic: the legacy
        // prefix is always tried second, whatever the machine's config says.
        let entries = fixture_entries();
        assert!(resolve_tiers(&entries, "ab-bbbbbbbb").is_some());
        assert!(resolve_tiers(&entries, "ALPHA-NODE").is_some());
        assert!(resolve_tiers(&entries, "aaaaaaaa").is_some());
        assert!(resolve_tiers(&entries, "AAAAAAAA").is_none());
        assert!(resolve_tiers(&entries, "ab-zzzzzzzz").is_none());
    }

    #[test]
    fn field_arm_aliases_status_and_renders_containers_compact() {
        let row = json!({"status": "blocked", "tags": ["one", "two"], "gone": null});
        assert_eq!(render_field(&row, "status"), "blocked");
        assert_eq!(render_field(&row, "_status"), "blocked");
        assert_eq!(render_field(&row, "tags"), "[\"one\", \"two\"]");
        assert_eq!(render_field(&row, "missing"), "null");
        assert_eq!(render_field(&row, "gone"), "null");
    }

    #[test]
    fn the_archive_hit_stamps_read_only() {
        let archived =
            vec![json!({"id": "x-dddd4444", "slug": "delta-archived", "archived_at": "t"})];
        let hit = archive_hit(&archived, "delta-archived").unwrap();
        assert_eq!(hit["_archived"], json!(true));
        assert!(archive_hit(&archived, "x-miss9999").is_none());
        let by_previous = vec![json!({"id": "x-new", "previous_id": "x-old"})];
        assert_eq!(
            archive_hit(&by_previous, "x-old").unwrap()["id"],
            json!("x-new")
        );
    }

    #[test]
    fn the_parse_rejects_second_positionals_and_unknown_flags() {
        let two = vec!["x-1".to_string(), "x-2".to_string()];
        assert!(GetArgs::parse(&two).is_none());
        let unknown = vec!["x-1".to_string(), "--nope".to_string()];
        assert!(GetArgs::parse(&unknown).is_none());
        let ok = vec!["x-1".to_string(), "--grouped".to_string()];
        assert!(GetArgs::parse(&ok).is_some());
    }

    #[test]
    fn stamped_adds_resolved_cwd_from_cwd_when_no_project() {
        let row = json!({"id": "x-1", "cwd": "/repo"});
        let out = stamped(&row);
        assert_eq!(out["_resolved_cwd"], json!("/repo"));
    }
}
