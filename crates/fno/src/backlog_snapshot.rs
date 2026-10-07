//! The board snapshot: the web backlog page with the data embedded. One
//! page, two modes; the Python HTML renderer this replaces was a second
//! board that drifted from the served one.
//!
//! The writer gathers once through the same read model the bridge serves
//! (`backlog_model`), computes every card's node view, and injects the
//! payload into the vendored page as one `application/json` script tag. The
//! page detects the tag and runs entirely client-side: filters and lane
//! regrouping recompute from the embedded cards, the panel reads the embedded
//! node views, and no write control renders.

use crate::backlog_model::{self, Query};
use crate::backlog_view;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// The `fno board-render` verb: read the request from stdin
/// (`{"targets": [{"path", "scope"}], "vault"}`), gather once, write every
/// target, print a JSON receipt. Exit 0 only when every target wrote; a
/// failed target exits 1 so the keeper's render pass withholds its
/// rendered_version stamp and retries, the same contract the Python
/// `render-views` had.
pub fn run(_rest: &[String]) -> i32 {
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        eprintln!("board-render: the request on stdin was unreadable");
        return 2;
    }
    let request: Request = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("board-render: bad request JSON: {e}");
            return 2;
        }
    };
    let failed = render_request(&request);
    let mut exit = 0;
    if let Ok(receipt) = &failed {
        exit = i32::from(!receipt.failed.is_empty());
    }
    let receipt_text = failed
        .as_ref()
        .ok()
        .and_then(|r| serde_json::to_string(r).ok());
    if let Some(line) = receipt_text {
        println!("{line}");
    } else if let Err(e) = failed {
        eprintln!("board-render: {e}");
        exit = 1;
    }
    exit
}

/// One render target row: where to write and the project scope (`None` or
/// `"all"` reads as every project).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Target {
    pub path: String,
    #[serde(default)]
    pub scope: Option<String>,
}

/// The request the Python caller writes to stdin: the configured local
/// targets plus the Obsidian vault name for the deep links.
///
/// `deny_unknown_fields` is the version-skew guard: an installed binary
/// predating the `public` field must refuse the request, never silently fall
/// back to rendering the private graph onto a public path.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub targets: Vec<Target>,
    #[serde(default)]
    vault: Option<String>,
    /// The public open-work projection: selection (`public_backlog_entries`
    /// semantics) and the title gate run here in Rust beside the output
    /// allowlist, so the private graph never leaves the process unfiltered.
    #[serde(default)]
    public: Option<PublicRequest>,
}

/// The public projection's request row: one named project scope.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PublicRequest {
    pub project: String,
}

/// The receipt: one row per target under `written` or `failed`, so the
/// caller's warning names the path.
#[derive(Debug, Serialize)]
struct Receipt {
    written: Vec<Value>,
    failed: Vec<Value>,
}

fn render_request(request: &Request) -> Result<Receipt, String> {
    // Leads are read once at render time from the agent registry and
    // stamped: the page header says when the leads were read, and a lead
    // change alone does not re-render the page. A public render drops the
    // roster here: inputs_from_rows rebuilds the inputs without one, and
    // the payload allowlist strips `leads_at`.
    let now = now_secs();
    let agents = std::fs::read_to_string(crate::agents_view::registry_path())
        .map(|raw| agents_from_registry(&raw, now))
        .unwrap_or_default();
    let graph = backlog_view::graph_path();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let leads_at = (!agents.is_empty()).then_some(now);
    let gathered = runtime.block_on(backlog_model::gather(&graph, agents));
    let public = request.public.is_some();
    let mut inputs = match &request.public {
        Some(spec) => inputs_from_rows(&select_public_rows(&gathered.rows, &spec.project)?),
        None => gathered,
    };
    // The render already pays the graph read; one registry + question read
    // here freezes the session-derived keys and `has:question` into the
    // page. A public projection drops the roster and the search maps with
    // it, so it reads neither.
    if !public {
        backlog_model::read_search_sources(&mut inputs, true);
    }
    let mut receipt = Receipt {
        written: Vec::new(),
        failed: Vec::new(),
    };
    for target in &request.targets {
        let scope = target
            .scope
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "all");
        match render_one(&inputs, scope, request.vault.as_deref(), leads_at, public) {
            Ok((page, cards)) => match atomic_write(Path::new(&target.path), &page) {
                Ok(()) => receipt
                    .written
                    .push(json!({ "path": target.path, "cards": cards })),
                Err(e) => receipt
                    .failed
                    .push(json!({ "path": target.path, "error": e })),
            },
            Err(e) => receipt
                .failed
                .push(json!({ "path": target.path, "error": e })),
        }
    }
    Ok(receipt)
}

/// The selected row set as read-model inputs: no claims, no roster, flow
/// marked unavailable. `select_public_rows` produced the rows; the render
/// allowlists what the page embeds.
fn inputs_from_rows(rows: &[Value]) -> backlog_model::Inputs {
    backlog_model::Inputs {
        backend: "graph".into(),
        rows: rows.to_vec(),
        order: rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
            .collect(),
        flow: json!({"available": false, "reason": "public projection"}),
        ..Default::default()
    }
}

/// Render one target's page from the gathered inputs. A failed source read
/// refuses the target (the caller leaves the last good page byte-unchanged,
/// the Python renderer's rule).
fn render_one(
    inputs: &backlog_model::Inputs,
    scope: Option<&str>,
    vault: Option<&str>,
    leads_at: Option<u64>,
    public: bool,
) -> Result<(String, usize), String> {
    if let Some(err) = &inputs.rows_error {
        return Err(err.clone());
    }
    let mut pairs: Vec<(String, String)> = vec![
        ("view".to_string(), "list".to_string()),
        ("all".to_string(), "1".to_string()),
    ];
    if let Some(scope) = scope {
        pairs.push(("project".to_string(), scope.to_string()));
    }
    let query = Query::from_pairs(&pairs)?;
    let board = backlog_model::board(inputs, &query);
    let mut flat: Vec<&backlog_model::Card> = Vec::new();
    for lane in &board.lanes {
        for cell in &lane.cells {
            flat.extend(cell.cards.iter());
        }
    }
    // Zero matching rows under a named scope is the typo'd-project
    // signature: leave the operator's last good page byte-unchanged rather
    // than replace it with an empty board.
    if let Some(scope) = scope {
        if flat.is_empty() {
            return Err(format!(
                "no graph rows carry project {scope:?}; target left unchanged"
            ));
        }
    }
    // Every card's detail view, keyed by id, plus the Obsidian deep link the
    // writer computes from the row's plan_path.
    let ids: Vec<String> = flat.iter().map(|c| c.id.clone()).collect();
    let mut nodes: HashMap<String, Value> = HashMap::with_capacity(ids.len());
    for id in &ids {
        if nodes.contains_key(id) {
            continue;
        }
        let mut view = serde_json::to_value(
            backlog_model::node(inputs, id).ok_or_else(|| format!("node read failed for {id}"))?,
        )
        .map_err(|e| format!("node serialization failed: {e}"))?;
        if let (Some(vault), Some(plan)) = (vault, view.get("plan_path").and_then(Value::as_str)) {
            if let Some(url) = obsidian_url(vault, plan) {
                view["obsidian"] = Value::String(url);
            }
        }
        nodes.insert(id.clone(), view);
    }
    let count = ids.len();
    // Every card's search field map: one builder (the board's), embedded
    // so the page's grammar filters with no bridge.
    let by_ref = backlog_model::board_refs(inputs);
    let mut search = std::collections::HashMap::with_capacity(ids.len());
    for card in &flat {
        let row = by_ref.get(card.id.as_str()).copied();
        search.insert(
            card.id.clone(),
            backlog_model::search_fields(inputs, &by_ref, card, row),
        );
    }
    let mut payload = json!({
        "schema": 1,
        "generated_at": now_secs(),
        "vault": vault,
        "backend": board.backend,
        "scope": board.scope,
        "flow": board.stats.flow,
        "facets": board.facets,
        "columns": board.stats.totals.iter().map(|t| t.column).collect::<Vec<_>>(),
        "cards": &flat,
        "nodes": nodes,
        "search": search,
        "search_keys": crate::search_query::keys_json(),
        "search_as_of": inputs.read_at,
        "search_names": !inputs
            .errors
            .iter()
            .any(|e| e.contains("names unavailable")),
        "leads_at": leads_at,
    });
    if public {
        payload = to_public_payload(payload);
    }
    Ok((snapshot_page(crate::web::BACKLOG_PAGE, &payload)?, count))
}

/// The registry rows the snapshot's roster needs: live promoted agents only.
/// Only the role fields copy over, so `live` stays false on the static page
/// (no `harness_session_id` is carried).
fn agents_from_registry(raw: &str, now: u64) -> Vec<crate::proto::AgentRow> {
    crate::agents_view::derive_rows(raw, now)
        .unwrap_or_default()
        .iter()
        .filter(|r| !r.exited && r.role_scope.is_some())
        .map(|r| crate::proto::AgentRow {
            name: r.name.clone(),
            role_level: r.role_level,
            role_scope: r.role_scope.clone(),
            ..Default::default()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The public projection: selection, the title gate, and the output allowlist.
// The private page embeds whole node views and free text; the public page is
// the allowlisted subset, so a future private-payload field (session ids,
// agent names, search maps) can never reach it by being added on the writer
// side alone.
// ---------------------------------------------------------------------------

/// The payload's top-level fields the public page may carry. `vault` is
/// private-mode only.
const PUBLIC_PAYLOAD_FIELDS: &[&str] = &[
    "schema",
    "generated_at",
    "backend",
    "scope",
    "flow",
    "facets",
    "columns",
    "cards",
    "nodes",
];

/// The card fields the public page may carry: the board facts only. `lead`
/// and `live` are roster facts and stay private.
const PUBLIC_CARD_FIELDS: &[&str] = &[
    "id",
    "slug",
    "title",
    "column",
    "order",
    "rank",
    "priority",
    "size",
    "status",
    "project",
    "parent",
    "kind",
    "tags",
    "blocked",
    "claimed",
    "created_at",
    "completed_at",
    "encounters",
    "encounters_operator",
];

/// The node view's fields the public page may carry. plan_path, cwd,
/// details, current_state, origin evidence, notes, decisions, unavailable
/// and sessions are private; prs and the node links are public, with links
/// filtered to public ids.
const PUBLIC_NODE_FIELDS: &[&str] = &[
    "card",
    "kind",
    "difficulty",
    "created_at",
    "completed_at",
    "prs",
    "children",
    "contained",
    "blocked_by",
    "blocks",
    "related",
    "parent",
];

/// The link lists the public panel renders, filtered to public ids.
const LINK_KEYS: &[&str] = &[
    "children",
    "contained",
    "blocked_by",
    "blocks",
    "related",
    "parent",
];

/// The facet lists the public filter bar may carry. `leads` is a roster
/// fact; `tags` is free text the title gate never sees, so both stay
/// private even though the page could render them.
const PUBLIC_FACET_FIELDS: &[&str] = &[
    "projects",
    "epics",
    "priorities",
    "sizes",
    "statuses",
    "kinds",
];

/// Reduce `value` (an object) to the allowlisted fields. A non-object passes
/// through, so `null` slots ride along.
fn allowlist_copy(value: &Value, fields: &[&str]) -> Value {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| fields.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<String, Value>>()
            .into(),
        other => other.clone(),
    }
}

/// The public payload: the same shape run through the three allowlists, with
/// every node link retargeted to a public id.
fn to_public_payload(payload: Value) -> Value {
    let ids: std::collections::HashSet<&str> = payload["cards"]
        .as_array()
        .map(|cards| {
            cards
                .iter()
                .filter_map(|c| c.get("id").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let mut out = allowlist_copy(&payload, PUBLIC_PAYLOAD_FIELDS);
    if let Some(cards) = out.get_mut("cards").and_then(Value::as_array_mut) {
        for card in cards.iter_mut() {
            *card = allowlist_copy(card, PUBLIC_CARD_FIELDS);
        }
    }
    if let Some(nodes) = out.get_mut("nodes").and_then(Value::as_object_mut) {
        for view in nodes.values_mut() {
            let mut public_view = allowlist_copy(view, PUBLIC_NODE_FIELDS);
            // The node's embedded card gets the same card allowlist the
            // top-level cards got, so a private Card field (session ids)
            // added later cannot leak through this copy.
            if let Some(card) = public_view.get_mut("card") {
                *card = allowlist_copy(card, PUBLIC_CARD_FIELDS);
            }
            for key in LINK_KEYS {
                if let Some(links) = public_view.get_mut(*key).and_then(Value::as_array_mut) {
                    links.retain(|l| {
                        l.get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| ids.contains(id))
                    });
                }
            }
            *view = public_view;
        }
    }
    if let Some(facets) = out.get_mut("facets") {
        *facets = allowlist_copy(facets, PUBLIC_FACET_FIELDS);
    }
    out
}

/// The swimlane project key, ported from `_project_key`: the row's project
/// string, else the unscoped label (which never matches a named scope).
fn project_key(row: &Value) -> &str {
    match row.get("project").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p,
        _ => "(unscoped)",
    }
}

/// The public backlog's status membership, ported from
/// `derived_status` + `PUBLIC_BACKLOG_STATUSES`: an open status, unless the
/// row is terminally closed (`superseded_by`, the one closure signal an
/// open raw status can carry) with a `completed_at`.
fn in_public_backlog_set(row: &Value) -> bool {
    let status = row.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(status, "in_progress" | "ready" | "blocked" | "idea") {
        return false;
    }
    let superseded = row
        .get("superseded_by")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let completed = row
        .get("completed_at")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    !(superseded && completed)
}

/// The one leak vocabulary, a byte-for-byte port of
/// `roadmap_public.LEAK_PATTERNS` (also ported at
/// `fno-agents title_gate.rs`): node ids and PR numbers are public; home
/// paths and session ids are not.
static LEAK_PATTERNS: std::sync::LazyLock<Vec<(&'static str, regex::Regex)>> =
    std::sync::LazyLock::new(|| {
        vec![
            (
                "home-path",
                regex::Regex::new(r"(?:~/(?:[^\s]+)|/(?:Users|home)/[^\s/]+(?:/[^\s]+)?)")
                    .expect("static regex"),
            ),
            (
                "session-id",
                regex::Regex::new(
                    r"(?i)\b(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|ses-[A-Za-z0-9_-]+)\b",
                )
                .expect("static regex"),
            ),
        ]
    });

/// The selection behind `--backlog-html`: `public_backlog_entries` plus the
/// title gate, in the same order the markdown render runs them. A leaking
/// title costs its own row only; the warning names the classes so the
/// operator hears why a row went missing.
fn select_public_rows(rows: &[Value], project: &str) -> Result<Vec<Value>, String> {
    let selected: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("public").and_then(Value::as_bool) != Some(false))
        .filter(|r| project_key(r) == project)
        .filter(|r| in_public_backlog_set(r))
        .collect();
    let mut out = Vec::new();
    let mut omitted = 0usize;
    let mut classes: Vec<&str> = Vec::new();
    for row in selected {
        let title = row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        let hits: Vec<&str> = LEAK_PATTERNS
            .iter()
            .filter(|(_, p)| p.is_match(&title))
            .map(|(class, _)| *class)
            .collect();
        if hits.is_empty() {
            out.push(row.clone());
        } else {
            omitted += 1;
            classes.extend(hits);
        }
    }
    if omitted > 0 {
        classes.sort_unstable();
        classes.dedup();
        eprintln!(
            "Warning: omitted {omitted} public row(s) from the {project} backlog render: title matched {}; the rest published.",
            classes.join(", ")
        );
    }
    if out.is_empty() {
        // The typo'd-project signature, same rule the scoped private render
        // runs: refuse rather than publish an empty public board.
        return Err(format!(
            "no public rows carry project {project:?}; nothing rendered"
        ));
    }
    Ok(out)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Inject the payload into the page: the body gains `data-snapshot` and one
/// JSON script tag ahead of the page's own script, so the page script reads
/// it on first run. Every `<` in the JSON escapes to `<`, which JSON
/// parses identically and which can never close the tag early.
fn snapshot_page(page: &str, payload: &Value) -> Result<String, String> {
    let raw = serde_json::to_string(payload).map_err(|e| format!("payload failed: {e}"))?;
    let safe = raw.replace('<', "\\u003c");
    let anchor = match page.find(BODY_ANCHOR) {
        Some(idx) => idx,
        None => return Err("the vendored page lost its <body> anchor".into()),
    };
    let after = anchor + BODY_ANCHOR.len();
    Ok(format!(
        "{}{}<script type=\"application/json\" id=\"fno-snapshot\">{}</script>{}",
        &page[..anchor],
        "<body data-snapshot=\"true\">",
        safe,
        &page[after..]
    ))
}

/// The injection anchor. The page test pins the same anchor.
const BODY_ANCHOR: &str = "<body>";

/// Stage-then-rename write, so a reader never sees a half page.
fn atomic_write(out: &Path, body: &str) -> Result<(), String> {
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "board".into());
    let tmp = parent.join(format!(".{name}.tmp{}", std::process::id()));
    let write = std::fs::write(&tmp, body);
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", out.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename {}: {e}", out.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Obsidian deep links: the port of render_html.py's plan-path canonicalizer.
// A plan path reaches the graph in several shapes (vault-relative,
// vault-prefixed, worktree-rooted); the link wants vault-relative with `.md`
// stripped, and only a markdown file links at all.
// ---------------------------------------------------------------------------

/// The vault top-level directories a canonical plan path may start with.
const VAULT_TOPLEVEL_DIRS: &[&str] = &["internal/"];

fn canonical_plan_path(plan_path: &str, vault: Option<&str>) -> Option<String> {
    let p = plan_path.trim();
    if p.is_empty() {
        return None;
    }
    if VAULT_TOPLEVEL_DIRS.iter().any(|d| p.starts_with(d)) {
        return Some(p.to_string());
    }
    if let Some(vault) = vault {
        let needle = format!("/{vault}/");
        strip_vault_prefix(p, &needle)
    } else {
        last_toplevel_segment(p)
    }
}

fn strip_vault_prefix(p: &str, needle: &str) -> Option<String> {
    let idx = p.rfind(needle)?;
    let stripped = &p[idx + needle.len()..];
    if VAULT_TOPLEVEL_DIRS.iter().any(|d| stripped.starts_with(d)) {
        return Some(stripped.to_string());
    }
    None
}

/// The worktree-rooted shape: pick the LAST top-level dir occurrence and
/// keep from there.
fn last_toplevel_segment(p: &str) -> Option<String> {
    let mut best: isize = -1;
    for marker in VAULT_TOPLEVEL_DIRS {
        let needle = format!("/{marker}");
        if let Some(idx) = p.rfind(&needle) {
            let idx = idx as isize;
            if idx > best {
                best = idx;
            }
        }
    }
    if best >= 0 {
        return Some(p[best as usize + 1..].to_string());
    }
    None
}

fn obsidian_url(vault: &str, plan_path: &str) -> Option<String> {
    let canonical = canonical_plan_path(plan_path, Some(vault))?;
    let canonical = canonical.trim_end_matches('/').to_string();
    if !canonical.ends_with(".md") {
        return None;
    }
    let target = canonical[..canonical.len() - 3].to_string();
    Some(format!(
        "obsidian://open?vault={}&file={}",
        percent_encode(vault, ""),
        percent_encode(&target, "/")
    ))
}

/// Percent-encode with Python's `quote` defaults: unreserved characters and
/// the `safe` set pass through, everything else becomes %XX.
fn percent_encode(text: &str, safe: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "_.~-".contains(c) || safe.contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `fno board-render` claims its name lexically, beside the other native
/// front verbs: the argv `board-render ...` never forwards to Python.
pub fn classify(args: &[std::ffi::OsString]) -> Option<Vec<String>> {
    if args.first()?.to_str()? != "board-render" {
        return None;
    }
    Some(
        args.iter()
            .skip(1)
            .filter_map(|a| a.to_str().map(str::to_string))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload rides BEFORE the page's own script (so it exists when the
    /// script runs), the body gains the snapshot flag, and the page tail
    /// survives byte-identical.
    #[test]
    fn the_payload_rides_ahead_of_the_page_script() {
        let page = "<html><body>\n<script>main();</script></body></html>";
        let out = snapshot_page(page, &json!({"cards": []})).unwrap();
        assert!(out.contains("<body data-snapshot=\"true\">"), "{out}");
        let json_start = out.find("id=\"fno-snapshot\">").unwrap();
        let page_script = out.find("<script>main();").unwrap();
        assert!(
            json_start < page_script,
            "payload must precede the page script"
        );
        assert!(out.ends_with("</script></body></html>"));
        // The render embeds every card's search field map, the grammar's
        // key table, and the as-of stamp.
        let rows = vec![
            json!({"id": "x-s1", "slug": "s1", "title": "One", "status": "ready", "priority": "p2"}),
            json!({"id": "x-s2", "slug": "s2", "title": "Two", "status": "idea", "priority": "p2"}),
        ];
        let inputs = backlog_model::Inputs {
            backend: "graph".into(),
            rows,
            order: vec!["x-s1".into(), "x-s2".into()],
            flow: json!({"available": false, "reason": "fixture"}),
            read_at: 1790856000,
            ..Default::default()
        };
        let (page, count) = render_one(&inputs, None, None, None, false).unwrap();
        assert_eq!(count, 2);
        let marker = "id=\"fno-snapshot\">";
        let start = page.find(marker).unwrap() + marker.len();
        let end = page[start..].find("</script>").unwrap() + start;
        let payload: Value = serde_json::from_str(&page[start..end]).unwrap();
        let search = payload.get("search").expect("the search maps ride");
        assert!(search.get("x-s1").is_some() && search.get("x-s2").is_some());
        assert_eq!(
            payload["search_keys"]["keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["names"][0] == "status"),
            true,
            "the key table rides"
        );
        assert_eq!(payload["search_as_of"], 1790856000);
    }

    /// A `</script>` inside the data must not close the tag early: every `<`
    /// escapes, and the escape parses back to the same string.
    #[test]
    fn a_script_closer_in_the_data_cannot_break_the_tag() {
        let page = "<html><body><script>var p;</script></body></html>";
        let payload = json!({"nodes": {"x-1": {"title": "</script><script>alert(1)</script>"}}});
        let out = snapshot_page(page, &payload).unwrap();
        let marker = "id=\"fno-snapshot\">";
        let start = out.find(marker).unwrap() + marker.len();
        let end = out[start..].find("</script>").unwrap() + start;
        let back: Value = serde_json::from_str(&out[start..end]).unwrap();
        assert_eq!(back, payload);
        // A lone `<` escapes by the same one rule.
        let out = snapshot_page("<body>x", &json!({"q": "a<b"})).unwrap();
        assert!(out.contains("a\\u003cb"), "{out}");
    }

    /// The registry reader keeps exactly the live promoted rows, with their
    /// scope and level; exited and unpromoted rows drop. Only role fields
    /// copy, so the static page's roster stays paneless.
    #[test]
    fn promoted_registry_rows_become_the_snapshot_roster() {
        let raw = r#"{"agents": [
            {"name": "lead-live", "role_level": 1, "role_scope": "x-aaaa"},
            {"name": "lead-exited", "role_level": 2, "role_scope": "x-bbbb", "status": "exited"},
            {"name": "plain", "role_level": null, "role_scope": null}
        ]}"#;
        let roster = agents_from_registry(raw, 1000);
        assert_eq!(roster.len(), 1, "one live promoted row survives");
        assert_eq!(roster[0].name, "lead-live");
        assert_eq!(roster[0].role_scope.as_deref(), Some("x-aaaa"));
        assert_eq!(roster[0].role_level, Some(1));
        assert!(roster[0].harness_session_id.is_none());
        assert!(agents_from_registry("not json at all", 1000).is_empty());
    }

    /// The lexical classifier: the verb name claims itself, every other
    /// first word forwards.
    #[test]
    fn board_render_claims_its_name() {
        use std::ffi::OsString;
        assert!(classify(&[OsString::from("board-render")]).is_some());
        assert!(classify(&[OsString::from("board-render"), OsString::from("--x")]).is_some());
        assert!(classify(&[OsString::from("mux")]).is_none());
    }

    /// Only a markdown plan deep-links, and both URL parts encode.
    #[test]
    fn the_obsidian_link_encodes_vault_and_file() {
        assert_eq!(
            obsidian_url("c3po", "internal/fno/plans/my plan.md").as_deref(),
            Some("obsidian://open?vault=c3po&file=internal/fno/plans/my%20plan")
        );
        assert_eq!(obsidian_url("c3po", "internal/fno/plans/dir"), None);
    }

    /// The canonicalizer accepts the three recorded shapes and refuses a
    /// path with no recognizable vault segment.
    #[test]
    fn plan_paths_canonicalize_to_vault_relative() {
        assert_eq!(
            canonical_plan_path("internal/fno/plans/x.md", None).as_deref(),
            Some("internal/fno/plans/x.md")
        );
        assert_eq!(
            canonical_plan_path("/Users/me/c3po/internal/fno/plans/x.md", Some("c3po")).as_deref(),
            Some("internal/fno/plans/x.md")
        );
        assert_eq!(
            canonical_plan_path("/wt/footnote/some-worktree/internal/fno/plans/x.md", None)
                .as_deref(),
            Some("internal/fno/plans/x.md")
        );
        assert_eq!(canonical_plan_path("~/elsewhere/x.md", Some("c3po")), None);
    }

    /// The public projection end to end: the request surface is exact (an
    /// unknown field refuses, so an installed binary predating `public`
    /// never renders the private graph to a public path); selection keeps
    /// only the project's open, public, clean-titled rows; and the rendered
    /// page's embedded payload passes the three allowlists, so any field
    /// outside them fails this test.
    #[test]
    fn the_public_projection_selects_gates_and_sanitizes() {
        assert!(serde_json::from_str::<Request>(
            r#"{"targets":[{"path":"/tmp/b.html"}],"rogue":1}"#
        )
        .is_err());
        let legacy: Request =
            serde_json::from_str(r#"{"targets":[{"path":"/tmp/b.html"}],"vault":"c3po"}"#).unwrap();
        assert!(legacy.public.is_none());
        let public: Request = serde_json::from_str(
            r#"{"targets":[{"path":"/tmp/b.html"}],"public":{"project":"fno"}}"#,
        )
        .unwrap();
        assert_eq!(public.public.as_ref().unwrap().project, "fno");

        let rows = vec![
            json!({
                "id": "x-1", "title": "Public thing", "slug": "public-thing",
                "status": "ready", "priority": "p1", "project": "fno",
                "type": "feature", "public": true,
                "cwd": "/Users/someone/secret", "session_id": "ses-leak",
                "details": "secret details",
                "notes": [{"text": "secret note"}],
                "sessions": [{"phase": "execute",
                              "session_id": "9b1c2d3e-0000-4000-8000-00000000leak"}],
                "blocked_by": ["x-2"],
                "plan_path": "/Users/someone/internal/fno/plans/pub.md",
            }),
            json!({"id": "x-2", "title": "Linked private", "slug": "lp",
                   "status": "ready", "project": "fno", "public": false}),
            json!({"id": "x-3", "title": "Ship it at /Users/bb/tmp", "slug": "leak",
                   "status": "ready", "project": "fno"}),
            json!({"id": "x-4", "title": "Wrong project", "slug": "wp",
                   "status": "ready", "project": "other"}),
            json!({"id": "x-5", "title": "Done work", "slug": "dw",
                   "status": "done", "project": "fno",
                   "completed_at": "2026-01-01"}),
            json!({"id": "x-6", "title": "Stale open", "slug": "so",
                   "status": "in_progress", "project": "fno",
                   "superseded_by": "x-1", "completed_at": "2026-01-01"}),
        ];
        let selected = select_public_rows(&rows, "fno").unwrap();
        let ids: Vec<&str> = selected.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["x-1"]);

        let inputs = inputs_from_rows(&selected);
        let (private, _) = render_one(&inputs, None, None, None, false).unwrap();
        assert!(
            private
                .contains("fno agents adopt 9b1c2d3e-0000-4000-8000-00000000leak --cross-project"),
            "the private page carries the recovery command"
        );
        let (page, count) = render_one(&inputs, None, None, None, true).unwrap();
        assert_eq!(count, 1);
        assert!(page.contains("Public thing"), "{page}");
        for secret in [
            "/Users/someone/secret",
            "ses-leak",
            "9b1c2d3e",
            "fno agents",
            "--cross-project",
            "secret details",
            "secret note",
            "Linked private",
            "obsidian://",
        ] {
            assert!(!page.contains(secret), "leaked {secret}");
        }
        // The structural gate: walk the embedded payload and fail on any
        // field the allowlists do not name.
        let marker = "id=\"fno-snapshot\">";
        let start = page.find(marker).unwrap() + marker.len();
        let end = page[start..].find("</script>").unwrap() + start;
        let payload: Value = serde_json::from_str(&page[start..end]).unwrap();
        for key in payload.as_object().unwrap().keys() {
            assert!(
                PUBLIC_PAYLOAD_FIELDS.contains(&key.as_str()),
                "payload field {key} is not allowlisted"
            );
        }
        for card in payload["cards"].as_array().unwrap() {
            for key in card.as_object().unwrap().keys() {
                assert!(
                    PUBLIC_CARD_FIELDS.contains(&key.as_str()),
                    "card field {key} is not allowlisted"
                );
            }
        }
        for view in payload["nodes"].as_object().unwrap().values() {
            for key in view.as_object().unwrap().keys() {
                assert!(
                    PUBLIC_NODE_FIELDS.contains(&key.as_str()),
                    "node field {key} is not allowlisted"
                );
            }
        }
        for key in payload["facets"].as_object().unwrap().keys() {
            assert!(
                PUBLIC_FACET_FIELDS.contains(&key.as_str()),
                "facet field {key} is not allowlisted"
            );
        }
        // A link to a non-public id drops.
        let view = &payload["nodes"]["x-1"];
        assert_eq!(view["blocked_by"].as_array().map(Vec::len), Some(0));

        // The typo'd-project refusal: nothing public means nothing rendered.
        assert!(select_public_rows(&rows, "nope").is_err());
    }
}
