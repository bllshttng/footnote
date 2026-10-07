//! Upgrade stored role vocabulary before any runtime reader opens it.
//! This is the sole compatibility boundary; normal readers use only current keys.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const WORDS: &[(&str, &str)] = &[
    ("king", "lead"),
    ("kings", "leads"),
    ("crown", "role"),
    ("crowns", "roles"),
    ("crowned", "promoted"),
    ("crowning", "promoting"),
    ("uncrowned", "unpromoted"),
    ("crownless", "unassigned"),
    ("heir", "successor"),
    ("heirs", "successors"),
    ("court", "team"),
    ("courts", "teams"),
    ("reign", "term"),
    ("reigns", "terms"),
    ("reigning", "serving"),
    ("regnal", "generation"),
    ("coronation", "promotion"),
    ("coronations", "promotions"),
    ("coronate", "promote"),
    ("coronated", "promoted"),
    ("coronating", "promoting"),
    ("abdicate", "step_down"),
    ("abdicates", "steps_down"),
    ("abdicated", "stepped_down"),
    ("abdicating", "stepping_down"),
    ("abdication", "departure"),
    ("bestow", "grant"),
    ("bestowed", "granted"),
    ("bestows", "grants"),
    ("bestowing", "granting"),
    ("abdicat", "step_down"),
];

fn vocabulary(text: &str) -> String {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PATTERN.get_or_init(|| regex::Regex::new(r"[A-Za-z]+").expect("constant pattern"));
    let replaced = re
        .replace_all(text, |cap: &regex::Captures<'_>| {
            let word = &cap[0];
            let Some((_, new)) = WORDS
                .iter()
                .find(|(old, _)| *old == word.to_ascii_lowercase())
            else {
                return word.to_string();
            };
            if word.bytes().all(|b| b.is_ascii_uppercase()) {
                new.to_ascii_uppercase()
            } else if word.as_bytes()[0].is_ascii_uppercase() {
                let mut shown = new.to_string();
                shown[..1].make_ascii_uppercase();
                shown
            } else {
                new.to_string()
            }
        })
        .into_owned();
    replaced
        .replace("term_armed", "lead_armed")
        .replace("term_checkin", "lead_checkin")
        .replace("term_dispatch_exception", "lead_dispatch_exception")
        .replace("role_succession_reverted", "team_succession_reverted")
        .replace("role_ledger", "team_ledger")
        .replace("term_eval", "lead_eval")
        .replace("fno agents lead", "fno agents org")
}

#[doc(hidden)]
pub fn migrate_node_provenance(value: &mut Value) -> Result<bool, String> {
    let Some(map) = value.as_object_mut() else {
        return Ok(false);
    };
    let Some(legacy) = map.remove("source_crown") else {
        return Ok(false);
    };
    if let Some(current) = map.get("source_team") {
        if current != &legacy {
            return Err("conflicting source_team provenance; migration refused".into());
        }
    } else {
        map.insert("source_team".into(), legacy);
    }
    Ok(true)
}

fn migrate_value(value: &mut Value, field: &str) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            let old = std::mem::take(map);
            for (key, mut value) in old {
                let new = vocabulary(&key);
                migrate_value(&mut value, &new)?;
                if let Some(existing) = map.get(&new) {
                    if existing != &value {
                        return Err(format!(
                            "conflicting stored fields for {new}; migration refused"
                        ));
                    }
                } else {
                    map.insert(new, value);
                }
            }
            let sorted = std::mem::take(map).into_iter().collect::<BTreeMap<_, _>>();
            map.extend(sorted);
        }
        Value::Array(items) => {
            for item in items {
                migrate_value(item, field)?;
            }
        }
        Value::String(text)
            if matches!(
                field,
                "type"
                    | "event"
                    | "arm"
                    | "lane"
                    | "driver"
                    | "kind"
                    | "phase"
                    | "reason"
                    | "checkin_text"
                    | "manifest_path"
                    | "state_path"
                    | "cancel_path"
            ) =>
        {
            *text = vocabulary(text)
        }
        _ => (),
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("role-upgrade-{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        if let Ok(meta) = std::fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e: std::io::Error| format!("{}: {e}", path.display()))
}

fn migrate_file(path: &Path) -> Result<(), String> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let sidecar = if path.file_name().and_then(|s| s.to_str()) == Some("registry.json") {
        let dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("locks");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        dir.join("_registry.lock")
    } else {
        PathBuf::from(lock_path)
    };
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sidecar)
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let extension = if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("events.jsonl"))
    {
        "jsonl"
    } else {
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    };
    let output = match extension {
        "json" => {
            let mut value: Value =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            if value.get("agents").is_some()
                && value
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .is_some_and(|v| v > 40)
            {
                return Ok(());
            }
            let original = value.clone();
            migrate_value(&mut value, "")?;
            if path.file_name().and_then(|n| n.to_str()) == Some("registry.json") {
                if let Some(map) = value.as_object_mut() {
                    if map.contains_key("agents") && map.contains_key("schema_version") {
                        let floor = map
                            .get("min_writer_version")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        map.insert("schema_version".into(), Value::from(40));
                        map.insert("min_writer_version".into(), Value::from(floor.max(40)));
                    }
                }
            }
            if value == original {
                return Ok(());
            }
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())? + "\n"
        }
        "jsonl" => {
            let mut out = String::new();
            for line in text.split_inclusive('\n') {
                let Ok(mut value) = serde_json::from_str::<Value>(line) else {
                    out.push_str(line);
                    continue;
                };
                let original = value.clone();
                migrate_value(&mut value, "")?;
                if value == original {
                    out.push_str(line);
                } else {
                    out.push_str(&serde_json::to_string(&value).map_err(|e| e.to_string())?);
                    if line.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            out
        }
        "toml" => {
            let document: toml::Value =
                toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut value = serde_json::to_value(document).map_err(|e| e.to_string())?;
            let original = value.clone();
            migrate_value(&mut value, "")?;
            if value == original {
                return Ok(());
            }
            let document: toml::Value = serde_json::from_value(value).map_err(|e| e.to_string())?;
            toml::to_string_pretty(&document).map_err(|e| e.to_string())?
        }
        "md" => {
            text.lines()
                .map(|line| {
                    if let Some((key, rest)) = line.split_once(':') {
                        if !key.is_empty()
                            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        {
                            return format!("{}:{rest}", vocabulary(key));
                        }
                    }
                    line.to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        }
        _ => return Ok(()),
    };
    if output != text {
        atomic_write(path, output.as_bytes())?;
    }
    Ok(())
}

fn selected(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    matches!(
        name,
        "events.db"
            | "registry.json"
            | "team_names.json"
            | "crown_names.json"
            | "events.jsonl"
            | "config.toml"
    ) || ((name.starts_with("lead-") || name.starts_with("king-") || name.starts_with("reign-"))
        && matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("json" | "jsonl" | "md")
        ))
}

fn walk(root: &Path, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Ok(());
    }
    for entry in std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            // A relocated spaces root is still this root's spaces; skipping
            // it would stamp the marker over unmigrated role dirs.
            if depth == 0 && entry.file_name() == "spaces" && path.is_dir() {
                walk(&path, depth + 1)?;
            }
            continue;
        }
        if kind.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            let parent_is_container = root.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
                matches!(
                    s,
                    "spaces" | "worktrees" | "king" | "lead" | "kings" | "leads"
                )
            });
            if parent_is_container
                || matches!(
                    name.as_str(),
                    "agents"
                        | "spaces"
                        | "worktrees"
                        | "king"
                        | "lead"
                        | "kings"
                        | "leads"
                        | "events"
                        | "state"
                )
            {
                walk(&path, depth + 1)?;
                let current = vocabulary(&name);
                if current != name {
                    let target = path.with_file_name(current);
                    if target.exists() {
                        merge_dir(&path, &target)?;
                    } else {
                        std::fs::rename(&path, target).map_err(|e| e.to_string())?;
                    }
                }
            }
        } else if kind.is_file() && selected(&path) {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let current = if name == "crown_names.json" {
                "team_names.json".to_string()
            } else {
                vocabulary(name)
            };
            let target = path.with_file_name(&current);
            // Judge the collision before rewriting: a refused rename must
            // leave the legacy file byte-for-byte as it was found.
            if current != name && target.exists() {
                if name != "crown_names.json" {
                    return Err(format!(
                        "both role files exist at {}; migration refused",
                        path.display()
                    ));
                }
                // The name store moved to team_names.json before this
                // migration, so a surviving crown_names.json is an older
                // generation. The live store wins; the old one is kept.
                std::fs::rename(&path, superseded_backup(&path)).map_err(|e| e.to_string())?;
                continue;
            }
            if name == "events.db" {
                crate::event_store::upgrade_role_store(&path)?;
            } else {
                migrate_file(&path)?;
            }
            if current != name {
                std::fs::rename(&path, target).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Fold a legacy role directory into a current one that new code already
/// wrote. As with the name store, the current entry is live: a name in both
/// keeps it and parks the legacy entry beside it as `.superseded`.
fn merge_dir(legacy: &Path, current: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(legacy).map_err(|e| format!("{}: {e}", legacy.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let mut dest = current.join(entry.file_name());
        if std::fs::symlink_metadata(&dest).is_ok() {
            dest = superseded_backup(&dest);
        }
        std::fs::rename(entry.path(), dest).map_err(|e| e.to_string())?;
    }
    std::fs::remove_dir(legacy).map_err(|e| format!("{}: {e}", legacy.display()))
}

fn superseded_backup(path: &Path) -> PathBuf {
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("store");
    let mut n = 0;
    loop {
        let candidate = path.with_file_name(match n {
            0 => format!("{stem}.superseded"),
            _ => format!("{stem}.superseded.{n}"),
        });
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

pub fn run_at(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let marker = root.join("migrations/role-vocabulary-v1.done");
    if marker.exists() {
        return Ok(());
    }
    crate::live_store_fence::refuse_worktree_build_on_operator_store(root)?;
    std::fs::create_dir_all(marker.parent().expect("marker has parent"))
        .map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(marker.with_extension("lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    if marker.exists() {
        return Ok(());
    }
    walk(root, 0)?;
    atomic_write(&marker, b"1\n")
}

/// The roots one migration run walks. Readers find spaces at
/// `<FNO_AGENTS_HOME parent>/spaces` unless `FNO_SPACES_DIR` names them, so
/// that parent is a root too when it holds spaces; without it a daemon
/// pinned to its agents home never migrates the space role directories.
fn state_roots(var: impl Fn(&str) -> Option<PathBuf>) -> BTreeSet<PathBuf> {
    let mut roots = BTreeSet::new();
    for key in ["FNO_STATE_DIR", "FNO_AGENTS_HOME", "FNO_SPACES_DIR"] {
        if let Some(path) = var(key) {
            roots.insert(path);
        }
    }
    if var("FNO_SPACES_DIR").is_none() {
        if let Some(parent) = var("FNO_AGENTS_HOME")
            .as_deref()
            .and_then(Path::parent)
            .filter(|p| p.join("spaces").is_dir())
        {
            roots.insert(parent.to_path_buf());
        }
    }
    roots
}

pub fn run() -> Result<(), String> {
    let mut roots = state_roots(|key| {
        std::env::var_os(key)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });
    let explicit_roots = !roots.is_empty();
    if !explicit_roots {
        if let Some(root) = crate::live_store_fence::operator_state_root() {
            roots.insert(root);
        }
    }
    if let Some(config) = std::env::var_os("FNO_CONFIG").filter(|s| !s.is_empty()) {
        let config = PathBuf::from(config);
        if config.is_file() {
            crate::live_store_fence::refuse_worktree_build_on_operator_store(&config)?;
            migrate_file(&config)?;
        }
    }
    if !explicit_roots {
        if let Ok(cwd) = std::env::current_dir() {
            roots.insert(cwd.join(".fno"));
        }
    }
    for root in roots {
        run_at(&root)?;
    }
    Ok(())
}

pub fn upgrade_event_store(conn: &mut rusqlite::Connection) -> Result<(), String> {
    use sha2::Digest;
    let ready: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM events_meta WHERE key = 'role_vocabulary_v1')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if ready {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let done: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM events_meta WHERE key = 'role_vocabulary_v1')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if done {
        return Ok(());
    }
    let rows = {
        let mut query = tx
            .prepare("SELECT seq, type, line FROM events")
            .map_err(|e| e.to_string())?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    for (seq, event_type, line) in rows {
        let current_type = vocabulary(&event_type);
        let current_line = match serde_json::from_str::<Value>(&line) {
            Ok(mut value) => {
                let original = value.clone();
                migrate_value(&mut value, "")?;
                if value != original {
                    serde_json::to_string(&value).map_err(|e| e.to_string())?
                } else {
                    line.clone()
                }
            }
            Err(_) => line.clone(),
        };
        if current_type != event_type || current_line != line {
            let hash = sha2::Sha256::digest(current_line.as_bytes()).to_vec();
            tx.execute(
                "UPDATE events SET type = ?1, line = ?2, row_hash = CASE WHEN EXISTS(SELECT 1 FROM events other WHERE other.row_hash = ?3 AND other.seq <> ?4) THEN row_hash ELSE ?3 END WHERE seq = ?4",
                rusqlite::params![current_type, current_line, hash, seq],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    tx.execute(
        "INSERT INTO events_meta(key, value) VALUES ('role_vocabulary_v1', '1')",
        [],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

pub fn contains_legacy_vocabulary(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphabetic())
        .any(|word| WORDS.iter().any(|(old, _)| word.eq_ignore_ascii_case(old)))
}

pub fn retired_verb(verb: &str) -> Option<&'static str> {
    match verb {
        "king" => Some("fno agents org <action>"),
        "king-checkin" => Some("fno agents org checkin"),
        "king-history" => Some("fno agents org history"),
        "crown" => Some("fno agents org promote <session> --scope <scope>"),
        "court" => Some("fno agents org"),
        "court-fold" => Some("fno agents org fold"),
        "court-orphans" => Some("fno agents org vacancies"),
        "reign-ledger" => Some("fno agents org rundown"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_preserves_authority_and_history_refuses_conflicts_and_is_once_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.json");
        std::fs::write(&path, r#"{"schema_version":39,"agents":[{"name":"stable-row","crown_level":2,"crown_scope":"epic-alpha","crown_grantor":"human"}]}"#).unwrap();
        let events = tmp.path().join("events.jsonl");
        std::fs::write(
            &events,
            "{\"type\":\"reign_checkin\",\"data\":{\"heir_name\":\"Avery\"}}\n",
        )
        .unwrap();
        run_at(tmp.path()).unwrap();
        let migrated = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&migrated).unwrap();
        assert_eq!(value["agents"][0]["role_scope"], "epic-alpha");
        assert_eq!(value["agents"][0]["role_level"], 2);
        assert_eq!(value["agents"][0]["role_grantor"], "human");
        assert_eq!(value["agents"][0]["name"], "stable-row");
        assert_eq!(value["schema_version"], 40);
        assert!(!migrated.contains("crown"));
        assert!(std::fs::read_to_string(events)
            .unwrap()
            .contains("lead_checkin"));
        run_at(tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), migrated);
        let mut history = serde_json::json!({"data":{"registry":{"schema_version":39,"agents":[{"crown_scope":"epic-alpha"}]}}});
        migrate_value(&mut history, "").unwrap();
        assert_eq!(history["data"]["registry"]["schema_version"], 39);
        assert_eq!(
            history["data"]["registry"]["agents"][0]["role_scope"],
            "epic-alpha"
        );
        let mut conflict = serde_json::json!({"crown_scope":"old","role_scope":"new"});
        assert!(migrate_value(&mut conflict, "").is_err());
        assert_eq!(retired_verb("king"), Some("fno agents org <action>"));
        assert_eq!(retired_verb("lead"), None);
        let mut node = serde_json::json!({"source_crown":"L1 project", "title":"preserved"});
        assert!(migrate_node_provenance(&mut node).unwrap());
        assert_eq!(node["source_team"], "L1 project");
        assert_eq!(node["title"], "preserved");
        assert!(!migrate_node_provenance(&mut node).unwrap());
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE events_meta(key TEXT PRIMARY KEY, value TEXT); CREATE TABLE events(seq INTEGER PRIMARY KEY, event_id TEXT, type TEXT, line TEXT, row_hash BLOB UNIQUE);").unwrap();
        db.execute(
            "INSERT INTO events VALUES(1, 'stable-id', 'reign_checkin', ?1, x'01')",
            [r#"{"type":"reign_checkin","data":{"heir_name":"Avery"}}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO events VALUES(2, 'second-id', 'lead_checkin', ?1, x'02')",
            [r#"{"type":"lead_checkin","data":{"successor_name":"Avery"}}"#],
        )
        .unwrap();
        upgrade_event_store(&mut db).unwrap();
        upgrade_event_store(&mut db).unwrap();
        let row: (String, String, String) = db
            .query_row(
                "SELECT event_id, type, line FROM events WHERE seq = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "stable-id");
        assert_eq!(
            db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT event_id FROM events WHERE seq = 2", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "second-id"
        );
        assert_eq!(row.1, "lead_checkin");
        assert!(row.2.contains("successor_name"));
        assert!(!row.2.contains("heir_name"));
    }

    #[test]
    fn a_superseded_name_store_yields_to_the_live_one_and_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let agents = tmp.path().join("agents");
        std::fs::create_dir(&agents).unwrap();
        let legacy = r#"{"version":1,"crowns":{"fno":{"name":"Old","regnal":1}}}"#;
        std::fs::write(agents.join("crown_names.json"), legacy).unwrap();
        std::fs::write(
            agents.join("team_names.json"),
            r#"{"version":1,"teams":{"fno":{"name":"Live","regnal":2}}}"#,
        )
        .unwrap();
        run_at(tmp.path()).unwrap();
        assert!(!agents.join("crown_names.json").exists());
        assert_eq!(
            std::fs::read_to_string(agents.join("crown_names.json.superseded")).unwrap(),
            legacy
        );
        let live = std::fs::read_to_string(agents.join("team_names.json")).unwrap();
        assert!(live.contains("Live") && live.contains("generation"));
    }

    #[test]
    fn a_legacy_role_dir_folds_into_the_live_one_and_keeps_both_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let space = tmp.path().join("spaces").join("repo");
        let (kings, leads) = (space.join("kings"), space.join("leads"));
        std::fs::create_dir_all(&kings).unwrap();
        std::fs::create_dir_all(&leads).unwrap();
        std::fs::write(kings.join("fno.md"), "scope: fno\nshape: court\n").unwrap();
        std::fs::write(kings.join("ops.md"), "scope: ops\n").unwrap();
        std::fs::write(leads.join("fno.md"), "scope: fno\nshape: team\n").unwrap();
        run_at(tmp.path()).unwrap();
        assert!(!kings.exists());
        assert_eq!(
            std::fs::read_to_string(leads.join("fno.md")).unwrap(),
            "scope: fno\nshape: team\n"
        );
        assert!(std::fs::read_to_string(leads.join("fno.md.superseded"))
            .unwrap()
            .contains("court"));
        assert_eq!(
            std::fs::read_to_string(leads.join("ops.md")).unwrap(),
            "scope: ops\n"
        );

        let state = tmp.path().join("linked");
        let moved = tmp.path().join("moved-spaces");
        std::fs::create_dir_all(moved.join("repo").join("kings")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink(&moved, state.join("spaces")).unwrap();
        run_at(&state).unwrap();
        assert!(moved.join("repo").join("leads").is_dir());
        assert!(!moved.join("repo").join("kings").exists());
    }

    #[test]
    fn a_pinned_agents_home_still_walks_the_spaces_beside_it() {
        fn env(pairs: &[(&str, &str)], key: &str) -> Option<PathBuf> {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| PathBuf::from(v))
        }
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("bare");
        let agents = state.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let home = agents.to_str().unwrap();
        let roots = state_roots(|k| env(&[("FNO_AGENTS_HOME", home)], k));
        assert!(!roots.contains(&state), "no spaces beside the home");
        std::fs::create_dir(state.join("spaces")).unwrap();
        let roots = state_roots(|k| env(&[("FNO_AGENTS_HOME", home)], k));
        assert!(roots.contains(&state));
        let pinned = [("FNO_AGENTS_HOME", home), ("FNO_SPACES_DIR", "/t/spaces")];
        let roots = state_roots(|k| env(&pinned, k));
        assert!(!roots.contains(&state));
        assert!(roots.contains(Path::new("/t/spaces")));
    }

    #[test]
    fn a_refused_collision_leaves_the_legacy_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let kings = tmp.path().join("kings");
        std::fs::create_dir(&kings).unwrap();
        let legacy = r#"{"crown_scope":"x"}"#;
        std::fs::write(kings.join("king-a.json"), legacy).unwrap();
        std::fs::write(kings.join("lead-a.json"), "{}").unwrap();
        assert!(run_at(tmp.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(kings.join("king-a.json")).unwrap(),
            legacy
        );
    }
}
