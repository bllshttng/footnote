//! Read-only discovery of every config file opencode reads plugins from,
//! and the consented edit that disables omo and stranger plugin entries.
//!
//! Files scanned, in opencode's own merge order (internal/opencode/docs/
//! config.md, the oh-my-openagent migration guide): the global dir's
//! config.json / opencode.json / opencode.jsonc / tui.json,
//! `$OPENCODE_CONFIG`, `$OPENCODE_CONFIG_DIR`'s opencode.json[c],
//! `~/.opencode/opencode.json[c]`, and every project opencode.json[c] and
//! `.opencode/opencode.json[c]` walking up from cwd to the git root (1.x
//! stops at the git root). A project config is reported, never edited.
//!
//! The scanner skips strings and `//` and `/* */` comments so a commented
//! example never matches and a commented file survives an edit byte-exact
//! outside the removed span.

use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// oh-my-openagent / oh-my-opencode: footnote replaces it; the installer
    /// still asks before flipping the entry.
    Omo,
    /// A bare `fno` or `footnote` spec: an unrelated npm package (fno@0.5.0,
    /// footnote@1.1.0) a stranger owns; footnote loads as a local file, so
    /// the spec imports the wrong code into every session.
    Stranger,
}

impl FindingKind {
    pub fn label(self) -> &'static str {
        match self {
            FindingKind::Omo => "oh-my-openagent",
            FindingKind::Stranger => "stranger spec",
        }
    }

    pub fn why(self) -> &'static str {
        match self {
            FindingKind::Omo => {
                "oh-my-openagent's mirror shadows footnote's own install; its fno: commands load twice and its task tool collides"
            }
            FindingKind::Stranger => {
                "an unrelated npm package, not footnote; footnote loads as a local plugin file"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub file: PathBuf,
    /// Byte span of the element inside the original file (start inclusive,
    /// end exclusive), so an edit removes exactly the entry.
    pub start: usize,
    pub end: usize,
    /// The rendered spec text, as the file carries it.
    pub entry: String,
    pub kind: FindingKind,
    /// A project config: reported, never edited.
    pub project: bool,
}

impl Finding {
    pub fn summary(&self) -> String {
        format!(
            "{}: {} ({})",
            self.file.display(),
            self.entry,
            self.kind.why()
        )
    }
}

/// The global dir's four plugin-bearing files, then the explicit pointers,
/// then the project walk. Deduplicated, order preserved.
pub fn plugin_config_files(cwd: &Path) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    let mut seen: BTreeMap<PathBuf, ()> = BTreeMap::new();
    let mut push = |path: PathBuf, project: bool, out: &mut Vec<(PathBuf, bool)>| {
        if seen.insert(path.clone(), ()).is_none() {
            out.push((path, project));
        }
    };
    let conf = crate::opencode_install::config_dir();
    for name in ["config.json", "opencode.json", "opencode.jsonc", "tui.json"] {
        let path = conf.join(name);
        if path.is_file() {
            push(path, false, &mut out);
        }
    }
    if let Ok(env_file) = std::env::var("OPENCODE_CONFIG") {
        if !env_file.is_empty() {
            let path = PathBuf::from(env_file);
            if path.is_file() {
                push(path, false, &mut out);
            }
        }
    }
    if let Ok(dir_env) = std::env::var("OPENCODE_CONFIG_DIR") {
        if !dir_env.is_empty() {
            let dir = PathBuf::from(dir_env);
            for name in ["opencode.json", "opencode.jsonc"] {
                let path = dir.join(name);
                if path.is_file() {
                    push(path, false, &mut out);
                }
            }
        }
    }
    // ~/.opencode/: opencode reads it after the global config (the omo guide
    // names it in the merge order).
    let home_oc = crate::paths::dirs_home().join(".opencode");
    for name in ["opencode.json", "opencode.jsonc"] {
        let path = home_oc.join(name);
        if path.is_file() {
            push(path, false, &mut out);
        }
    }
    // Project walk: cwd up to the git root (1.x stops there). The root dir
    // itself is included so a config at the git root is covered.
    for dir in walk_up_dirs(cwd) {
        for rel in [
            "opencode.json",
            "opencode.jsonc",
            ".opencode/opencode.json",
            ".opencode/opencode.jsonc",
        ] {
            let path = dir.join(rel);
            if path.is_file() {
                push(path, true, &mut out);
            }
        }
    }
    out
}

fn walk_up_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur = Some(cwd.to_path_buf());
    while let Some(dir) = cur {
        dirs.push(dir.clone());
        if dir.join(".git").exists() {
            break;
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    dirs
}

/// The npm spec's bare package name: no `file:` prefix, no `@<tag>` suffix.
/// A `file:` path is judged by its last path component.
fn spec_bare_name(spec: &str) -> &str {
    let name = spec.strip_prefix("file:").unwrap_or(spec);
    let last = name.rsplit('/').next().unwrap_or(name);
    last.split('@').next().unwrap_or(last)
}

/// Classify one plugin-array entry (the bare package name of a string
/// entry, or a 2.x object's `package` value).
pub fn classify_spec(spec: &str) -> Option<FindingKind> {
    if let Some(path) = spec.strip_prefix("file:") {
        // The name sits mid-path in a real file: entry (...oh-my-openagent/
        // packages/omo-opencode/dist/index.js), so the whole path is judged.
        return if path.contains("oh-my-openagent") || path.contains("oh-my-opencode") {
            Some(FindingKind::Omo)
        } else {
            None
        };
    }
    match spec_bare_name(spec) {
        "oh-my-openagent" | "oh-my-opencode" => Some(FindingKind::Omo),
        "fno" | "footnote" => Some(FindingKind::Stranger),
        _ => None,
    }
}

/// Byte-span of the string token starting at `from` (the opening quote),
/// returning (start, end_exclusive). None when unterminated.
fn string_span_from(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from + 1;
    while i < bytes.len() {
        match bytes[i] {
            0x5c => i += 2,
            0x22 => return Some((from, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// Read a file's top-level `"plugin"`/`"plugins"` array as
/// `(array_start, array_end, element_spans)` in ORIGINAL byte coordinates.
/// Strings and `//`/`/* */` comments are skipped; comments inside the array
/// survive inside the spans. Only a key at the ROOT object's depth matches,
/// so a nested example never wins.
fn scan_plugin_array(text: &str) -> Option<(usize, usize, Vec<(usize, usize)>)> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut elements: Vec<(usize, usize)> = Vec::new();
    let mut elem_start: Option<usize> = None;
    let mut array_open: Option<usize> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if c == b'"' {
            let Some((s, e)) = string_span_from(bytes, i) else {
                return None;
            };
            if array_open.is_some() && depth == 2 && elem_start.is_none() {
                elem_start = Some(i);
            }
            // A root-object key: depth 1, followed by a structural colon.
            if depth == 1 {
                let key = &text[s + 1..e - 1];
                if key == "plugin" || key == "plugins" {
                    let Some(after) = next_structural(text, e) else {
                        return None;
                    };
                    if bytes[after] == b':' {
                        let Some(bracket) = next_structural(text, after + 1) else {
                            return None;
                        };
                        if bytes[bracket] == b'[' {
                            array_open = Some(bracket);
                        }
                    }
                }
            }
            i = e;
            continue;
        }
        match c {
            b'{' | b'[' => {
                if array_open.is_some() && depth == 2 {
                    elem_start = Some(i);
                }
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                }
                i += 1;
            }
            b']' => {
                if depth > 0 {
                    depth -= 1;
                }
                // Closing the plugin array: depth returns to the root
                // object's level (1).
                if depth == 1 && array_open.is_some() {
                    if let Some(es) = elem_start {
                        elements.push((es, i));
                    }
                    return Some((array_open.unwrap(), i + 1, elements));
                }
                i += 1;
            }
            b',' => {
                if array_open.is_some() && depth == 2 && elem_start.is_some() {
                    elements.push((elem_start.unwrap(), i));
                    elem_start = None;
                }
                i += 1;
            }
            _ => {
                if array_open.is_some() && depth == 2 && elem_start.is_none() {
                    elem_start = Some(i);
                }
                i += 1;
            }
        }
        if depth == 0 && array_open.is_some() {
            // The root object closed before the array did: not a real array.
            return None;
        }
    }
    None
}

/// Advance past whitespace and comments after `from`, returning the index of
/// the next structural byte (or None at end of input).
fn next_structural(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        return if i < bytes.len() { Some(i) } else { None };
    }
}

/// Strip `//` and `/* */` comments (preserving newlines and strings) so
/// serde_json can validate an edited document.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c as char);
            match c {
                b'\\' => {
                    if let Some(&n) = bytes.get(i + 1) {
                        out.push(n as char);
                        i += 1;
                    }
                }
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c as char);
            i += 1;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// The parsed spec of one array element: a bare string, or a 2.x object's
/// `package` value. The bool records an object entry (2.x shape).
fn element_spec(raw: &str) -> Option<(String, bool)> {
    let cleaned = strip_comments(raw).trim().to_string();
    if let Ok(s) = serde_json::from_str::<String>(&cleaned) {
        return Some((s, false));
    }
    let v = serde_json::from_str::<serde_json::Value>(&cleaned).ok()?;
    let package = v.get("package")?.as_str()?.to_string();
    Some((package, true))
}

/// Every finding across the files opencode reads plugins from. Read-only.
pub fn audit(cwd: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (file, project) in plugin_config_files(cwd) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some((_as, _ae, elements)) = scan_plugin_array(&text) else {
            continue;
        };
        for (start, end) in elements {
            let raw = &text[start..end];
            let Some((spec, _is_object)) = element_spec(raw) else {
                continue;
            };
            let Some(kind) = classify_spec(&spec) else {
                continue;
            };
            findings.push(Finding {
                file: file.clone(),
                start,
                end,
                entry: spec,
                kind,
                project,
            });
        }
    }
    findings
}

/// oh-my-openagent's own config surfaces, reported and never edited:
/// ~/.omo/omo.jsonc (5.x) and a legacy oh-my-openagent.json[c] /
/// oh-my-opencode.json[c] in the global dir (4.19.x).
pub fn omo_config_paths(conf: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let omo = crate::paths::dirs_home().join(".omo/omo.jsonc");
    if omo.is_file() {
        out.push(omo);
    }
    for name in [
        "oh-my-openagent.json",
        "oh-my-openagent.jsonc",
        "oh-my-opencode.json",
        "oh-my-opencode.jsonc",
    ] {
        let path = conf.join(name);
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

/// Disable every non-project finding: per file, back up, remove each
/// finding's element span plus one adjacent comma, re-parse the
/// comment-stripped document and refuse the write when it no longer
/// parses. Returns (undo lines, refusal lines).
pub fn disable(findings: &[&Finding]) -> (Vec<String>, Vec<String>) {
    let mut by_file: BTreeMap<PathBuf, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        if f.project {
            continue;
        }
        by_file.entry(f.file.clone()).or_default().push(f);
    }
    let mut undo = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for (file, group) in by_file {
        let Ok(original) = std::fs::read_to_string(&file) else {
            refused.push(format!("{}: unreadable", file.display()));
            continue;
        };
        let mut spans: Vec<(usize, usize)> = group.iter().map(|f| (f.start, f.end)).collect();
        spans.sort();
        if spans.iter().any(|(s, e)| original.get(*s..*e).is_none()) {
            refused.push(format!(
                "{}: the file changed since the audit; re-run the install",
                file.display()
            ));
            continue;
        }
        let backup = file.with_file_name(format!(
            "{}.fno-backup-{}",
            file.file_name().unwrap_or_default().to_string_lossy(),
            utc_stamp()
        ));
        undo.push(format!(
            "undo: cp '{}' '{}'",
            backup.display(),
            file.display()
        ));
        cut_and_write(&file, &original, &spans, &backup, &mut refused, &mut undo);
    }
    (undo, refused)
}

/// The per-agent model assignments in ~/.omo/omo.jsonc's `[opencode]`
/// block (5.x config), read-only: footnote never writes under ~/.omo. The
/// read only feeds the suggested agent block the install summary prints.
pub fn omo_agent_models() -> Option<Vec<(String, String)>> {
    let path = crate::paths::dirs_home().join(".omo/omo.jsonc");
    let text = std::fs::read_to_string(path).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(strip_comments(&text).trim()).ok()?;
    let agents = v.get("[opencode]")?.get("agents")?.as_object()?;
    Some(
        agents
            .iter()
            .filter_map(|(name, def)| {
                let model = def.get("model")?.as_str()?.to_string();
                Some((name.clone(), model))
            })
            .collect(),
    )
}

/// omo agent -> the footnote agent that covers its job. Agents absent here
/// (atlas, multimodal-looker, frontend-ui-ux-engineer, document-writer) and
/// every category have no footnote counterpart and are named as not carried.
const OMO_TO_FOOTNOTE: &[(&str, &[&str])] = &[
    ("fno", &["sisyphus"]),
    ("fno:archer", &["hephaestus", "sisyphus-junior"]),
    ("fno:architect", &["oracle", "prometheus", "metis"]),
    ("fno:scout", &["explore", "librarian"]),
    ("fno:verifier", &["momus"]),
];

/// The suggested opencode.json `agent` block mapping omo's model
/// assignments onto footnote's agents, plus the omo names nothing carries.
/// Printed, never written.
pub fn suggested_agent_block(models: &[(String, String)]) -> (String, Vec<String>) {
    let mut block = serde_json::Map::new();
    let mut carried: Vec<&str> = Vec::new();
    for (footnote, omo_names) in OMO_TO_FOOTNOTE {
        // One model per footnote agent: the first omo agent with an
        // assignment wins the slot; its siblings still count as carried -
        // their counterpart exists, the slot just went to another.
        let mut slotted = false;
        for omo in omo_names.iter() {
            if let Some((_, model)) = models.iter().find(|(name, _)| name.as_str() == *omo) {
                if !slotted {
                    block.insert((*footnote).to_string(), json!({ "model": model }));
                    slotted = true;
                }
                carried.push(*omo);
            }
        }
    }
    let mut not_carried: Vec<String> = models
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|n| !carried.contains(n))
        .map(str::to_string)
        .collect();
    not_carried.sort();
    (
        serde_json::to_string_pretty(&serde_json::Value::Object(block)).unwrap_or_default(),
        not_carried,
    )
}

fn utc_stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Cut the spans out of `original` (each plus one adjacent comma), verify
/// the result still parses, then back up and write. Pushes refusal lines
/// for a refused file and drops the undo line only when nothing was cut.
fn cut_and_write(
    file: &Path,
    original: &str,
    spans: &[(usize, usize)],
    backup: &Path,
    refused: &mut Vec<String>,
    undo: &mut Vec<String>,
) {
    let mut edited = original.to_string();
    for (s, e) in spans.iter().rev() {
        let cut = comma_span(&edited, *s, *e);
        edited.replace_range(cut.0..cut.1, "");
    }
    let stripped = strip_comments(&edited);
    if serde_json::from_str::<serde_json::Value>(stripped.trim()).is_err() {
        refused.push(format!(
            "{}: the edit no longer parses; the file is unchanged",
            file.display()
        ));
        undo.pop();
        return;
    }
    if std::fs::copy(file, backup).is_err() {
        refused.push(format!(
            "{}: backup failed; the file is unchanged",
            file.display()
        ));
        undo.pop();
        return;
    }
    if std::fs::write(file, &edited).is_err() {
        refused.push(format!(
            "{}: write failed; the file is unchanged",
            file.display()
        ));
        undo.pop();
        return;
    }
}

/// One element's cut range: the span plus the FOLLOWING comma when present,
/// else the PRECEDING comma.
fn comma_span(text: &str, s: usize, e: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let mut cut_start = s;
    let cut_end = e;
    if let Some(after) = next_structural(text, e) {
        if bytes[after] == b',' {
            return (s, after + 1);
        }
    }
    let mut j = s;
    while j > 0 && bytes[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if j > 0 && bytes[j - 1] == b',' {
        cut_start = j - 1;
    }
    (cut_start, cut_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The opencode-config contract, one declaration: the scan finds the
    /// plugin array through comments and classifies entries (omo, stranger,
    /// file: paths, the 2.x object form); disable cuts the flagged entries,
    /// keeps comments, backs up the original, and leaves bystanders.
    #[test]
    fn scan_classify_disable_one_contract() {
        let text = r#"{
  // opencode config
  "$schema": "https://opencode.ai/config.json",
  "plugin": [
    // a bare fno spec is a stranger's npm package, not footnote
    "oh-my-openagent@latest",
    "opencode-antigravity-auth",
    "fno"
  ],
  "theme": "decoy"
}"#;
        let Some((_s, _e, elements)) = scan_plugin_array(text) else {
            panic!("array not found");
        };
        assert_eq!(elements.len(), 3);
        let first = &text[elements[0].0..elements[0].1];
        assert_eq!(strip_comments(first).trim(), "\"oh-my-openagent@latest\"");
        let third = &text[elements[2].0..elements[2].1];
        assert_eq!(strip_comments(third).trim(), "\"fno\"");
        let after = &text[elements[2].1..];
        assert!(strip_comments(after).trim_start().starts_with("],"));
        // Classification of the scanned entries: omo, stranger, innocent.
        assert_eq!(
            classify_spec("oh-my-openagent@latest"),
            Some(FindingKind::Omo)
        );
        assert_eq!(classify_spec("oh-my-opencode"), Some(FindingKind::Omo));
        assert_eq!(classify_spec("fno"), Some(FindingKind::Stranger));
        assert_eq!(
            classify_spec("footnote@latest"),
            Some(FindingKind::Stranger)
        );
        // The name sits mid-path in a real file: entry.
        assert_eq!(
            classify_spec("file:/x/oh-my-openagent/dist/index.js"),
            Some(FindingKind::Omo)
        );
        assert_eq!(classify_spec("opencode-antigravity-auth"), None);
        // The 2.x object form classifies by its `package` value.
        let v2 = r#"{
  "plugins": [
    { "package": "oh-my-openagent" },
    "opencode-acme-plugin"
  ]
}"#;
        let Some((_s, _e, v2e)) = scan_plugin_array(v2) else {
            panic!("v2 array not found");
        };
        assert_eq!(v2e.len(), 2);
        let (spec, is_object) = element_spec(&v2[v2e[0].0..v2e[0].1]).unwrap();
        assert_eq!(spec, "oh-my-openagent");
        assert!(is_object);
        assert_eq!(classify_spec(&spec), Some(FindingKind::Omo));

        // disable(): cuts the flagged entries from the same fixture shape,
        // keeps comments, backs up the original, leaves bystanders.
        let dir = std::env::temp_dir().join(format!("fno-ocfg-disable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("opencode.jsonc");
        let text = "{\n  // opencode config\n  \"plugin\": [\n    \"oh-my-openagent@latest\",\n    \"opencode-antigravity-auth\",\n    \"fno\"\n  ],\n  \"theme\": \"decoy\"\n}\n";
        std::fs::write(&file, text).unwrap();
        let (_s, _e, elements) = scan_plugin_array(text).unwrap();
        let finding = |i: usize, spec: &str, kind: FindingKind| Finding {
            file: file.clone(),
            start: elements[i].0,
            end: elements[i].1,
            entry: spec.to_string(),
            kind,
            project: false,
        };
        let findings = vec![
            finding(0, "oh-my-openagent@latest", FindingKind::Omo),
            finding(2, "fno", FindingKind::Stranger),
        ];
        let refs: Vec<&Finding> = findings.iter().collect();
        let (undo, refused) = disable(&refs);
        assert!(refused.is_empty(), "refused: {refused:?}");
        assert_eq!(undo.len(), 1);
        let edited = std::fs::read_to_string(&file).unwrap();
        assert!(edited.contains("opencode-antigravity-auth"));
        assert!(!edited.contains("oh-my-openagent"));
        assert!(edited.contains("// opencode config"));
        let v: serde_json::Value = serde_json::from_str(strip_comments(&edited).trim()).unwrap();
        assert_eq!(v["plugin"].as_array().unwrap().len(), 1);
        let backup = undo[0].split('\'').nth(1).unwrap();
        assert_eq!(std::fs::read_to_string(backup).unwrap(), text);
    }
}
