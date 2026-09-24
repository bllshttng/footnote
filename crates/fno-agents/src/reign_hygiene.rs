//! Five reign-hygiene checks judge order in a source-ordered transcript
//! projection. Claude tool uses retain their command or path target; Codex
//! calls share `codex_call_text`. Bash command text stays intact because a
//! source path in Bash can prove a read. User and assistant text remain
//! distinct, and injected skill/system user markers are not operator asks. An
//! unreadable, malformed, or over-budget transcript is unmeasurable, never clean.

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

static PATH_RE: OnceLock<Regex> = OnceLock::new();
static CLAIM_RE: OnceLock<Regex> = OnceLock::new();
static LINE_SUFFIX_RE: OnceLock<Regex> = OnceLock::new();
static ABDICATION_RE: OnceLock<Regex> = OnceLock::new();
static RULING_TEXT_RE: OnceLock<Regex> = OnceLock::new();
static RULING_MAIL_RE: OnceLock<Regex> = OnceLock::new();
static DISPATCH_RE: OnceLock<Regex> = OnceLock::new();
static CORONATION_RE: OnceLock<Regex> = OnceLock::new();
static CROWN_READ_RE: OnceLock<Regex> = OnceLock::new();
static PRWATCH_RE: OnceLock<Regex> = OnceLock::new();
static CONTEXT_PROBE_RE: OnceLock<Regex> = OnceLock::new();
static CONTEXT_ASK_RE: OnceLock<Regex> = OnceLock::new();

// One check-in reads at most the existing four-mebibyte live-transcript window.
pub(crate) const CHECKIN_TRANSCRIPT_BUDGET_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Entry {
    pub index: usize,
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckResult {
    pub check: String,
    pub applicable: bool,
    pub status: String,
    pub detail: String,
    pub index: Option<usize>,
}

impl CheckResult {
    fn new(
        check: &str,
        applicable: bool,
        status: &str,
        detail: impl Into<String>,
        index: Option<usize>,
    ) -> Self {
        Self {
            check: check.to_string(),
            applicable,
            status: status.to_string(),
            detail: detail.into(),
            index,
        }
    }

    #[cfg(test)]
    fn assert_fires(&self, fixture_name: &str) {
        assert!(
            self.applicable,
            "{}: not-applicable on {fixture_name}",
            self.check
        );
        assert_eq!(
            self.status, "violation",
            "{}: expected violation on {fixture_name}, got {} ({})",
            self.check, self.status, self.detail
        );
    }
}

#[cfg(test)]
fn load_entries(path: &Path) -> Result<Vec<Entry>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("{}: unreadable fixture: {e}", path.display()))?;
    load_entries_text(&raw, &path.display().to_string())
}

#[cfg(test)]
fn load_entries_text(raw: &str, source: &str) -> Result<Vec<Entry>, String> {
    #[derive(Deserialize)]
    struct Fixture {
        entries: Vec<Entry>,
    }

    serde_json::from_str::<Fixture>(raw)
        .map(|fixture| fixture.entries)
        .map_err(|e| format!("{source}: malformed fixture: {e}"))
}

fn compiled(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid reign-hygiene regex"))
}

fn tool_uses(entries: &[Entry]) -> impl Iterator<Item = &Entry> {
    entries.iter().filter(|entry| entry.kind == "tool_use")
}

fn normalized_path(path: &str) -> String {
    compiled(&LINE_SUFFIX_RE, r":\d+(-\d+)?$")
        .replace(path, "")
        .trim_start_matches(|ch| ch == '.' || ch == '/')
        .replace("~/", "")
        .replace("<home>/", "")
}

fn path_re() -> &'static Regex {
    compiled(
        &PATH_RE,
        r"[A-Za-z0-9_./-]+\.(?:py|sh|md|rs|toml|ya?ml|json|ts)(?::\d+(?:-\d+)?)?",
    )
}

fn claim_re() -> &'static Regex {
    compiled(
        &CLAIM_RE,
        r"[A-Za-z0-9_./-]+\.(?:py|sh|md|rs|toml|ya?ml|json|ts):\d+",
    )
}

fn reads(entries: &[Entry]) -> HashMap<String, Vec<usize>> {
    let mut reads: HashMap<String, Vec<usize>> = HashMap::new();
    for entry in tool_uses(entries) {
        let candidates: Vec<String> = match entry.tool.as_deref() {
            Some("Read" | "Edit" | "Write" | "Grep" | "Glob") => {
                vec![entry.target.clone()]
            }
            Some("Bash") => path_re()
                .find_iter(&entry.target)
                .map(|capture| capture.as_str().to_string())
                .collect(),
            _ => Vec::new(),
        };
        for candidate in candidates {
            let candidate = candidate.trim();
            if candidate.is_empty()
                || candidate.starts_with("skill:")
                || candidate.starts_with("agent:")
            {
                continue;
            }
            reads
                .entry(normalized_path(candidate))
                .or_default()
                .push(entry.index);
        }
    }
    reads
}

fn covered(claim_path: &str, read_path: &str) -> bool {
    let claim = normalized_path(claim_path);
    let read = normalized_path(read_path);
    claim == read || claim.ends_with(&format!("/{read}")) || read.ends_with(&format!("/{claim}"))
}

fn first(entries: &[Entry], predicate: impl Fn(&Entry) -> bool) -> Option<usize> {
    entries
        .iter()
        .find(|entry| predicate(entry))
        .map(|entry| entry.index)
}

fn is_spawn(entry: &Entry) -> bool {
    entry.kind == "tool_use"
        && (entry.tool.as_deref() == Some("Agent")
            || entry.target.contains("agents spawn")
            || entry.target.contains("backlog advance"))
}

fn flush_shell_word(args: &mut Vec<String>, word: &mut String, started: &mut bool) {
    if *started {
        args.push(std::mem::take(word));
        *started = false;
    }
}

fn shell_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut args = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut escaped = false;
    let mut in_comment = false;
    for character in command.chars() {
        if in_comment {
            if character == '\n' {
                flush_shell_word(&mut args, &mut word, &mut started);
                if !args.is_empty() {
                    segments.push(std::mem::take(&mut args));
                }
                in_comment = false;
            }
            continue;
        }
        if escaped {
            word.push(character);
            started = true;
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else if character == '\\' && active_quote == '"' {
                escaped = true;
            } else {
                word.push(character);
            }
            started = true;
            continue;
        }
        match character {
            '\\' => {
                escaped = true;
                started = true;
            }
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            '#' if !started => in_comment = true,
            ';' | '|' | '&' | '\n' => {
                flush_shell_word(&mut args, &mut word, &mut started);
                if !args.is_empty() {
                    segments.push(std::mem::take(&mut args));
                }
            }
            character if character.is_whitespace() => {
                flush_shell_word(&mut args, &mut word, &mut started);
            }
            _ => {
                word.push(character);
                started = true;
            }
        }
    }
    if escaped {
        word.push('\\');
    }
    flush_shell_word(&mut args, &mut word, &mut started);
    if !args.is_empty() {
        segments.push(args);
    }
    segments
}

fn crown_spawn_command(target: &str) -> bool {
    shell_command_segments(target).iter().any(|args| {
        args.len() >= 3
            && args[0] == "fno"
            && args[1] == "agents"
            && args[2] == "spawn"
            && args
                .iter()
                .skip(3)
                .any(|arg| arg == "--crown" || arg.starts_with("--crown="))
    })
}

fn ruling_command(target: &str) -> bool {
    if target.contains("--help") || target.contains("--to-self") {
        return false;
    }
    let mail_send =
        compiled(&RULING_MAIL_RE, r"(^|[;|&\n])\s*fno agents mail send\b").is_match(target);
    if mail_send {
        return true;
    }
    compiled(
        &DISPATCH_RE,
        r"(^|[;|&\n])\s*fno backlog update\b.*--dispatch-(?:verb|brief)",
    )
    .is_match(target)
}

fn is_ruling(entry: &Entry) -> bool {
    (entry.kind == "tool_use" && ruling_command(&entry.target))
        || (entry.kind == "assistant_text"
            && compiled(
                &RULING_TEXT_RE,
                r"(?i)\b(Ruling:|I (?:approve|revise|rule|ruled)|DOCTRINE|AMENDMENT|SCOPE CHANGE)\b",
            )
            .is_match(&entry.text))
}

fn check1_claim_before_read(entries: &[Entry]) -> CheckResult {
    let reads = reads(entries);
    let mut offenders: Vec<(usize, String)> = Vec::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == "assistant_text")
    {
        for claim in claim_re().find_iter(&entry.text) {
            let claim = claim.as_str();
            let path = compiled(&LINE_SUFFIX_RE, r":\d+(-\d+)?$").replace(claim, "");
            let prior = reads.iter().any(|(read_path, indices)| {
                covered(&path, read_path)
                    && indices.iter().any(|read_index| *read_index < entry.index)
            });
            if !prior {
                offenders.push((entry.index, claim.to_string()));
            }
        }
    }
    if let Some((index, first_claim)) = offenders.first() {
        return CheckResult::new(
            "check1_claim_before_read",
            true,
            "violation",
            format!(
                "{} claim(s) before any read; first: '{first_claim}' at index {index}; e.g. PR-mergeable/authorship claims are the same root (assert from proxy, not the authoritative source) - see generalization note in the bank task",
                offenders.len()
            ),
            Some(*index),
        );
    }
    let claim_count = entries
        .iter()
        .filter(|entry| entry.kind == "assistant_text" && claim_re().is_match(&entry.text))
        .count();
    CheckResult::new(
        "check1_claim_before_read",
        claim_count > 0,
        if claim_count > 0 {
            "clean"
        } else {
            "not-applicable"
        },
        format!("{claim_count} claim(s), all preceded by a read"),
        None,
    )
}

fn check2_spawn_abdicate_rule(entries: &[Entry], shape: Option<&str>) -> CheckResult {
    if shape == Some("court") {
        return CheckResult::new(
            "check2_spawn_abdicate_rule",
            false,
            "not-applicable",
            "court reign: the pass exit rule does not apply",
            None,
        );
    }
    let first_spawn = first(entries, is_spawn);
    let Some(first_spawn) = first_spawn else {
        return CheckResult::new(
            "check2_spawn_abdicate_rule",
            false,
            "not-applicable",
            "no spawn in reign; pass-shape abdicate rule does not apply",
            None,
        );
    };
    let first_abdication = first(entries, |entry| {
        entry.kind == "assistant_text"
            && compiled(&ABDICATION_RE, r"^\s*\**\s*Abdicat(?:ing|ed)\b").is_match(&entry.text)
    });
    let Some(first_abdication) = first_abdication else {
        return CheckResult::new(
            "check2_spawn_abdicate_rule",
            true,
            "violation",
            "spawned but never declared abdication",
            Some(first_spawn),
        );
    };
    for entry in entries {
        if entry.index <= first_abdication || entry.kind != "tool_use" {
            continue;
        }
        if is_spawn(entry) || ruling_command(&entry.target) {
            return CheckResult::new(
                "check2_spawn_abdicate_rule",
                true,
                "violation",
                format!(
                    "abdicated at index {first_abdication} then took orchestration action at index {} ('{}') - did not exit",
                    entry.index,
                    entry.target.chars().take(60).collect::<String>()
                ),
                Some(entry.index),
            );
        }
    }
    CheckResult::new(
        "check2_spawn_abdicate_rule",
        true,
        "clean",
        format!("spawned at {first_spawn}, abdicated at {first_abdication}, no later action"),
        None,
    )
}

fn check3_crown_before_ruling(entries: &[Entry]) -> CheckResult {
    let first_ruling = first(entries, is_ruling);
    let Some(first_ruling) = first_ruling else {
        return CheckResult::new(
            "check3_crown_before_ruling",
            false,
            "not-applicable",
            "no ruling in reign; crown duty does not apply",
            None,
        );
    };
    let coronation = compiled(&CORONATION_RE, r"(^|[;|&\n])\s*fno agents crown\s");
    let crown_read = compiled(
        &CROWN_READ_RE,
        r"(^|[;|&\n])\s*fno (?:agents court\b|whoami(?:\s|$))",
    );
    for entry in entries {
        if entry.index >= first_ruling {
            break;
        }
        if entry.kind != "tool_use" || entry.tool.as_deref() != Some("Bash") {
            continue;
        }
        if coronation.is_match(&entry.target)
            || crown_spawn_command(&entry.target)
            || crown_read.is_match(&entry.target)
        {
            return CheckResult::new(
                "check3_crown_before_ruling",
                true,
                "clean",
                format!(
                    "crown read or coronation at {} precedes first ruling at {first_ruling}",
                    entry.index
                ),
                None,
            );
        }
    }
    CheckResult::new(
        "check3_crown_before_ruling",
        true,
        "violation",
        format!(
            "ruled at index {first_ruling} with no crown read (fno agents court) or coronation before it"
        ),
        Some(first_ruling),
    )
}

fn check4_prwatch_before_dispatch(entries: &[Entry]) -> CheckResult {
    let first_dispatch = first(entries, is_spawn);
    let Some(first_dispatch) = first_dispatch else {
        return CheckResult::new(
            "check4_prwatch_before_dispatch",
            false,
            "not-applicable",
            "no dispatch in reign; pr-watch duty does not apply",
            None,
        );
    };
    let probe_re = compiled(
        &PRWATCH_RE,
        r"(^|[;|&\n])\s*fno (?:do pr watch status|pr-watch status|doctor(?:\s|$))",
    );
    let probe = first(entries, |entry| {
        entry.kind == "tool_use" && probe_re.is_match(&entry.target)
    });
    if let Some(probe) = probe.filter(|probe| *probe < first_dispatch) {
        return CheckResult::new(
            "check4_prwatch_before_dispatch",
            true,
            "clean",
            format!("pr-watch probe at {probe} precedes first dispatch at {first_dispatch}"),
            None,
        );
    }
    let where_probe = probe.map_or_else(
        || "no probe at all".to_string(),
        |probe| format!("first probe at {probe} (after dispatch)"),
    );
    CheckResult::new(
        "check4_prwatch_before_dispatch",
        true,
        "violation",
        format!(
            "dispatched at index {first_dispatch} without a pr-watch liveness probe before it ({where_probe})"
        ),
        Some(first_dispatch),
    )
}

fn check5_context_timing_heuristic(entries: &[Entry]) -> CheckResult {
    let probe_re = compiled(
        &CONTEXT_PROBE_RE,
        r"fno context\b|fno whoami context\b|context-probe|context_probe|context_window|token_count|token_usage",
    );
    let probe = first(entries, |entry| {
        entry.kind == "tool_use" && probe_re.is_match(&entry.target)
    });
    let Some(probe) = probe else {
        return CheckResult::new(
            "check5_context_timing_heuristic",
            false,
            "not-applicable",
            "no context probe in reign",
            None,
        );
    };
    let ask_re = compiled(
        &CONTEXT_ASK_RE,
        r"(?i)\b(context|how much (?:context|room)|window|token budget)\b",
    );
    let ask = first(entries, |entry| {
        entry.kind == "user_text" && ask_re.is_match(&entry.text)
    });
    if ask.is_some_and(|ask| ask < probe) {
        return CheckResult::new(
            "check5_context_timing_heuristic",
            true,
            "violation",
            format!(
                "HEURISTIC: context probe at {probe} only after operator asked at {}",
                ask.unwrap()
            ),
            Some(probe),
        );
    }
    CheckResult::new(
        "check5_context_timing_heuristic",
        true,
        "clean",
        format!("probe at {probe} not preceded by an operator ask"),
        None,
    )
}

pub(crate) fn run_checks(entries: &[Entry], shape: Option<&str>) -> [CheckResult; 5] {
    [
        check1_claim_before_read(entries),
        check2_spawn_abdicate_rule(entries, shape),
        check3_crown_before_ruling(entries),
        check4_prwatch_before_dispatch(entries),
        check5_context_timing_heuristic(entries),
    ]
}

fn append_entry(
    entries: &mut Vec<Entry>,
    kind: &str,
    tool: Option<String>,
    target: String,
    text: String,
) {
    entries.push(Entry {
        index: entries.len(),
        kind: kind.to_string(),
        tool,
        target,
        text,
    });
}

fn user_text_is_injected(text: &str) -> bool {
    [
        "Base directory for this skill",
        "<command-message>",
        "<command-name>",
        "<system-reminder>",
        "<task-notification>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn claude_tool_target(tool: &str, input: &Value) -> String {
    match tool {
        "Bash" => input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "Read" | "Edit" | "Write" => input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "Grep" | "Glob" => input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "Skill" => format!(
            "skill:{}",
            input.get("skill").and_then(Value::as_str).unwrap_or("")
        ),
        "Agent" => format!(
            "agent:{}",
            input
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
        _ => String::new(),
    }
}

fn claude_entries(raw: &str, path: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for (line_index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "{}:{}: malformed transcript JSON: {e}",
                path.display(),
                line_index + 1
            )
        })?;
        let message = row.get("message").unwrap_or(&row);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| row.get("type").and_then(Value::as_str))
            .unwrap_or("");
        let Some(content) = message.get("content") else {
            continue;
        };
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                match block.get("type").and_then(Value::as_str).unwrap_or("") {
                    "tool_use" => {
                        let tool = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let input = block.get("input").unwrap_or(&Value::Null);
                        append_entry(
                            &mut entries,
                            "tool_use",
                            Some(tool.to_string()),
                            claude_tool_target(tool, input),
                            String::new(),
                        );
                    }
                    "text" => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if role == "assistant" {
                            append_entry(
                                &mut entries,
                                "assistant_text",
                                None,
                                String::new(),
                                text.to_string(),
                            );
                        } else if role == "user" && !user_text_is_injected(text) {
                            append_entry(
                                &mut entries,
                                "user_text",
                                None,
                                String::new(),
                                text.to_string(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        } else if let Some(text) = content.as_str() {
            if role == "assistant" {
                append_entry(
                    &mut entries,
                    "assistant_text",
                    None,
                    String::new(),
                    text.to_string(),
                );
            } else if role == "user" && !user_text_is_injected(text) {
                append_entry(
                    &mut entries,
                    "user_text",
                    None,
                    String::new(),
                    text.to_string(),
                );
            }
        }
    }
    Ok(entries)
}

fn message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn codex_entries(raw: &str, path: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for (line_index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "{}:{}: malformed transcript JSON: {e}",
                path.display(),
                line_index + 1
            )
        })?;
        let Some(payload) = row.get("payload") else {
            continue;
        };
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                let target =
                    crate::transcript_activity::codex_call_text(line).ok_or_else(|| {
                        format!(
                            "{}:{}: transcript call has no command text",
                            path.display(),
                            line_index + 1
                        )
                    })?;
                append_entry(
                    &mut entries,
                    "tool_use",
                    Some("Bash".to_string()),
                    target,
                    String::new(),
                );
            }
            "message" => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                let text = message_text(payload.get("content").unwrap_or(&Value::Null));
                if role == "assistant" {
                    append_entry(&mut entries, "assistant_text", None, String::new(), text);
                } else if role == "user" && !user_text_is_injected(&text) {
                    append_entry(&mut entries, "user_text", None, String::new(), text);
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

pub(crate) fn entries_from_transcript(harness: &str, path: &Path) -> Result<Vec<Entry>, String> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|e| format!("{}: unreadable transcript: {e}", path.display()))?;
    let size = file
        .metadata()
        .map_err(|e| format!("{}: unreadable transcript: {e}", path.display()))?
        .len();
    if size > CHECKIN_TRANSCRIPT_BUDGET_BYTES {
        return Err(format!(
            "{}: transcript over cap ({size} bytes exceeds {}-byte check-in budget)",
            path.display(),
            CHECKIN_TRANSCRIPT_BUDGET_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(size.min(CHECKIN_TRANSCRIPT_BUDGET_BYTES) as usize);
    file.take(CHECKIN_TRANSCRIPT_BUDGET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: unreadable transcript: {e}", path.display()))?;
    if bytes.len() as u64 > CHECKIN_TRANSCRIPT_BUDGET_BYTES {
        return Err(format!(
            "{}: transcript over cap (more than {} bytes)",
            path.display(),
            CHECKIN_TRANSCRIPT_BUDGET_BYTES
        ));
    }
    let raw = String::from_utf8(bytes)
        .map_err(|e| format!("{}: transcript is not UTF-8: {e}", path.display()))?;
    match harness {
        "claude" => claude_entries(&raw, path),
        "codex" => codex_entries(&raw, path),
        other => Err(format!(
            "{}: unsupported transcript harness {other}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const REIGN: &str = include_str!("../tests/fixtures/king_reign/reign-2026-08-05.json");
    const LATE_CROWN: &str = include_str!("../tests/fixtures/king_reign/synthetic-late-crown.json");
    const NO_SPAWN: &str = include_str!("../tests/fixtures/king_reign/synthetic-no-spawn.json");

    fn entry(index: usize, kind: &str, tool: Option<&str>, target: &str, text: &str) -> Entry {
        Entry {
            index,
            kind: kind.to_string(),
            tool: tool.map(str::to_string),
            target: target.to_string(),
            text: text.to_string(),
        }
    }

    fn fixture(raw: &str) -> Vec<Entry> {
        load_entries_text(raw, "fixture.json").expect("fixture parses")
    }

    fn temp_file(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fno-reign-hygiene-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn ac6_fixture_has_no_session_identity() {
        let identity = regex::Regex::new(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|/Users/|c55d89d3",
        )
        .unwrap();
        for raw in [REIGN, LATE_CROWN, NO_SPAWN] {
            assert!(!identity.is_match(raw), "fixture leaks session identity");
        }
    }

    #[test]
    fn ac7_fixture_covers_every_consumed_kind() {
        let path = temp_file("fixture.json", REIGN);
        let entries = load_entries(&path).expect("fixture file parses");
        for needed in ["tool_use", "assistant_text", "user_text"] {
            assert!(
                entries.iter().any(|entry| entry.kind == needed),
                "fixture missing {needed}"
            );
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn ac5_malformed_fixture_fails_loudly() {
        let err = load_entries_text("{ not valid json ,, ", "malformed.json")
            .expect_err("malformed fixture must refuse");
        assert!(err.contains("malformed.json"), "{err}");
    }

    #[test]
    fn check1_fires_on_reign() {
        let result = check1_claim_before_read(&fixture(REIGN));
        result.assert_fires("reign-2026-08-05");
        assert_eq!(result.index, Some(84));
    }

    #[test]
    fn check2_fires_on_reign() {
        let result = check2_spawn_abdicate_rule(&fixture(REIGN), None);
        result.assert_fires("reign-2026-08-05");
        assert_eq!(result.index, Some(65));
    }

    #[test]
    fn check3_fires_on_reign() {
        let result = check3_crown_before_ruling(&fixture(REIGN));
        result.assert_fires("reign-2026-08-05");
        assert_eq!(result.index, Some(42));
    }

    #[test]
    fn check4_fires_on_reign() {
        let result = check4_prwatch_before_dispatch(&fixture(REIGN));
        result.assert_fires("reign-2026-08-05");
        assert_eq!(result.index, Some(28));
    }

    #[test]
    fn ac3_check2_not_applicable_when_no_spawn() {
        let result = check2_spawn_abdicate_rule(&fixture(NO_SPAWN), None);
        assert!(!result.applicable);
        assert_eq!(result.status, "not-applicable");
    }

    #[test]
    fn ac4_check3_fails_on_late_crown() {
        check3_crown_before_ruling(&fixture(LATE_CROWN)).assert_fires("synthetic-late-crown");
    }

    #[test]
    fn check1_same_basename_does_not_suppress_violation() {
        let entries = vec![
            entry(0, "tool_use", Some("Read"), "skills/execute/SKILL.md", ""),
            entry(
                1,
                "assistant_text",
                None,
                "",
                "skills/target/SKILL.md:40 pins the executor.",
            ),
        ];
        check1_claim_before_read(&entries).assert_fires("synthetic-basename-collision");
    }

    #[test]
    fn check2_agent_spawn_after_abdication_fires() {
        let entries = vec![
            entry(
                0,
                "tool_use",
                Some("Bash"),
                "fno agents spawn w1 --crown level=0,scope=fno",
                "",
            ),
            entry(1, "assistant_text", None, "", "Abdicating. Crown expired."),
            entry(2, "tool_use", Some("Agent"), "agent:archer", ""),
        ];
        check2_spawn_abdicate_rule(&entries, None)
            .assert_fires("synthetic-agent-spawn-after-abdication");
    }

    #[test]
    fn heuristic_check5_runs_on_reign() {
        let result = check5_context_timing_heuristic(&fixture(REIGN));
        assert_eq!(result.check, "check5_context_timing_heuristic");
        assert!(
            result.status == "violation"
                || result.status == "clean"
                || result.status == "not-applicable"
        );
    }

    #[test]
    fn inherited_crown_court_read_before_ruling_is_clean() {
        let entries = vec![
            entry(0, "tool_use", Some("Bash"), "fno agents court --json", ""),
            entry(
                1,
                "tool_use",
                Some("Bash"),
                "fno agents mail send decision",
                "",
            ),
        ];
        let result = check3_crown_before_ruling(&entries);
        assert_eq!(result.status, "clean", "{}", result.detail);
    }

    #[test]
    fn pr_watch_inside_quoted_details_is_not_a_probe() {
        let entries = vec![
            entry(0, "tool_use", Some("Bash"), "fno agents spawn worker", ""),
            entry(
                1,
                "tool_use",
                Some("Bash"),
                "fno backlog idea \"pr-watch status reading wedged\"",
                "",
            ),
        ];
        let result = check4_prwatch_before_dispatch(&entries);
        assert_eq!(result.status, "violation", "{}", result.detail);
    }

    #[test]
    fn mail_help_and_to_self_are_not_rulings() {
        for target in [
            "fno agents mail send --help",
            "fno agents mail send --to-self report",
        ] {
            assert!(
                !is_ruling(&entry(0, "tool_use", Some("Bash"), target, "")),
                "{target}"
            );
        }
    }

    #[test]
    fn crown_mentioned_in_spawn_prompt_does_not_prove_crowning() {
        let entries = vec![
            entry(
                0,
                "tool_use",
                Some("Bash"),
                "fno agents spawn worker --prompt 'mention --crown in your answer'",
                "",
            ),
            entry(
                1,
                "tool_use",
                Some("Bash"),
                "fno agents mail send ruling",
                "",
            ),
        ];
        assert_eq!(
            check3_crown_before_ruling(&entries).status,
            "violation",
            "a flag mentioned inside quoted prompt text is not a crown"
        );
        let commented = vec![
            entry(
                0,
                "tool_use",
                Some("Bash"),
                "fno agents spawn worker # --crown\n",
                "",
            ),
            entry(
                1,
                "tool_use",
                Some("Bash"),
                "fno agents mail send ruling",
                "",
            ),
        ];
        assert_eq!(
            check3_crown_before_ruling(&commented).status,
            "violation",
            "a flag in a shell comment is not a crown"
        );
        let crowned = vec![
            entry(
                0,
                "tool_use",
                Some("Bash"),
                "fno agents spawn worker --crown x-root",
                "",
            ),
            entry(
                1,
                "tool_use",
                Some("Bash"),
                "fno agents mail send ruling",
                "",
            ),
        ];
        assert_eq!(check3_crown_before_ruling(&crowned).status, "clean");
    }

    #[test]
    fn bare_sed_path_is_a_read() {
        let entries = vec![
            entry(
                0,
                "tool_use",
                Some("Bash"),
                "sed -n 1,80p crates/fno-agents/src/crown_settle.rs",
                "",
            ),
            entry(
                1,
                "assistant_text",
                None,
                "",
                "crates/fno-agents/src/crown_settle.rs:8 settles the crown.",
            ),
        ];
        let result = check1_claim_before_read(&entries);
        assert_eq!(result.status, "clean", "{}", result.detail);
    }

    #[test]
    fn court_shape_makes_check2_not_applicable() {
        let entries = vec![entry(
            0,
            "tool_use",
            Some("Bash"),
            "fno agents spawn worker",
            "",
        )];
        let result = check2_spawn_abdicate_rule(&entries, Some("court"));
        assert!(!result.applicable);
        assert_eq!(
            result.detail,
            "court reign: the pass exit rule does not apply"
        );
    }

    #[test]
    fn skill_body_user_text_is_not_an_operator_ask() {
        let body = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"<command-message>check context\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"fno whoami context\"}}]}}\n"
        );
        let path = temp_file("claude.jsonl", body);
        let entries = entries_from_transcript("claude", &path).expect("transcript parsed");
        let result = check5_context_timing_heuristic(&entries);
        assert_eq!(result.status, "clean", "{}", result.detail);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn codex_function_call_line_becomes_a_bash_entry() {
        let path = temp_file(
            "codex.jsonl",
            "{\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"fno whoami context\"}}\n",
        );
        let entries = entries_from_transcript("codex", &path).expect("rollout parsed");
        assert!(entries.iter().any(|entry| {
            entry.kind == "tool_use"
                && entry.tool.as_deref() == Some("Bash")
                && entry.target == "fno whoami context"
        }));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unreadable_transcript_is_an_error_naming_path() {
        let path = std::env::temp_dir().join(format!("missing-reign-{}.jsonl", std::process::id()));
        let err =
            entries_from_transcript("claude", &path).expect_err("missing transcript is not empty");
        assert!(err.contains(&path.display().to_string()), "{err}");
    }
}
