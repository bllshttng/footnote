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
//!
//! Resolution runs over a light index (id, slug, archived flag) and the
//! winner is served through the single-node loader; the read never
//! assembles the whole graph to answer one id.

use serde_json::{json, Value};

use super::node_ref::resolve_tiers;
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
    // The light index answers the resolution tiers without assembling any
    // node body: id, slug, archived flag, in store order.
    let index = match super::read_resolution_index(&graph_path) {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!(
                "Could not read the graph cleanly, so '{}' cannot be resolved: {}",
                args.id, err
            );
            return GRAPH_UNREADABLE_EXIT;
        }
    };
    let live: Vec<Value> = index
        .iter()
        .filter(|r| r.get("archived_at").is_none())
        .cloned()
        .collect();
    if let Some(hit) = resolve_tiers(&live, args.id) {
        let id = hit
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return serve_loaded(&graph_path, &id, false, &args.render, &args.id);
    }
    let archived: Vec<Value> = index
        .iter()
        .filter(|r| r.get("archived_at").is_some())
        .cloned()
        .collect();
    if let Some(hit) = resolve_tiers(&archived, args.id) {
        let id = hit
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return serve_loaded(&graph_path, &id, true, &args.render, &args.id);
    }
    let served = served_store_path(&graph_path);
    eprintln!(
        "No node matching '{}' (id/slug/bare-hex) in {}",
        args.id,
        served.display()
    );
    1
}

/// Load one node and serve it with the same annotations the full-export read
/// produced. `archived` stamps `_archived` before the work-map annotation,
/// matching the captured Python bytes. The readiness overlay runs over the
/// node plus its dependency closure so `status`/`blocked_reason` derive
/// exactly as they do against the whole graph.
fn serve_loaded(
    graph_path: &std::path::Path,
    id: &str,
    archived: bool,
    render: &Render,
    query: &str,
) -> i32 {
    let connection = match super::read_connection(graph_path) {
        Ok(connection) => connection,
        Err(err) => {
            eprintln!("Could not read the graph cleanly, so '{query}' cannot be resolved: {err}");
            return GRAPH_UNREADABLE_EXIT;
        }
    };
    let node = match super::nodes::node_claims_by_id() {
        Ok(claims) => {
            let claim = claims.get(id).cloned().unwrap_or_default();
            let loaded =
                match super::nodes::load_with_claim(&connection, id, Some(claim.clone()), None) {
                    Ok(loaded) => loaded,
                    Err(err) => {
                        eprintln!(
                        "Could not read the graph cleanly, so '{query}' cannot be resolved: {err}"
                    );
                        return GRAPH_UNREADABLE_EXIT;
                    }
                };
            match loaded {
                Some(node) => {
                    let mut row = node.to_json();
                    super::nodes::project_claim_value(&mut row, claim);
                    Some(row)
                }
                None => {
                    // A raw-carried resident loads only through raw_rows_where;
                    // the whole-graph export served it verbatim, so the
                    // single-node read does too. Still absent after that: a
                    // node that vanished between the two reads, the same race
                    // the export named `vanished mid-export`.
                    let found = match super::nodes::raw_rows_where(
                        &connection,
                        Some(&[id.to_string()][..]),
                    ) {
                        Ok(found) => found,
                        Err(err) => {
                            eprintln!(
                                "Could not read the graph cleanly, so '{query}' cannot be resolved: {err}"
                            );
                            return GRAPH_UNREADABLE_EXIT;
                        }
                    };
                    found
                        .into_iter()
                        .find(|(raw_id, _, _)| raw_id == id)
                        .map(|(_, _, mut row)| {
                            super::nodes::project_claim_value(&mut row, claim);
                            row
                        })
                }
            }
        }
        Err(err) => {
            eprintln!("Could not read the graph cleanly, so '{query}' cannot be resolved: {err}");
            return GRAPH_UNREADABLE_EXIT;
        }
    };
    let node = match node {
        Some(node) => node,
        None => {
            eprintln!(
                "Could not read the graph cleanly, so '{query}' cannot be resolved: node {id} vanished mid-export"
            );
            return GRAPH_UNREADABLE_EXIT;
        }
    };
    let mut rows = vec![node];
    let closure = match dependency_closure(&connection, &rows[0]) {
        Ok(closure) => closure,
        Err(err) => {
            eprintln!("Could not read the graph cleanly, so '{query}' cannot be resolved: {err}");
            return GRAPH_UNREADABLE_EXIT;
        }
    };
    rows.extend(closure);
    crate::graph_store::apply_defaults(&mut rows, false);
    crate::graph_store::apply_readiness_overlay(&mut rows);
    let row = rows.remove(0);
    let out = stamped_annotated(row, archived);
    println!("{}", render_out(&out, render));
    0
}

/// The rows the single-node defaults pass can visit for this node: every
/// direct child (the children summary rebuilds from the rows in the vec),
/// every direct blocker, and each loaded row's supersession successor and
/// own blockers, transitively. An id that loads absent stays absent, so
/// `compute_readiness` answers `unknown-dep` exactly as it does against the
/// whole-graph index. Read errors propagate: a silently skipped child would
/// render a wrong children summary instead of failing the read.
fn dependency_closure(
    connection: &rusqlite::Connection,
    target: &Value,
) -> Result<Vec<Value>, String> {
    let target_id = target
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut loaded: std::collections::BTreeMap<String, Value> = Default::default();
    let mut queue: Vec<String> = target
        .get("blocked_by")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Direct children in store order, then raw-carried children: the
    // children summary in apply_defaults summarizes whatever rows the vec
    // holds, so the vec must hold them.
    let mut statement = connection
        .prepare_cached("SELECT id FROM nodes WHERE parent_id = ?1 ORDER BY ordinal, id")
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map(rusqlite::params![target_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    queue.extend(ids);
    let parents = vec![target_id.clone()];
    let children = super::nodes::raw_children(connection, &parents)?;
    for (id, _, body) in children {
        if loaded.contains_key(&id) {
            continue;
        }
        enqueue_deps(Some(&body), &mut queue);
        loaded.insert(id, body);
    }
    while let Some(id) = queue.pop() {
        if loaded.contains_key(&id) {
            continue;
        }
        let row = match super::nodes::load(connection, &id) {
            Ok(Some(node)) => Some(node.to_json()),
            Ok(None) => {
                // A raw-carried resident loads only through raw_rows_where.
                let found = super::nodes::raw_rows_where(connection, Some(&[id.clone()][..]))?;
                found.into_iter().next().map(|(_, _, row)| row)
            }
            Err(err) => return Err(err),
        };
        if let Some(row) = row {
            enqueue_deps(Some(&row), &mut queue);
            loaded.insert(id, row);
        }
    }
    Ok(loaded.into_values().collect())
}

/// Queue a loaded row's blockers and supersession successor: everything the
/// readiness pass may look up about it.
fn enqueue_deps(row: Option<&Value>, queue: &mut Vec<String>) {
    let Some(row) = row else {
        return;
    };
    if let Some(blockers) = row.get("blocked_by").and_then(Value::as_array) {
        for blocker in blockers {
            if let Some(id) = blocker.as_str() {
                queue.push(id.to_string());
            }
        }
    }
    if let Some(successor) = row.get("superseded_by").and_then(Value::as_str) {
        if !successor.is_empty() {
            queue.push(successor.to_string());
        }
    }
}

/// The render ladder: field, grouped, or the pretty JSON default. `_branch`
/// is a derived field: the node's branch from the one Rust mint/resolver,
/// resolved against the stamped `_resolved_cwd`; `null` when the row has no
/// id.
fn render_out(row: &Value, render: &Render) -> String {
    if let Some(field) = &render.field {
        if field == "_branch" {
            return crate::node_branch::resolve(row).unwrap_or_else(|| "null".to_string());
        }
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
fn stamped_annotated(mut row: Value, archived: bool) -> Value {
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

/// The store file the served read came from: the sqlite mirror the layout
/// resolver names for this anchor, else the anchor itself.
fn served_store_path(graph_path: &std::path::Path) -> std::path::PathBuf {
    let db = super::database_path(graph_path);
    if db.exists() {
        db
    } else {
        graph_path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::node_ref::archive_hit;
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
    fn the_branch_field_renders_the_mint_for_a_row_without_a_repo() {
        let row = json!({"id": "x-eeee", "type": "feature", "slug": "some-work", "_resolved_cwd": Value::Null});
        let render = Render {
            field: Some("_branch".to_string()),
            ..Default::default()
        };
        assert_eq!(render_out(&row, &render), "feature/x-eeee-some-work");
        assert_eq!(render_out(&json!({"type": "feature"}), &render), "null");
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
}
