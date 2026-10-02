//! The king-guard wire layer: payload translation and shared refusal text.
//!
//! [`super::king_guard`] owns the decision core (registry, manifest, roots,
//! mode). This module owns everything that crosses the payload boundary:
//! the file-edit target derivation (claude file_path payloads and codex
//! apply_patch bodies), the codex/agy payload translation, and the
//! two-line refusal text both wires print.

use serde_json::Value;
use std::path::Path;

/// The two-line refusal. The shell twin is a pure exec shim, so this text is
/// the only copy of the rule it enforces.
pub(super) fn deny_text(target: &str, repo_root: &Path) -> String {
    format!(
        "king-delegation-guard: write target '{target}' is inside the repo ({repo}), and a crowned session does not write SOURCE.\n\
         A king operates the machine and does not author it: deploy and repair verbs (fno config plugin install, fno doctor update) run, build output and everything outside the repo allow, repo source does not. Delegate the edit or escalate. An operator can list an in-repo path in config.king.write_roots.\n",
        repo = repo_root.display(),
    )
}

// ── File edit classification ────────────────────────────────────────────────

/// The denied write, if any, for the file-editing tools: a `file_path` (or
/// `notebook_path`) payload judges by its one path; a codex patch payload
/// (body in `tool_input.command`) judges by its header paths. The marker
/// gate keeps an arbitrary command from being read as a patch; the deny
/// names the first denied path.
pub(super) fn read_denied(
    tool: &str,
    ti: &Value,
    targets: &[String],
    allowed: &dyn Fn(&str) -> bool,
) -> Option<String> {
    if tool == "Bash" {
        return targets.iter().find(|t| !allowed(t)).map(|t| t.to_string());
    }
    let file = ti
        .get("file_path")
        .or_else(|| ti.get("notebook_path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !file.is_empty() {
        return (!allowed(file)).then(|| file.to_string());
    }
    let cmd = ti.get("command").and_then(Value::as_str).unwrap_or("");
    let targets = if cmd.contains("*** Begin Patch") {
        patch_targets(cmd)
    } else {
        Vec::new()
    };
    targets.iter().find(|t| !allowed(t)).map(|t| t.to_string())
}

/// Write targets a codex apply_patch body binds: the four header prefixes,
/// the same four `hooks/lib/write-targets.sh` reads. A patch BODY is file
/// content, never re-scanned as shell; only the headers name paths.
pub(super) fn patch_targets(command: &str) -> Vec<String> {
    command
        .lines()
        .filter_map(|line| {
            let path = line
                .strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .or_else(|| line.strip_prefix("*** Move to: "))?;
            let path = path.trim_end_matches('\r');
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

/// Translate an agy PreToolUse payload into the claude shape the guard core
/// reads: toolCall.name -> tool_name (agy tool names mapped: run_command ->
/// Bash, write_blob -> Write, file_change -> Edit, edit_notebook ->
/// NotebookEdit), toolCall.args -> tool_input with a recognized path key
/// promoted to file_path and CommandLine to command, conversationId ->
/// session_id, workspacePaths[0] -> cwd. Measured against agy 1.2.7's
/// embedded hook contract (camelCase protojson, matcher-grouped
/// registration, `{"decision":"deny","reason":...}` veto); agy auth was
/// down machine-wide during the live fire, so the file tools' exact arg
/// keys are unmeasured and the promotion reads every candidate spelling.
pub(super) fn agy_to_claude(v: Value) -> Value {
    let Some(obj) = v.as_object() else {
        return Value::Null;
    };
    let call = obj.get("toolCall").cloned().unwrap_or(Value::Null);
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    let args = call.get("args").cloned().unwrap_or(Value::Null);
    let mut ti = match args {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if !ti.contains_key("file_path") {
        let key = [
            "file_path",
            "FilePath",
            "AbsPath",
            "TargetFile",
            "Path",
            "FileName",
        ]
        .into_iter()
        .find(|k| ti.get(*k).and_then(Value::as_str).is_some());
        if let Some(k) = key {
            let p = ti.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            ti.insert("file_path".to_string(), Value::String(p));
        }
    }
    if !ti.contains_key("command") {
        if let Some(c) = ti.get("CommandLine").and_then(Value::as_str) {
            ti.insert("command".to_string(), Value::String(c.to_string()));
        }
    }
    let tool = match name {
        "run_command" => "Bash",
        "write_blob" => "Write",
        "file_change" => "Edit",
        "edit_notebook" => "NotebookEdit",
        other => other,
    };
    let cwd = obj
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or("");
    serde_json::json!({
        "tool_name": tool,
        "tool_input": Value::Object(ti),
        "session_id": obj.get("conversationId").cloned().unwrap_or(Value::Null),
        "cwd": cwd,
    })
}
