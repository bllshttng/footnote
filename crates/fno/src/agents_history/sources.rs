use crate::{agents_view, event_store, store_client};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(super) type Rows = Result<Vec<Value>, String>;

pub(super) struct Sources {
    pub(super) registry: Rows,
    pub(super) receipts: Result<Vec<Receipt>, String>,
    pub(super) graph: Rows,
    pub(super) ledger: Rows,
    pub(super) events: Rows,
    pub(super) repo_slug: Option<String>,
}

pub(super) struct Receipt {
    pub(super) path: PathBuf,
    pub(super) value: Value,
}
pub(super) struct Paths {
    pub(super) arg: String,
    pub(super) graph: PathBuf,
    pub(super) ledger: PathBuf,
    pub(super) events: PathBuf,
    pub(super) agents_home: PathBuf,
    pub(super) repo_slug: Option<String>,
}
pub(super) fn load_sources(paths: &Paths) -> Sources {
    let registry = read_rows(&registry_path(&paths.agents_home), "agents");
    let receipts = read_receipts(&paths.agents_home.join("reap-receipts"));
    let graph = store_client::rows(&paths.graph, None, None);
    let ledger = read_rows(&paths.ledger, "entries");
    let events = Ok(read_events(&paths.agents_home, &paths.events));
    Sources {
        registry,
        receipts,
        graph,
        ledger,
        events,
        repo_slug: paths.repo_slug.clone(),
    }
}

fn registry_path(agents_home: &Path) -> PathBuf {
    let resolved = agents_view::registry_path();
    if resolved.parent() == Some(agents_home) {
        resolved
    } else {
        agents_home.join("registry.json")
    }
}

fn read_rows(path: &Path, key: &str) -> Rows {
    let raw = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|err| format!("{}: invalid JSON: {err}", path.display()))?;
    if let Some(rows) = value.as_array() {
        return Ok(rows.clone());
    }
    value
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| format!("{}: missing {key} array", path.display()))
}

fn read_receipts(dir: &Path) -> Result<Vec<Receipt>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("{}: {err}", dir.display())),
    };
    let mut receipts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str(&raw) else {
            continue;
        };
        receipts.push(Receipt { path, value });
    }
    Ok(receipts)
}

fn read_events(agents_home: &Path, global_events: &Path) -> Vec<Value> {
    let agent_text = event_store::journal_text(
        &agents_home.join("events.jsonl"),
        &["agent_spawned", "agent_removed", "agent_row_reaped"],
    );
    let global_text = event_store::journal_text(
        global_events,
        &["agent_resumed", "agent_resume_failed", "agent_send_started"],
    );
    parse_json_lines(&agent_text)
        .into_iter()
        .chain(parse_json_lines(&global_text))
        .collect()
}

fn parse_json_lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
