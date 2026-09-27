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
    pub crown: String,
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
        format!(
            "file={} footnote={} {} stop={} crown={} -> {}",
            file,
            self.footnote,
            enabled,
            self.stop,
            self.crown,
            if self.installed {
                "installed"
            } else {
                "not installed"
            },
        )
    }
}

/// Read the hooks file's real state against the adapters this install
/// ships. `adapter`/`crown` are the shipped adapter scripts; `None` means
/// this install carries none, which reads `unverifiable`/`not_shipped`
/// rather than a guess.
pub fn status(hooks_file: &Path, adapter: Option<&Path>, crown: Option<&Path>) -> HookStatus {
    let mut s = HookStatus {
        file: "ok".to_string(),
        file_error: None,
        file_line: None,
        file_column: None,
        footnote: "absent".to_string(),
        enabled: true,
        stop: "unverifiable".to_string(),
        crown: "not_shipped".to_string(),
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
    s.crown = match (crown, fn_map) {
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
    s.installed = s.footnote == "configured" && s.stop == "matches" && s.crown != "missing";
    s
}

/// Install footnote's Stop (and crown PreInvocation) handlers, preserving
/// every other byte of structure: foreign top-level namespaces, foreign
/// `footnote` keys, and `footnote.enabled` all survive. Refuses - writing
/// nothing - on an unparseable file, a non-object root, or a non-object
/// `footnote` namespace; the remedy is the operator's, not ours.
pub fn install(
    hooks_file: &Path,
    adapter: &Path,
    crown: Option<&Path>,
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
    fn_map.insert(
        "Stop".to_string(),
        json!([{"type": "command", "command": adapter.display().to_string(), "timeout": 60}]),
    );
    if let Some(crown) = crown {
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
        let needs_append = !has_handler(fn_map.get("PreInvocation"), crown);
        if needs_append {
            let pre = fn_map
                .get_mut("PreInvocation")
                .and_then(Value::as_array_mut)
                .expect("array checked above");
            pre.push(json!({
                "type": "command",
                "command": crown.display().to_string(),
                "timeout": 30
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
        let err = install(&path, adapter, None).expect_err("malformed must refuse");
        assert!(err.contains("line 1"), "refusal names position: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), broken.as_bytes());
    }

    /// AC2-HP: a foreign namespace and a disabled footnote hook survive an
    /// install; the receipt says configured-but-disabled; the status still
    /// reads installed (enabled is not part of installed()).
    #[test]
    fn ac2_hp_disabled_and_foreign_survive() {
        let (_dir, path) = tmp("ac2");
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
        let adapter = Path::new("/plugin/hooks/agy-target-stop-hook.sh");
        let receipt = install(&path, adapter, None).expect("install succeeds");
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
        let s = status(&path, Some(adapter), None);
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
        let s = status(&path, None, None);
        assert_eq!(s.stop, "unverifiable");
        assert!(!s.installed);
    }

    /// An absent file is created with its parent directory.
    #[test]
    fn install_creates_absent_file_with_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/nested/hooks.json");
        let adapter = Path::new("/plugin/hooks/agy-target-stop-hook.sh");
        install(&path, adapter, None).expect("install into absent path");
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            data["footnote"]["Stop"][0]["command"],
            "/plugin/hooks/agy-target-stop-hook.sh"
        );
    }

    /// The crown PreInvocation handler is appended when absent and not
    /// duplicated when present; other footnote keys survive.
    #[test]
    fn crown_appended_once_and_other_keys_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let crown = Path::new("/plugin/hooks/agy-crown-inject.sh");
        std::fs::write(&path, r#"{"footnote": {"Stop": [], "note": "keep me"}}"#).unwrap();
        let adapter = Path::new("/plugin/hooks/agy-target-stop-hook.sh");
        install(&path, adapter, Some(crown)).expect("install");
        install(&path, adapter, Some(crown)).expect("second install");
        let data: Value = serde::de::Deserialize::deserialize(
            &mut serde_json::Deserializer::from_str(&std::fs::read_to_string(&path).unwrap()),
        )
        .unwrap();
        let pre = data["footnote"]["PreInvocation"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "crown appended once");
        assert_eq!(pre[0]["command"], crown.display().to_string());
        assert_eq!(data["footnote"]["note"], "keep me");
    }

    /// A footnote namespace that is not an object refuses without writing.
    #[test]
    fn non_object_footnote_refused() {
        let (_dir, path) = tmp("ac4");
        std::fs::write(&path, r#"{"footnote": "legacy string"}"#).unwrap();
        let adapter = Path::new("/tmp/fake/adapter.sh");
        let err = install(&path, adapter, None).expect_err("non-object footnote refuses");
        assert!(err.contains("not an object"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"footnote": "legacy string"}"#,
        );
    }
}
