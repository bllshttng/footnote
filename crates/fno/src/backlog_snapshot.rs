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
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Request {
    pub targets: Vec<Target>,
    #[serde(default)]
    vault: Option<String>,
}

/// The receipt: one row per target under `written` or `failed`, so the
/// caller's warning names the path.
#[derive(Debug, Serialize)]
struct Receipt {
    written: Vec<Value>,
    failed: Vec<Value>,
}

fn render_request(request: &Request) -> Result<Receipt, String> {
    let graph = backlog_view::graph_path();
    // Leads are read once at render time from the agent registry and
    // stamped: the page header says when the crowns were read, and a crown
    // change alone does not re-render the page.
    let now = now_secs();
    let agents = std::fs::read_to_string(crate::agents_view::registry_path())
        .map(|raw| agents_from_registry(&raw, now))
        .unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let leads_at = (!agents.is_empty()).then_some(now);
    let inputs = runtime.block_on(backlog_model::gather(&graph, agents));
    let mut receipt = Receipt {
        written: Vec::new(),
        failed: Vec::new(),
    };
    for target in &request.targets {
        let scope = target
            .scope
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "all");
        match render_one(&inputs, scope, request.vault.as_deref(), leads_at) {
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

/// Render one target's page from the gathered inputs. A failed source read
/// refuses the target (the caller leaves the last good page byte-unchanged,
/// the Python renderer's rule).
fn render_one(
    inputs: &backlog_model::Inputs,
    scope: Option<&str>,
    vault: Option<&str>,
    leads_at: Option<u64>,
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
    let payload = json!({
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
        "leads_at": leads_at,
    });
    Ok((snapshot_page(crate::web::BACKLOG_PAGE, &payload)?, count))
}

/// The registry rows the snapshot's roster needs: live crowned agents only.
/// Only the crown fields copy over, so `live` stays false on the static page
/// (no `harness_session_id` is carried).
fn agents_from_registry(raw: &str, now: u64) -> Vec<crate::proto::AgentRow> {
    crate::agents_view::derive_rows(raw, now)
        .unwrap_or_default()
        .iter()
        .filter(|r| !r.exited && r.crown_scope.is_some())
        .map(|r| crate::proto::AgentRow {
            name: r.name.clone(),
            crown_level: r.crown_level,
            crown_scope: r.crown_scope.clone(),
            ..Default::default()
        })
        .collect()
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

    /// The registry reader keeps exactly the live crowned rows, with their
    /// scope and level; exited and uncrowned rows drop. Only crown fields
    /// copy, so the static page's roster stays paneless.
    #[test]
    fn crowned_registry_rows_become_the_snapshot_roster() {
        let raw = r#"{"agents": [
            {"name": "lead-live", "crown_level": 1, "crown_scope": "x-3b09"},
            {"name": "lead-exited", "crown_level": 2, "crown_scope": "x-0ce3", "exited": true},
            {"name": "plain", "crown_level": null, "crown_scope": null}
        ]}"#;
        let roster = agents_from_registry(raw, 1000);
        assert_eq!(roster.len(), 1, "one live crowned row survives");
        assert_eq!(roster[0].name, "lead-live");
        assert_eq!(roster[0].crown_scope.as_deref(), Some("x-3b09"));
        assert_eq!(roster[0].crown_level, Some(1));
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
}
