//! `fno-agents backlog-notes` (wave 3): inventory, digest migration,
//! and history readback over the note corpus.
//!
//! `inventory` is a read-only census. `migrate` defaults to preview; an
//! explicit `--apply --manifest <file>` journals every original note, then
//! replaces the hot row with one bounded current state and the migrated
//! marker. `history` is the paged readback. Originals are keyed by node and
//! source position and are never truncated.
use crate::backlog::node_state::{self, HISTORY_MARKER_KEY, STATE_KEY};
use crate::backlog::note_history;
use crate::graph_store;
use serde_json::{json, Value};
use sha2::Digest as _;
use std::path::PathBuf;

use crate::graph_get::default_graph_path;

/// Canonical hash of a row's notes array: the manifest pins it, so a row that
/// changed since the manifest was prepared reads as stale, never as silently
/// migrated.
fn notes_hash(notes: &Value) -> String {
    let bytes = serde_json::to_vec(notes).unwrap_or_default();
    format!("sha256:{:x}", sha2::Sha256::digest(&bytes))
}

fn notes_of(row: &Value) -> Vec<Value> {
    row.get("progress_notes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn char_len(v: Option<&Value>) -> usize {
    v.and_then(Value::as_str)
        .map(str::chars)
        .map(Iterator::count)
        .unwrap_or(0)
}

/// Read-only census: backend, node counts, note counts and character totals
/// per node (source hash included so a manifest can be prepared from it).
fn run_inventory(graph: &std::path::Path, json_out: bool) -> i32 {
    let entries = match graph_store::read_rows(graph) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fno-agents backlog-notes: {e}");
            return 1;
        }
    };
    let backend = crate::backlog::backend(graph);
    let mut nodes_with_notes = 0usize;
    let mut total_notes = 0usize;
    let mut total_chars = 0usize;
    let mut per_node: Vec<Value> = Vec::new();
    for row in &entries {
        let Some(id) = graph_store::entry_id(row) else {
            continue;
        };
        let notes = notes_of(row);
        if notes.is_empty() {
            continue;
        }
        nodes_with_notes += 1;
        total_notes += notes.len();
        let chars: usize = notes
            .iter()
            .map(|n| char_len(n.get("text")).saturating_add(char_len(n.get("body"))))
            .sum();
        total_chars += chars;
        per_node.push(json!({
            "node_id": id,
            "notes": notes.len(),
            "chars": chars,
            "notes_hash": notes_hash(&Value::Array(notes.clone())),
        }));
    }
    if json_out {
        println!(
            "{}",
            json!({
                "backend": backend.name(),
                "nodes_scanned": entries.len(),
                "nodes_with_notes": nodes_with_notes,
                "total_notes": total_notes,
                "total_chars": total_chars,
                "nodes": per_node,
            })
        );
    } else {
        println!(
            "backend={} nodes={} with_notes={} notes={} chars={}",
            backend.name(),
            entries.len(),
            nodes_with_notes,
            total_notes,
            total_chars
        );
    }
    0
}

/// Paged history readback: `backlog-notes history [<node>|<slug>] [--node <id>] [--offset N] [--limit N]`.
fn run_history(
    graph: &std::path::Path,
    node: Option<&str>,
    row: Option<&Value>,
    offset: usize,
    limit: usize,
    json_out: bool,
) -> i32 {
    let (records, total) = if note_history::history_path(graph).exists() {
        match note_history::read(graph, node, offset, limit) {
            Ok(page) => page,
            Err(e) => {
                eprintln!("fno-agents backlog-notes: {e}");
                return 1;
            }
        }
    } else {
        (Vec::new(), 0)
    };
    if json_out {
        println!(
            "{}",
            json!({"total": total, "offset": offset, "records": records})
        );
        return 0;
    }
    // A token neither the graph row nor the journal knows is a usage error;
    // an archived node's journal outlives its row, so only the pair-empty
    // case refuses.
    if total == 0 && row.is_none() {
        match node {
            Some(tok) => {
                eprintln!(
                    "fno-agents backlog-notes: no node or journal record resolves to '{tok}'"
                );
                return 1;
            }
            None => return 0,
        }
    }
    for r in &records {
        let body = r
            .get("original")
            .map(note_history::record_body)
            .unwrap_or("");
        let rev = r
            .get("prior_revision")
            .and_then(Value::as_u64)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let reason = r.get("reason").and_then(Value::as_str).unwrap_or("");
        let session = r
            .get("source_session_id")
            .and_then(Value::as_str)
            .unwrap_or("-");
        println!("rev {rev} {reason} session {session}");
        println!("{body}");
    }
    // The trailer names ONE node; a whole-journal read prints blocks only.
    let Some(tok) = node else {
        return 0;
    };
    let display_id = row
        .and_then(graph_store::entry_id)
        .map(str::to_string)
        .unwrap_or_else(|| tok.to_string());
    let first = if total == 0 { 0 } else { offset + 1 };
    let last = offset + records.len();
    let mut trailer = format!("{display_id}: records {first}-{last} of {total}");
    match row {
        Some(r) => {
            trailer.push_str(&format!(
                "; current_state revision {}",
                crate::backlog::node_state::row_revision(r)
            ));
            let legacy = r
                .get("progress_notes")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            if legacy > 0 {
                trailer.push_str(&format!(
                    "; {legacy} legacy progress_notes (fno backlog get {display_id})"
                ));
            }
        }
        None => trailer.push_str("; current_state revision 0"),
    }
    println!("{trailer}");
    0
}

/// One manifest entry. `state` is the replacement current-state body;
/// `details` optionally shortens an oversized details field; `source_hash`
/// pins the row's notes array the manifest was prepared from.
#[derive(Clone, Debug)]
struct ManifestEntry {
    node_id: String,
    source_hash: String,
    state: Option<String>,
    details: Option<String>,
    session_id: Option<String>,
    harness: Option<String>,
}

fn parse_manifest(path: &std::path::Path) -> Result<Vec<ManifestEntry>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("manifest read: {e}"))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("manifest json: {e}"))?;
    let arr = doc
        .as_array()
        .or_else(|| doc.get("entries").and_then(Value::as_array))
        .ok_or_else(|| "manifest must be an array or {\"entries\": [...]}".to_string())?;
    let mut out = Vec::new();
    for (i, e) in arr.iter().enumerate() {
        let node_id = e
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("manifest[{i}] missing node_id"))?
            .to_string();
        let source_hash = e
            .get("source_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("manifest[{i}] missing source_hash"))?
            .to_string();
        out.push(ManifestEntry {
            node_id,
            source_hash,
            state: e.get("state").and_then(Value::as_str).map(str::to_string),
            details: e.get("details").and_then(Value::as_str).map(str::to_string),
            session_id: e
                .get("author")
                .and_then(|a| a.get("session_id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            harness: e
                .get("author")
                .and_then(|a| a.get("harness"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(out)
}

enum RowVerdict {
    /// Already migrated (marker present, no notes): the run is idempotent.
    Unchanged,
    /// Terminal row with no manifest entry: the notes move verbatim.
    TerminalVerbatim,
    Digested(ManifestEntry),
    Unresolved(String),
}

/// Validate one row against the manifest under the caller's read.
fn judge(row: &Value, manifest: &[ManifestEntry]) -> RowVerdict {
    let Some(id) = graph_store::entry_id(row).map(str::to_string) else {
        return RowVerdict::Unresolved("row has no id".into());
    };
    let notes = notes_of(row);
    let migrated = row.get(HISTORY_MARKER_KEY).is_some();
    if notes.is_empty() {
        if migrated {
            return RowVerdict::Unchanged;
        }
        return RowVerdict::Unresolved("no notes to migrate but row is not marked migrated".into());
    }
    if migrated {
        // A migrated row must never regrow notes; treat as stale input.
        return RowVerdict::Unresolved("row is migrated but still carries progress_notes".into());
    }
    let status = row.get("status").and_then(Value::as_str).unwrap_or("");
    let terminal = status == "done" || status == "superseded";
    match manifest.iter().find(|m| m.node_id == id) {
        Some(entry) => {
            if notes_hash(&Value::Array(notes)) != entry.source_hash {
                return RowVerdict::Unresolved("source_hash mismatch (stale manifest)".into());
            }
            // The combined candidate prose must fit the budget.
            let details_len = match &entry.details {
                Some(d) => node_state::count_prose(d),
                None => char_len(row.get("details")),
            };
            let state_len = match &entry.state {
                Some(s) => node_state::count_prose(s),
                None => row
                    .get(STATE_KEY)
                    .and_then(|s| s.get("body"))
                    .and_then(Value::as_str)
                    .map(|s| s.chars().count())
                    .unwrap_or(0),
            };
            if details_len + state_len > node_state::PROSE_LIMIT {
                return RowVerdict::Unresolved(format!(
                    "digest over budget: details={details_len} state={state_len} total={} limit={}",
                    details_len + state_len,
                    node_state::PROSE_LIMIT
                ));
            }
            RowVerdict::Digested(entry.clone())
        }
        None if terminal => RowVerdict::TerminalVerbatim,
        None => RowVerdict::Unresolved("open node has no manifest entry".into()),
    }
}

/// Journal every note of one row (with source positions), then the outgoing
/// details when a digest shortens them. Returns the count of records written.
fn journal_originals(
    graph: &std::path::Path,
    row: &Value,
    entry: Option<&ManifestEntry>,
    reason: &str,
) -> Result<usize, String> {
    let id = graph_store::entry_id(row).unwrap_or_default();
    let mut count = 0usize;
    for (i, note) in notes_of(row).iter().enumerate() {
        note_history::append(
            graph,
            id,
            reason,
            Some(node_state::row_revision(row)),
            Some(i as u64),
            note,
            entry.and_then(|e| e.session_id.as_deref()),
            entry.and_then(|e| e.harness.as_deref()),
        )?;
        count += 1;
    }
    if let (Some(entry), Some(orig)) = (entry, row.get("details").filter(|d| d.is_string())) {
        if let Some(short) = &entry.details {
            if node_state::count_prose(short) < char_len(Some(orig)) {
                note_history::append(
                    graph,
                    id,
                    note_history::REASON_DETAILS_ARCHIVED,
                    Some(node_state::row_revision(row)),
                    None,
                    &json!({ "details": orig }),
                    entry.session_id.as_deref(),
                    entry.harness.as_deref(),
                )?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// The receipt's shape: counts the plan names, positive on every run.
fn print_receipt(
    json_out: bool,
    scanned: usize,
    archived_notes: usize,
    digested_nodes: usize,
    legacy_details_archived: usize,
    unchanged: usize,
    unresolved: &[(String, String)],
    history_verified: usize,
) {
    if json_out {
        println!(
            "{}",
            json!({
                "scanned": scanned,
                "archived_notes": archived_notes,
                "digested_nodes": digested_nodes,
                "legacy_details_archived": legacy_details_archived,
                "unchanged": unchanged,
                "unresolved": unresolved.iter().map(|(id, r)| json!({"node_id": id, "reason": r})).collect::<Vec<_>>(),
                "history_verified": history_verified,
            })
        );
    } else {
        for (id, r) in unresolved {
            eprintln!("unresolved {id}: {r}");
        }
        println!(
            "scanned={scanned} archived_notes={archived_notes} digested_nodes={digested_nodes} legacy_details_archived={legacy_details_archived} unchanged={unchanged} unresolved={} history_verified={history_verified}",
            unresolved.len()
        );
    }
}

/// Preview or apply the manifest. Preview changes nothing; apply journals
/// each row's originals under the publication lock, replaces the hot row,
/// and verifies the readback before counting the row migrated.
fn run_migrate(
    graph: &std::path::Path,
    manifest_path: Option<&std::path::Path>,
    apply: bool,
    json_out: bool,
) -> i32 {
    let entries = match graph_store::read_rows(graph) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fno-agents backlog-notes: {e}");
            return 1;
        }
    };
    let manifest = match manifest_path {
        Some(p) => match parse_manifest(p) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("fno-agents backlog-notes: {e}");
                return 1;
            }
        },
        None if apply => {
            eprintln!("fno-agents backlog-notes: --apply needs --manifest <file>");
            return 2;
        }
        None => Vec::new(),
    };

    let mut unresolved: Vec<(String, String)> = Vec::new();
    let mut unchanged = 0usize;
    let mut work: Vec<(String, RowVerdict)> = Vec::new();
    for row in &entries {
        let Some(id) = graph_store::entry_id(row).map(str::to_string) else {
            continue;
        };
        let has_notes = !notes_of(row).is_empty();
        let named = manifest.iter().any(|m| m.node_id == id);
        let migrated = row.get(HISTORY_MARKER_KEY).is_some();
        if !has_notes && !named && !migrated {
            continue;
        }
        match judge(row, &manifest) {
            RowVerdict::Unchanged => unchanged += 1,
            RowVerdict::Unresolved(r) => unresolved.push((id, r)),
            v => work.push((id, v)),
        }
    }

    if !apply {
        let digested = work.len();
        print_receipt(
            json_out,
            entries.len(),
            notes_of_all(&entries),
            digested,
            0,
            unchanged,
            &unresolved,
            0,
        );
        return if unresolved.is_empty() { 0 } else { 1 };
    }

    let mut digested_nodes = 0usize;
    let mut archived_notes = 0usize;
    let mut details_archived = 0usize;
    let mut verified = 0usize;
    for (id, verdict) in &work {
        let entry: Option<ManifestEntry> = match verdict {
            RowVerdict::Digested(e) => Some(e.clone()),
            RowVerdict::TerminalVerbatim => None,
            _ => continue,
        };
        // Re-read and re-judge UNDER the lock, journal originals, then let the
        // candidate publish. A stale or refusing row leaves the row intact.
        let journaled_count = std::cell::Cell::new(0usize);
        let mut hook = |raw: &[Value]| -> Result<(), graph_store::StoreError> {
            let row = raw
                .iter()
                .find(|r| graph_store::entry_id(r) == Some(id.as_str()))
                .ok_or_else(|| graph_store::StoreError::Invalid(format!("{id} vanished")))?;
            match judge(row, &manifest) {
                RowVerdict::Digested(_) | RowVerdict::TerminalVerbatim => {}
                RowVerdict::Unchanged => {
                    return Err(graph_store::StoreError::Invalid(format!(
                        "{id} already migrated"
                    )))
                }
                RowVerdict::Unresolved(r) => {
                    return Err(graph_store::StoreError::Invalid(format!("{id}: {r}")))
                }
            }
            let reason = if matches!(verdict, RowVerdict::TerminalVerbatim) {
                note_history::REASON_TERMINAL_EVACUATED
            } else {
                note_history::REASON_NOTE_MIGRATED
            };
            let n = journal_originals(graph, row, entry.as_ref(), reason).map_err(|e| {
                graph_store::StoreError::Invalid(format!("history write failed: {e}"))
            })?;
            journaled_count.set(n);
            Ok(())
        };
        let published = graph_store::mutate_rows(
            graph,
            std::time::Duration::from_secs(30),
            None,
            Some(&mut hook),
            |rows| {
                // Build the candidate from the FRESH read, never the
                // pre-loop snapshot that republished stale rows.
                let candidate = build_candidate(
                    rows,
                    id,
                    entry.as_ref(),
                    matches!(verdict, RowVerdict::TerminalVerbatim),
                );
                *rows = candidate;
                Ok(true)
            },
        );
        if published.is_err() {
            unresolved.push((id.clone(), "apply failed; row left intact".into()));
            continue;
        }
        // Verify the readback before counting the row migrated.
        let expected = manifest
            .iter()
            .find(|m| m.node_id == *id)
            .map(|_| ())
            .is_some();
        let _ = expected;
        let rows_now = graph_store::read_rows(graph).unwrap_or_default();
        let row_now = rows_now
            .iter()
            .find(|r| graph_store::entry_id(r) == Some(id.as_str()));
        let clean = row_now
            .map(|r| notes_of(r).is_empty() && r.get(HISTORY_MARKER_KEY).is_some())
            .unwrap_or(false);
        let (_, history_count) =
            note_history::read(graph, Some(id), 0, usize::MAX).unwrap_or((Vec::new(), 0));
        let journaled = journaled_count.get();
        if clean && history_count > 0 && journaled > 0 {
            digested_nodes += 1;
            verified += 1;
            // Count what THIS run journaled, not the node's whole history:
            // a node with prior state revisions already carries records.
            archived_notes += journaled;
            if entry.as_ref().map(|e| e.details.is_some()).unwrap_or(false) {
                details_archived += 1;
            }
        } else {
            unresolved.push((id.clone(), "readback verification failed".into()));
        }
    }
    print_receipt(
        json_out,
        entries.len(),
        archived_notes,
        digested_nodes,
        details_archived,
        unchanged,
        &unresolved,
        verified,
    );
    if unresolved.is_empty() {
        0
    } else {
        1
    }
}

fn notes_of_all(entries: &[Value]) -> usize {
    entries.iter().map(|r| notes_of(r).len()).sum()
}

/// The candidate rows: exactly one row changes (notes gone, state/details/
/// marker applied); every other row passes through untouched.
fn build_candidate(
    entries: &[Value],
    node_id: &str,
    entry: Option<&ManifestEntry>,
    verbatim: bool,
) -> Vec<Value> {
    let mut out = Vec::with_capacity(entries.len());
    for row in entries {
        let mut row = row.clone();
        if graph_store::entry_id(&row) != Some(node_id) {
            out.push(row);
            continue;
        }
        let revision = node_state::row_revision(&row) + 1;
        if let Some(obj) = row.as_object_mut() {
            obj.remove("progress_notes");
            if let (Some(entry), Some(state)) = (entry, entry.and_then(|e| e.state.as_deref())) {
                obj.insert(
                    STATE_KEY.into(),
                    json!({
                        "body": state,
                        "revision": revision,
                        "updated_at": graph_store::now_isoformat(),
                        "source_session_id": entry.session_id,
                        "source_harness": entry.harness,
                    }),
                );
            }
            if let Some(details) = entry.and_then(|e| e.details.as_deref()) {
                obj.insert("details".into(), json!(details));
            }
            if verbatim {
                obj.remove(STATE_KEY);
            }
            obj.insert(
                HISTORY_MARKER_KEY.into(),
                json!({"migrated_at": graph_store::now_isoformat()}),
            );
        }
        out.push(row);
    }
    out
}

/// The usage text `--help` prints; names every command (the gate reads it).
fn print_usage() {
    println!(
        "usage: backlog-notes <command> [flags]

commands:
  inventory
        census of legacy progress_notes per node (backend, counts, chars)
  migrate --manifest <path> [--apply]
        carry each row's notes into the journal; dry run without --apply
  history [<node>|<slug>]
        read a node's note journal, oldest first
  stale <node> --plan <path>
        report notes newer than the plan's last commit (else its mtime)

flags:
  --node <id>              node for history or stale (a positional token also works)
  --plan <path>            plan file for stale
  --offset N --limit N     page the history read (default limit 50)
  --json                   machine output; history emits {{total, offset, records}}
  --graph <path>           store to read (default ~/.fno/graph.json)
  -h, --help               this text"
    );
}

/// `backlog-notes inventory|migrate|history|stale`.
pub fn run_notes(args: &[String]) -> i32 {
    let mut action = String::new();
    let mut graph: Option<PathBuf> = None;
    let mut manifest: Option<PathBuf> = None;
    let mut plan: Option<PathBuf> = None;
    let mut apply = false;
    let mut json_out = false;
    let mut node: Option<String> = None;
    let mut positional: Option<String> = None;
    let mut offset = 0usize;
    let mut limit = 50usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "inventory" | "migrate" | "history" | "stale" if action.is_empty() => {
                action = args[i].clone();
            }
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            "--graph" => {
                i += 1;
                match args.get(i) {
                    Some(v) => graph = Some(PathBuf::from(v)),
                    None => {
                        eprintln!("fno-agents backlog-notes: --graph needs a path");
                        return 2;
                    }
                }
            }
            "--manifest" => {
                i += 1;
                match args.get(i) {
                    Some(v) => manifest = Some(PathBuf::from(v)),
                    None => {
                        eprintln!("fno-agents backlog-notes: --manifest needs a path");
                        return 2;
                    }
                }
            }
            "--node" => {
                i += 1;
                match args.get(i) {
                    Some(v) => node = Some(v.clone()),
                    None => {
                        eprintln!("fno-agents backlog-notes: --node needs an id");
                        return 2;
                    }
                }
            }
            "--offset" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => offset = v,
                    None => {
                        eprintln!("fno-agents backlog-notes: --offset needs a number");
                        return 2;
                    }
                }
            }
            "--limit" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => limit = v,
                    None => {
                        eprintln!("fno-agents backlog-notes: --limit needs a number");
                        return 2;
                    }
                }
            }
            "--plan" => {
                i += 1;
                match args.get(i) {
                    Some(v) => plan = Some(PathBuf::from(v)),
                    None => {
                        eprintln!("fno-agents backlog-notes: --plan needs a path");
                        return 2;
                    }
                }
            }
            "--apply" => apply = true,
            "--json" | "-J" => json_out = true,
            other
                if (action == "history" || action == "stale")
                    && positional.is_none()
                    && !other.starts_with('-') =>
            {
                positional = Some(other.to_string());
            }
            other => {
                eprintln!("fno-agents backlog-notes: unknown argument {other}");
                return 2;
            }
        }
        i += 1;
    }
    if positional.is_some() && node.is_some() {
        eprintln!(
            "fno-agents backlog-notes: pass the node either positionally or via --node, not both"
        );
        return 2;
    }
    if node.is_none() {
        node = positional;
    }
    let graph = graph.unwrap_or_else(default_graph_path);
    // Resolve the token (positional or --node) against the graph for the
    // trailer: canonical id, current_state revision, legacy-note count. The
    // journal still answers when the graph does not know the token (an
    // archived node's history outlives its row).
    let mut row = None;
    if let Some(tok) = node.as_deref() {
        if let Ok(entries) = graph_store::read_rows(&graph) {
            row = crate::graph_get::find_entry(&entries, tok).cloned();
        }
    }
    if let Some(r) = &row {
        if let Some(id) = graph_store::entry_id(r) {
            // A slug token filters the journal under the row's canonical id.
            node = Some(id.to_string());
        }
    }
    match action.as_str() {
        "inventory" => run_inventory(&graph, json_out),
        "history" => run_history(
            &graph,
            node.as_deref(),
            row.as_ref(),
            offset,
            limit,
            json_out,
        ),
        "stale" => super::note_stale::run_stale(&graph, node.as_deref(), plan.as_deref(), json_out),
        "migrate" => run_migrate(&graph, manifest.as_deref(), apply, json_out),
        _ => {
            print_usage();
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_short_json_spelling_parses_like_the_long_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, serde_json::json!({"entries": []}).to_string()).unwrap();
        let base = vec![
            "inventory".to_string(),
            "--graph".to_string(),
            graph.display().to_string(),
        ];
        assert_eq!(run_notes(&base), 0);
        let mut short = base.clone();
        short.insert(0, "-J".to_string());
        assert_eq!(run_notes(&short), 0);
        // An unknown flag still refuses with usage.
        let mut bogus = base.clone();
        bogus.insert(0, "--bogus".to_string());
        assert_eq!(run_notes(&bogus), 2);
    }
}
