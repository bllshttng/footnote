//! Owner of agy's hooks.json: read its real state, install footnote's
//! handlers without touching anything else, and refuse (writing nothing)
//! over bytes that do not parse. The Python installer this replaces set
//! `data = {}` on a parse error and wrote, which destroyed a
//! malformed-but-foreign file with no backup and reported `installed`
//! anyway. Explicit paths only: no `$HOME` read inside, so tests pin a
//! tempdir and the real global file is unreachable from them.

use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The honest state of footnote's registration in one hooks file. `loaded`
/// and `runtime` are always `unverified`: whether agy loaded or ran the hook
/// is a live-process fact no static read can know.
#[derive(Debug, Serialize)]
pub struct HookStatus {
    /// "ok" | "absent" | "not_object" | "malformed"
    pub file: String,
    pub file_error: Option<String>,
    pub file_line: Option<usize>,
    pub file_column: Option<usize>,
    /// "absent" | "not_object" | "configured"
    pub footnote: String,
    /// agy's schema default; only an explicit false disables.
    pub enabled: bool,
    /// "matches" | "stale" | "unverifiable"
    pub stop: String,
    /// "matches" | "missing" | "not_shipped"
    pub team: String,
    /// "matches" | "missing" | "not_shipped"
    pub guard: String,
    /// footnote handlers whose script is gone; any one blocks `installed`.
    pub dead: usize,
    pub loaded: &'static str,
    pub runtime: &'static str,
    pub installed: bool,
}

#[derive(Debug, Serialize)]
pub struct InstallReceipt {
    pub status: &'static str,
    pub note: String,
    pub hooks_file: String,
    pub enabled: bool,
}

/// What one hooks-file read found, before interpretation.
enum Root {
    Ok(Map<String, Value>),
    Absent,
    NotObject,
    Malformed(serde_json::Error),
}

fn read_root(hooks_file: &Path) -> Root {
    match std::fs::read_to_string(hooks_file) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Root::Ok(map),
            Ok(_) => Root::NotObject,
            Err(e) => Root::Malformed(e),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Root::Absent,
        Err(e) => Root::Malformed(serde_json::Error::io(e)),
    }
}

fn has_handler(list: Option<&Value>, command: &Path) -> bool {
    list.and_then(Value::as_array)
        .map(|entries| {
            entries.iter().any(|h| {
                h.get("command").and_then(Value::as_str) == Some(command.to_string_lossy().as_ref())
            })
        })
        .unwrap_or(false)
}

/// The grouped shape (PreToolUse/PostToolUse) carries handlers under each
/// group's `hooks` key; a guard command in ANY group's list counts.
fn group_has_handler(list: Option<&Value>, command: &Path) -> bool {
    list.and_then(Value::as_array)
        .map(|groups| groups.iter().any(|g| has_handler(g.get("hooks"), command)))
        .unwrap_or(false)
}

/// A handler whose absolute script no longer exists (an archived worktree, a
/// renamed adapter); agy runs it on every event and it fails. Install writes
/// the bare path unquoted, so the whole command is checked before its first
/// token: a path with a space is one script, not a script plus arguments.
fn is_dead(handler: &Value) -> bool {
    let Some(command) = handler.get("command").and_then(Value::as_str) else {
        return false;
    };
    let whole = Path::new(command.trim());
    let first = command.split_whitespace().next().map(Path::new);
    whole.is_absolute() && !whole.exists() && !first.is_some_and(Path::exists)
}

/// Another copy of `script`: same file name, different path. A footnote
/// handler left by an older checkout is replaced, never run beside the new
/// one.
fn other_copy_of(handler: &Value, script: &Path) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| {
            let path = Path::new(c.trim());
            path != script && path.file_name().is_some() && path.file_name() == script.file_name()
        })
}

/// Every handler in one event list, flat or grouped under `hooks`.
fn handlers(list: &[Value]) -> Vec<&Value> {
    list.iter()
        .flat_map(|entry| match entry.get("hooks").and_then(Value::as_array) {
            Some(hooks) => hooks.iter().collect(),
            None => vec![entry],
        })
        .collect()
}

/// Dead handlers across the footnote namespace. Only footnote's own
/// namespace is read here; foreign namespaces stay untouched even when dead.
fn count_dead(fn_map: &Map<String, Value>) -> usize {
    fn_map
        .values()
        .filter_map(Value::as_array)
        .map(|list| handlers(list).into_iter().filter(|h| is_dead(h)).count())
        .sum()
}

/// Remove the handlers `gone` selects from every footnote event list. A group
/// this removal empties goes too; a group that was already empty stays.
/// Returns how many handlers went.
fn remove_handlers(fn_map: &mut Map<String, Value>, gone: impl Fn(&Value) -> bool) -> usize {
    let mut removed = 0;
    for list in fn_map.values_mut().filter_map(Value::as_array_mut) {
        list.retain_mut(
            |entry| match entry.get_mut("hooks").and_then(Value::as_array_mut) {
                Some(hooks) => {
                    let before = hooks.len();
                    hooks.retain(|h| !gone(h));
                    removed += before - hooks.len();
                    before == hooks.len() || !hooks.is_empty()
                }
                None => {
                    let drop = gone(entry);
                    removed += usize::from(drop);
                    !drop
                }
            },
        );
    }
    removed
}

/// The footnote namespace as an object, or a word for why not.
fn footnote_namespace(
    root: &Map<String, Value>,
) -> Result<Option<&Map<String, Value>>, &'static str> {
    match root.get("footnote") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err("not_object"),
    }
}

/// One human line for a status read or an install receipt.
impl HookStatus {
    pub fn summary(&self) -> String {
        let enabled = if self.enabled { "enabled" } else { "disabled" };
        let file = match self.file.as_str() {
            "ok" => "ok".to_string(),
            "absent" => "absent".to_string(),
            "not_object" => "not_object".to_string(),
            _ => format!(
                "malformed ({})",
                self.file_error.clone().unwrap_or_default()
            ),
        };
        let dead = if self.dead > 0 {
            format!(" dead={}", self.dead)
        } else {
            String::new()
        };
        format!(
            "file={} footnote={} {} stop={} team={} guard={}{} -> {}",
            file,
            self.footnote,
            enabled,
            self.stop,
            self.team,
            self.guard,
            dead,
            if self.installed {
                "installed"
            } else {
                "not installed"
            },
        )
    }
}

/// Read the hooks file's real state against the adapters this install
/// ships. `adapter`/`team` are the shipped adapter scripts; `None` means
/// this install carries none, which reads `unverifiable`/`not_shipped`
/// rather than a guess.
pub fn status(
    hooks_file: &Path,
    adapter: Option<&Path>,
    team: Option<&Path>,
    guard: Option<&Path>,
) -> HookStatus {
    let mut s = HookStatus {
        file: "ok".to_string(),
        file_error: None,
        file_line: None,
        file_column: None,
        footnote: "absent".to_string(),
        enabled: true,
        stop: "unverifiable".to_string(),
        team: "not_shipped".to_string(),
        guard: "not_shipped".to_string(),
        dead: 0,
        loaded: "unverified",
        runtime: "unverified",
        installed: false,
    };
    let root = match read_root(hooks_file) {
        Root::Ok(map) => map,
        Root::Absent => {
            s.file = "absent".to_string();
            return s;
        }
        Root::NotObject => {
            s.file = "not_object".to_string();
            return s;
        }
        Root::Malformed(e) => {
            s.file = "malformed".to_string();
            s.file_error = Some(e.to_string());
            s.file_line = Some(e.line());
            s.file_column = Some(e.column());
            return s;
        }
    };
    let (footnote_word, fn_map) = match footnote_namespace(&root) {
        Ok(Some(map)) => ("configured", Some(map)),
        Ok(None) => ("absent", None),
        Err(word) => (word, None),
    };
    s.footnote = footnote_word.to_string();
    if let Some(fn_map) = fn_map {
        s.enabled = !matches!(fn_map.get("enabled"), Some(Value::Bool(false)));
        s.dead = count_dead(fn_map);
    }
    s.stop = match (adapter, fn_map) {
        (None, _) => "unverifiable",
        (Some(a), Some(map)) => {
            if has_handler(map.get("Stop"), a) {
                "matches"
            } else {
                "stale"
            }
        }
        (Some(_), None) => "stale",
    }
    .to_string();
    s.team = match (team, fn_map) {
        (None, _) => "not_shipped",
        (Some(c), Some(map)) => {
            if has_handler(map.get("PreInvocation"), c) {
                "matches"
            } else {
                "missing"
            }
        }
        (Some(_), None) => "missing",
    }
    .to_string();
    s.guard = match (guard, fn_map) {
        (None, _) => "not_shipped",
        (Some(g), Some(map)) => {
            if group_has_handler(map.get("PreToolUse"), g) {
                "matches"
            } else {
                "missing"
            }
        }
        (Some(_), None) => "missing",
    }
    .to_string();
    s.installed = s.footnote == "configured"
        && s.stop == "matches"
        && s.team != "missing"
        && s.guard != "missing"
        && s.dead == 0;
    s
}

/// Install footnote's Stop (and team PreInvocation) handlers, preserving
/// every other byte of structure: foreign top-level namespaces, foreign
/// `footnote` keys, and `footnote.enabled` all survive. Refuses - writing
/// nothing - on an unparseable file, a non-object root, or a non-object
/// `footnote` namespace; the remedy is the operator's, not ours.
pub fn install(
    hooks_file: &Path,
    adapter: &Path,
    team: Option<&Path>,
    guard: Option<&Path>,
) -> Result<InstallReceipt, String> {
    let mut root = match read_root(hooks_file) {
        Root::Ok(map) => map,
        Root::Absent => Map::new(),
        Root::NotObject => {
            return Err(format!(
                "{}: not a JSON object; fix the JSON or move the file aside, \
                 then rerun `fno config setup`",
                hooks_file.display()
            ))
        }
        Root::Malformed(e) => {
            // {e} already carries the line/column; prefixing it again only
            // pushes the remedy past the transport's message cap.
            return Err(format!(
                "{}: {e}; fix the JSON or move the file aside, then rerun \
                 `fno config setup`",
                hooks_file.display()
            ));
        }
    };
    let fn_value = root
        .entry("footnote")
        .or_insert_with(|| Value::Object(Map::new()));
    let fn_map = match fn_value {
        Value::Object(map) => map,
        _ => {
            return Err(format!(
                "{}: the footnote namespace is present but not an object; fix \
                 it or move the file aside, then rerun `fno config setup`",
                hooks_file.display()
            ))
        }
    };
    let enabled = !matches!(fn_map.get("enabled"), Some(Value::Bool(false)));
    // The session-state reporter ships beside the stop adapter in the same
    // plugin stage; when the sibling exists on disk, register it under
    // PreInvocation (agy ignores Stop stdout, and the stop adapter owns that
    // event's decision contract). Append-once like the team.
    let report = adapter
        .parent()
        .map(|dir| dir.join("agy-session-report.sh"))
        .filter(|path| path.is_file());
    let mut pruned = remove_handlers(fn_map, is_dead);
    for script in [team, guard, report.as_deref()].into_iter().flatten() {
        pruned += remove_handlers(fn_map, |h| other_copy_of(h, script));
    }
    fn_map.insert(
        "Stop".to_string(),
        json!([{"type": "command", "command": adapter.display().to_string(), "timeout": 60}]),
    );
    if let Some(team) = team {
        match fn_map.get("PreInvocation") {
            Some(Value::Array(_)) => {}
            None => {
                fn_map.insert("PreInvocation".to_string(), Value::Array(Vec::new()));
            }
            Some(_) => {
                return Err(format!(
                    "{}: footnote.PreInvocation is present but not a list; fix \
                     it or move the file aside, then rerun `fno config setup`",
                    hooks_file.display()
                ))
            }
        }
        let needs_append = !has_handler(fn_map.get("PreInvocation"), team);
        if needs_append {
            let pre = fn_map
                .get_mut("PreInvocation")
                .and_then(Value::as_array_mut)
                .expect("array checked above");
            pre.push(json!({
                "type": "command",
                "command": team.display().to_string(),
                "timeout": 30
            }));
        }
    }
    if let Some(guard) = guard {
        match fn_map.get("PreToolUse") {
            Some(Value::Array(_)) => {}
            None => {
                fn_map.insert("PreToolUse".to_string(), Value::Array(Vec::new()));
            }
            Some(_) => {
                return Err(format!(
                    "{}: footnote.PreToolUse is present but not a list; fix \
                     it or move the file aside, then rerun `fno config setup`",
                    hooks_file.display()
                ))
            }
        }
        if !group_has_handler(fn_map.get("PreToolUse"), guard) {
            let pre = fn_map
                .get_mut("PreToolUse")
                .and_then(Value::as_array_mut)
                .expect("array checked above");
            pre.push(json!({
                "matcher": "*",
                "hooks": [{"type": "command", "command": guard.display().to_string(), "timeout": 30}]
            }));
        }
    }
    if let Some(report) = report {
        match fn_map.get("PreInvocation") {
            Some(Value::Array(_)) => {}
            None => {
                fn_map.insert("PreInvocation".to_string(), Value::Array(Vec::new()));
            }
            Some(_) => {
                return Err(format!(
                    "{}: footnote.PreInvocation is present but not a list; fix \
                     it or move the file aside, then rerun `fno config setup`",
                    hooks_file.display()
                ))
            }
        }
        if !has_handler(fn_map.get("PreInvocation"), &report) {
            let pre = fn_map
                .get_mut("PreInvocation")
                .and_then(Value::as_array_mut)
                .expect("array checked above");
            pre.push(json!({
                "type": "command",
                "command": report.display().to_string(),
                "timeout": 10
            }));
        }
    }
    let text = match serde_json::to_string_pretty(&Value::Object(root)) {
        Ok(t) => t,
        Err(e) => {
            return Err(format!(
                "{}: could not serialize: {e}",
                hooks_file.display()
            ))
        }
    };
    // Temp file in the SAME directory, then rename: a crash mid-write cannot
    // leave a truncated hooks.json behind.
    let parent = hooks_file.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("{}: could not create directory: {e}", parent.display()))?;
    let tmp: PathBuf = parent.join(format!(
        ".{}.tmp-{}",
        hooks_file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "hooks.json".to_string()),
        std::process::id()
    ));
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| format!("{}: could not write: {e}", tmp.display()))?;
    std::fs::rename(&tmp, hooks_file)
        .map_err(|e| format!("{}: could not replace: {e}", hooks_file.display()))?;
    let mut note = format!("Stop hook -> {}", hooks_file.display());
    if pruned > 0 {
        note.push_str(&format!(
            "; pruned {pruned} dead or superseded footnote handler(s)"
        ));
    }
    if !enabled {
        note.push_str(
            "; configured but disabled (footnote.enabled = false); agy will \
             not run it until you set it true",
        );
    }
    Ok(InstallReceipt {
        status: "installed",
        note,
        hooks_file: hooks_file.display().to_string(),
        enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp(status: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{status}.hooks.json"));
        (dir, path)
    }

    /// AC1-ERR: a malformed file is refused with the parse position, and the
    /// bytes on disk are untouched afterward.
    #[test]
    fn ac1_err_malformed_refused_and_preserved() {
        let (_dir, path) = tmp("ac1");
        let broken = "{\"other-plugin\":BROKEN USER CONFIG";
        std::fs::write(&path, broken).unwrap();
        let adapter = Path::new("/tmp/fake/adapter.sh");
        let err = install(&path, adapter, None, None).expect_err("malformed must refuse");
        assert!(err.contains("line 1"), "refusal names position: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), broken.as_bytes());
    }

    /// AC2-HP: a foreign namespace and a disabled footnote hook survive an
    /// install; the receipt says configured-but-disabled; the status still
    /// reads installed (enabled is not part of installed()).
    #[test]
    fn ac2_hp_disabled_and_foreign_survive() {
        let (dir, path) = tmp("ac2");
        let adapter = dir.path().join("footnote-agy-target-stop-hook.sh");
        std::fs::write(&adapter, "#!/usr/bin/env bash\n").unwrap();
        let adapter = adapter.as_path();
        std::fs::write(
            &path,
            r#"{
  "some-other-tool": {"Stop": [{"type": "command", "command": "/x/other.sh", "timeout": 5}]},
  "footnote": {
    "enabled": false,
    "Stop": [{"type": "command", "command": "/old/adapter.sh", "timeout": 60}]
  }
}"#,
        )
        .unwrap();
        let receipt = install(&path, adapter, None, None).expect("install succeeds");
        assert!(!receipt.enabled, "receipt carries disabled state");
        let text = std::fs::read_to_string(&path).unwrap();
        let data: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            data["some-other-tool"]["Stop"][0]["command"], "/x/other.sh",
            "foreign namespace unchanged"
        );
        assert_eq!(data["footnote"]["enabled"], false, "enabled untouched");
        assert_eq!(
            data["footnote"]["Stop"][0]["command"],
            adapter.display().to_string(),
            "Stop now names the adapter"
        );
        assert!(receipt.note.contains("configured but disabled"));
        let s = status(&path, Some(adapter), None, None);
        assert!(s.installed, "disabled is still installed");
    }

    /// AC3-ERR: no resolvable adapter reads stop=unverifiable and never
    /// installed.
    #[test]
    fn ac3_err_stop_unverifiable_without_adapter() {
        let (_dir, path) = tmp("ac3");
        std::fs::write(
            &path,
            r#"{"footnote": {"Stop": [{"type": "command", "command": "/any.sh", "timeout": 60}]}}"#,
        )
        .unwrap();
        let s = status(&path, None, None, None);
        assert_eq!(s.stop, "unverifiable");
        assert!(!s.installed);
    }

    /// An absent file is created with its parent directory.
    #[test]
    fn install_creates_absent_file_with_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/nested/hooks.json");
        // A real adapter dir with the session-state reporter as a sibling:
        // install registers the reporter under PreInvocation beside the
        // team, append-once.
        let adapter_dir = dir.path().join("stage").join("hooks");
        std::fs::create_dir_all(&adapter_dir).unwrap();
        let adapter = adapter_dir.join("footnote-agy-target-stop-hook.sh");
        std::fs::write(&adapter, "#!/usr/bin/env bash\n").unwrap();
        let report = adapter_dir.join("agy-session-report.sh");
        std::fs::write(&report, "#!/usr/bin/env bash\n").unwrap();
        install(&path, &adapter, None, None).expect("install into absent path");
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pre = data["footnote"]["PreInvocation"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "the reporter registers once");
        assert_eq!(pre[0]["command"], report.display().to_string());
        assert_eq!(
            data["footnote"]["Stop"][0]["command"],
            adapter.display().to_string()
        );
    }

    /// The team PreInvocation handler is appended when absent and not
    /// duplicated when present; the guard PreToolUse group is added once;
    /// a foreign namespace's PreToolUse groups and other footnote keys
    /// survive (AC13).
    #[test]
    fn team_appended_once_and_other_keys_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let team = Path::new("/plugin/hooks/agy-team-inject.sh");
        let guard = Path::new("/plugin/hooks/agy-lead-guard.sh");
        std::fs::write(
            &path,
            r#"{
  "some-other-tool": {"PreToolUse": [{"matcher": "run_command", "hooks": [{"type": "command", "command": "/x/other.sh"}]}]},
  "footnote": {"Stop": [], "note": "keep me"}
}"#,
        )
        .unwrap();
        let adapter = Path::new("/plugin/hooks/footnote-agy-target-stop-hook.sh");
        install(&path, adapter, Some(team), Some(guard)).expect("install");
        install(&path, adapter, Some(team), Some(guard)).expect("second install");
        let data: Value = serde::de::Deserialize::deserialize(
            &mut serde_json::Deserializer::from_str(&std::fs::read_to_string(&path).unwrap()),
        )
        .unwrap();
        let pre = data["footnote"]["PreInvocation"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "team appended once");
        assert_eq!(pre[0]["command"], team.display().to_string());
        let groups = data["footnote"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "guard group appended once");
        assert_eq!(groups[0]["matcher"], "*");
        assert_eq!(
            groups[0]["hooks"][0]["command"],
            guard.display().to_string()
        );
        assert_eq!(
            data["some-other-tool"]["PreToolUse"][0]["matcher"], "run_command",
            "foreign PreToolUse unchanged"
        );
        assert_eq!(data["footnote"]["note"], "keep me");
    }

    /// A footnote handler whose script is gone (an archived worktree) blocks
    /// `installed`, and the next install drops it and replaces a live copy of
    /// the same script from an older checkout. Live handlers on a path with a
    /// space survive, and so does a dead foreign one.
    #[test]
    fn dead_footnote_handlers_block_installed_and_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let hooks = dir.path().join("Jane Doe/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let adapter = hooks.join("footnote-agy-target-stop-hook.sh");
        let team = hooks.join("agy-team-inject.sh");
        let older = dir.path().join("older-tree/hooks/agy-team-inject.sh");
        std::fs::create_dir_all(older.parent().unwrap()).unwrap();
        for p in [&adapter, &team, &older] {
            std::fs::write(p, "#!/usr/bin/env bash\n").unwrap();
        }
        let gone = dir.path().join("archived-worktree/hooks/agy-old-inject.sh");
        let data = json!({
            "other": {"Stop": [{"type": "command", "command": gone.display().to_string()}]},
            "footnote": {
                "Stop": [{"type": "command", "command": adapter.display().to_string()}],
                "PreInvocation": [
                    {"type": "command", "command": gone.display().to_string()},
                    {"type": "command", "command": older.display().to_string()},
                    {"type": "command", "command": team.display().to_string()}
                ],
                "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": gone.display().to_string()}]}]
            }
        });
        std::fs::write(&path, data.to_string()).unwrap();
        let before = status(&path, Some(&adapter), Some(&team), None);
        assert_eq!(before.dead, 2);
        assert!(!before.installed, "a dead handler is not installed");
        let receipt = install(&path, &adapter, Some(&team), None).expect("install");
        assert!(receipt.note.contains("pruned 3"), "{}", receipt.note);
        let after = status(&path, Some(&adapter), Some(&team), None);
        assert_eq!(after.dead, 0);
        assert!(after.installed);
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pre = data["footnote"]["PreInvocation"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["command"], team.display().to_string());
        assert!(data["footnote"]["PreToolUse"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            data["other"]["Stop"][0]["command"],
            gone.display().to_string(),
            "foreign namespace untouched"
        );
    }

    /// A footnote namespace that is not an object refuses without writing.
    #[test]
    fn non_object_footnote_refused() {
        let (_dir, path) = tmp("ac4");
        std::fs::write(&path, r#"{"footnote": "legacy string"}"#).unwrap();
        let adapter = Path::new("/tmp/fake/adapter.sh");
        let err = install(&path, adapter, None, None).expect_err("non-object footnote refuses");
        assert!(err.contains("not an object"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"footnote": "legacy string"}"#,
        );
    }
}
