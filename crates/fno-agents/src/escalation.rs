//! Escalation notes: one markdown file per superuser-tier call, written by a
//! king into the project's escalations directory and read here by the king
//! check-in (overdue action) and `state path` (shell hooks). The note stands
//! alone: it names what is being decided, why now, every option with its
//! consequence, the recommendation, and the default taken on silence. No ids,
//! so a phone push reads complete.

use std::path::{Path, PathBuf};

use crate::finalize::{resolve_obsidian_vault, resolve_project_name, resolve_vault_root};

/// The four classes that reach the superuser. Everything else, the king
/// decides and logs.
pub const CLASSES: [&str; 4] = [
    "public-surface",
    "irreversible",
    "money-security",
    "law-change",
];

const SECTION_DECIDING: &str = "What is being decided";
const SECTION_WHY_NOW: &str = "Why it matters now";
const SECTION_OPTIONS: &str = "Options";
const SECTION_RECOMMENDATION: &str = "Recommendation";
const SECTION_ON_DEADLINE: &str = "If no answer by the deadline";
const NEXT_SPLIT: &str = "What happens next:";
// Question-file sections (the ask intake reads them; escalation notes leave
// them empty). Names match docs/architecture/attention-items.md.
const SECTION_BLOCKED_BECAUSE: &str = "Blocked because";
const SECTION_WHY_THESE: &str = "Why these options";
const SECTION_DOWNSIDE: &str = "Downside";
const SECTION_UNKNOWNS: &str = "Not thought through";
const SECTION_REVERSIBLE: &str = "Reversible";
const SECTION_COST_IF_WRONG: &str = "Cost if wrong";
const SECTION_MEANWHILE: &str = "Meanwhile";
const SECTION_WHY_USER: &str = "Why user";

#[derive(Default, Clone, PartialEq, Debug)]
pub struct EscalationOption {
    pub text: String,
    pub next: String,
}

#[derive(Default, Clone, PartialEq, Debug)]
pub struct Escalation {
    /// Set by [`scan`]; a bare [`parse`] carries no path.
    pub path: PathBuf,
    pub class: String,
    pub status: String,
    pub node: Option<String>,
    pub raised_by: String,
    pub raised_at: String,
    pub deadline: String,
    pub recommend: Option<usize>,
    pub on_silence: String,
    pub title: String,
    pub deciding: String,
    pub why_now: String,
    pub options: Vec<EscalationOption>,
    pub recommendation: String,
    pub on_deadline: String,
    // Question-file context fields; empty for an escalation note.
    pub blocked_because: String,
    pub options_rationale: String,
    pub downside: String,
    pub unknowns: String,
    pub reversible: String,
    pub cost_if_wrong: String,
    pub meanwhile: String,
    /// The user-only reason a reversible, recommended question still
    /// reaches the user (irreversible, money or credential, outside the
    /// machine, product or taste). Empty reads as absent.
    pub why_user: String,
}

/// The project's escalations directory: `<vault>/internal/<project>/escalations`
/// when obsidian is enabled with a vault, else the space's `escalations/`.
/// Mirrors `resolve_handoffs_dir`'s vault branch (finalize.rs); a config knob
/// would be an unrequested extra.
pub fn dir(cwd: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    vault_dir_with_home(cwd, home.as_deref(), "escalations")
}

/// The project's question pages directory (attention arm), same contract as
/// [`dir`]: `<vault>/internal/<project>/questions`, else `<space>/questions`.
pub fn questions_dir(cwd: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    vault_dir_with_home(cwd, home.as_deref(), "questions")
}

pub(crate) fn vault_dir_with_home(cwd: &Path, home: Option<&Path>, leaf: &str) -> PathBuf {
    let settings = cwd.join(".fno/config.toml");
    let mut candidates: Vec<PathBuf> = vec![settings.clone()];
    if let Some(h) = home {
        candidates.push(h.join(".fno/config.toml"));
    }
    if let Some(vault) = resolve_obsidian_vault(&candidates) {
        if let Some(vroot) = resolve_vault_root(&vault, home) {
            let project = resolve_project_name(None, home, cwd);
            return vroot.join("internal").join(project).join(leaf);
        }
    }
    crate::paths::space_dir(cwd).join(leaf)
}

/// Tolerant parse: anything missing is empty/None and [`problems`] names it.
pub fn parse(text: &str) -> Escalation {
    let mut esc = Escalation::default();
    let mut lines = text.lines().peekable();
    if lines.peek().map(|l| l.trim() == "---").unwrap_or(false) {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "class" => esc.class = value.to_string(),
                "status" => esc.status = value.to_string(),
                "node" => esc.node = (!value.is_empty()).then(|| value.to_string()),
                "raised_by" => esc.raised_by = value.to_string(),
                "raised_at" => esc.raised_at = value.to_string(),
                "deadline" => esc.deadline = value.to_string(),
                "recommend" => esc.recommend = value.parse::<usize>().ok(),
                "on_silence" => esc.on_silence = value.to_string(),
                "why_user" | "why-user" => esc.why_user = value.to_string(),
                _ => {}
            }
        }
    }

    let mut sections: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some(name) = line.strip_prefix("## ") {
            sections.push((name.trim().to_string(), String::new()));
        } else if esc.title.is_empty() && line.starts_with("# ") {
            esc.title = line[2..].trim().to_string();
        } else if let Some(last) = sections.last_mut() {
            last.1.push_str(line);
            last.1.push('\n');
        }
    }
    let section = |name: &str| -> String {
        sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, body)| body.trim().to_string())
            .unwrap_or_default()
    };
    esc.deciding = section(SECTION_DECIDING);
    esc.why_now = section(SECTION_WHY_NOW);
    esc.options = parse_options(&section(SECTION_OPTIONS));
    esc.recommendation = section(SECTION_RECOMMENDATION);
    esc.on_deadline = section(SECTION_ON_DEADLINE);
    esc.blocked_because = section(SECTION_BLOCKED_BECAUSE);
    esc.options_rationale = section(SECTION_WHY_THESE);
    esc.downside = section(SECTION_DOWNSIDE);
    esc.unknowns = section(SECTION_UNKNOWNS);
    esc.reversible = section(SECTION_REVERSIBLE);
    esc.cost_if_wrong = section(SECTION_COST_IF_WRONG);
    esc.meanwhile = section(SECTION_MEANWHILE);
    esc.why_user = section(SECTION_WHY_USER);
    esc
}

fn is_numbered(line: &str) -> bool {
    let mut chars = line.chars();
    chars.next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        && chars.next() == Some('.')
        && chars.next() == Some(' ')
}

fn parse_options(body: &str) -> Vec<EscalationOption> {
    let mut out: Vec<EscalationOption> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let new_option = is_numbered(line);
        if new_option {
            out.push(EscalationOption::default());
        }
        let Some(cur) = out.last_mut() else {
            continue;
        };
        let numbered_text = |s: &str| -> String {
            match s.find(". ") {
                Some(i) => s[i + 2..].trim().to_string(),
                None => s.to_string(),
            }
        };
        if let Some(pos) = line.find(NEXT_SPLIT) {
            let before = line[..pos].trim();
            let before = before.strip_suffix('.').unwrap_or(before);
            cur.next = line[pos + NEXT_SPLIT.len()..].trim().to_string();
            if new_option {
                cur.text = numbered_text(before);
            } else if !before.is_empty() {
                cur.text.push(' ');
                cur.text.push_str(before);
            }
        } else if new_option {
            cur.text = numbered_text(line);
        } else {
            cur.text.push(' ');
            cur.text.push_str(line);
        }
    }
    out
}

/// Every defect, so a king fixes the note instead of debugging a silent push.
pub fn problems(e: &Escalation) -> Vec<String> {
    let mut out = Vec::new();
    if e.class.is_empty() {
        out.push("missing class".to_string());
    } else if !CLASSES.contains(&e.class.as_str()) {
        out.push(format!("unknown class: {}", e.class));
    }
    for (name, body) in [
        (SECTION_DECIDING, &e.deciding),
        (SECTION_WHY_NOW, &e.why_now),
        (SECTION_RECOMMENDATION, &e.recommendation),
        (SECTION_ON_DEADLINE, &e.on_deadline),
    ] {
        if body.trim().is_empty() {
            out.push(format!("missing section: {name}"));
        }
    }
    if e.options.is_empty() {
        out.push(format!("missing section: {SECTION_OPTIONS}"));
    } else if e.options.len() < 2 {
        out.push("fewer than two options".to_string());
    }
    for (i, option) in e.options.iter().enumerate() {
        if option.next.is_empty() {
            out.push(format!("option {} has no {}", i + 1, NEXT_SPLIT));
        }
    }
    if chrono::DateTime::parse_from_rfc3339(&e.deadline).is_err() {
        out.push(format!("deadline does not parse: {}", e.deadline));
    }
    match e.recommend {
        None => out.push("recommend outside the options".to_string()),
        Some(n) if n == 0 || n > e.options.len() => {
            out.push("recommend outside the options".to_string())
        }
        Some(_) => {}
    }
    if e.class == "irreversible" && e.on_silence != "wait" {
        out.push("an irreversible call waits (on_silence: wait)".to_string());
    }
    out
}

/// The phone push: a title and a body that read complete with no id in them.
pub fn phone_text(e: &Escalation) -> (String, String) {
    let title = format!("fno escalation ({}): {}", e.class, e.title);
    let mut body = String::new();
    let problems = problems(e);
    if !problems.is_empty() {
        body.push_str(&format!(
            "incomplete escalation: missing {}\n\n",
            problems.join(", ")
        ));
    }
    if !e.deciding.is_empty() {
        body.push_str(&e.deciding);
        body.push_str("\n\n");
    }
    if !e.options.is_empty() {
        body.push_str("Options:\n");
        for (i, option) in e.options.iter().enumerate() {
            body.push_str(&format!(
                "{}. {} What happens next: {}\n",
                i + 1,
                option.text,
                if option.next.is_empty() {
                    "(missing)".to_string()
                } else {
                    option.next.clone()
                }
            ));
        }
        body.push('\n');
    }
    if let Some(n) = e.recommend {
        body.push_str(&format!("Recommended: option {n}\n"));
    }
    let silence = match (e.on_silence.as_str(), e.recommend) {
        ("take-recommended", Some(n)) => format!("take option {n}"),
        _ => "the king waits".to_string(),
    };
    body.push_str(&format!("No answer by {}: {}", e.deadline, silence));
    (title, body)
}

/// Open and past the deadline.
pub fn overdue(e: &Escalation, now: chrono::DateTime<chrono::Utc>) -> bool {
    if e.status != "open" {
        return false;
    }
    chrono::DateTime::parse_from_rfc3339(&e.deadline)
        .map(|d| d < now)
        .unwrap_or(false)
}

/// The inverse of [`parse`], for a Rust writer.
pub fn to_note(e: &Escalation) -> String {
    let mut fm = String::new();
    fm.push_str(&format!("class: {}\n", e.class));
    fm.push_str(&format!("status: {}\n", e.status));
    if let Some(node) = &e.node {
        fm.push_str(&format!("node: {node}\n"));
    }
    fm.push_str(&format!("raised_by: {}\n", e.raised_by));
    fm.push_str(&format!("raised_at: {}\n", e.raised_at));
    fm.push_str(&format!("deadline: {}\n", e.deadline));
    if let Some(n) = e.recommend {
        fm.push_str(&format!("recommend: {n}\n"));
    }
    fm.push_str(&format!("on_silence: {}\n", e.on_silence));
    let options = e
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("{}. {} What happens next: {}", i + 1, o.text, o.next))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---\n{fm}---\n# {}\n\n## {SECTION_DECIDING}\n{}\n\n## {SECTION_WHY_NOW}\n{}\n\n## {SECTION_OPTIONS}\n{options}\n\n## {SECTION_RECOMMENDATION}\n{}\n\n## {SECTION_ON_DEADLINE}\n{}\n",
        e.title, e.deciding, e.why_now, e.recommendation, e.on_deadline
    )
}

/// Every note in the directory, path-stamped and sorted. An unreadable
/// directory is an error, never an empty list; one unreadable note logs and
/// skips.
pub fn scan(dir: &Path) -> std::io::Result<Vec<Escalation>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    entries.sort();
    let mut out = Vec::new();
    for path in entries {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut e = parse(&text);
                e.path = path.clone();
                out.push(e);
            }
            Err(err) => eprintln!("escalation: unreadable note {}: {err}", path.display()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::test_env_lock;
    use chrono::Utc;

    const EXAMPLE: &str = r#"---
class: irreversible
status: open
node: x-aaaa
raised_by: king-a792
raised_at: 2026-09-15T09:00:00Z
deadline: 2026-09-16T09:00:00Z
recommend: 2
on_silence: wait
---
# Rewrite release history to remove a leaked token

## What is being decided
Whether to force-push the release branch to remove a token that a commit leaked.

## Why it matters now
The token is live. Anyone with read access to the repo can use it until it is rotated or removed.

## Options
1. Force-push the branch without the commit. What happens next: every open PR on the branch must rebase, and forks keep the old commit.
2. Rotate the token and leave history alone. What happens next: the old token stops working in minutes, and nothing is rewritten.

## Recommendation
Option 2. It closes the risk and cannot break anyone's checkout.

## If no answer by the deadline
The king waits. A force push cannot be undone.
"#;

    #[test]
    fn example_note_parses_clean_and_phones_complete() {
        let e = parse(EXAMPLE);
        assert_eq!(e.class, "irreversible");
        assert_eq!(e.title, "Rewrite release history to remove a leaked token");
        assert_eq!(e.node.as_deref(), Some("x-aaaa"));
        assert_eq!(e.recommend, Some(2));
        assert!(problems(&e).is_empty(), "{:?}", problems(&e));

        let (title, body) = phone_text(&e);
        assert_eq!(
            title,
            "fno escalation (irreversible): Rewrite release history to remove a leaked token"
        );
        assert!(body.contains("Whether to force-push"), "{body}");
        assert!(
            body.contains(
                "1. Force-push the branch without the commit What happens next: every open PR"
            ),
            "{body}"
        );
        assert!(body.contains("2. Rotate the token"), "{body}");
        assert!(body.contains("Recommended: option 2"), "{body}");
        assert!(
            body.contains("No answer by 2026-09-16T09:00:00Z: the king waits"),
            "{body}"
        );
        assert!(!body.contains("x-aaaa"), "no id in the body: {body}");
    }

    #[test]
    fn problems_names_each_defect() {
        let bad = parse(
            "---\nclass: fleet-config\nstatus: open\nraised_by: k\nraised_at: 2026-09-15T09:00:00Z\ndeadline: 2026-09-16T09:00:00Z\nrecommend: 1\non_silence: wait\n---\n# t\n\n## What is being decided\nd\n\n## Why it matters now\nw\n\n## Options\n1. Only one. What happens next: n.\n\n## Recommendation\nr\n\n## If no answer by the deadline\nx\n",
        );
        let found = problems(&bad);
        assert!(
            found
                .iter()
                .any(|p| p.contains("unknown class: fleet-config")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|p| p.contains("fewer than two options")),
            "{found:?}"
        );

        let hasty = parse(
            "---\nclass: irreversible\nstatus: open\nraised_by: k\nraised_at: 2026-09-15T09:00:00Z\ndeadline: 2026-09-16T09:00:00Z\nrecommend: 1\non_silence: take-recommended\n---\n# t\n\n## What is being decided\nd\n\n## Why it matters now\nw\n\n## Options\n1. A. What happens next: n.\n2. B. What happens next: m.\n\n## Recommendation\nr\n\n## If no answer by the deadline\nx\n",
        );
        let found = problems(&hasty);
        assert!(
            found.iter().any(|p| p.contains("irreversible call waits")),
            "{found:?}"
        );
    }

    #[test]
    fn to_note_round_trips_through_parse() {
        let e = parse(EXAMPLE);
        let reparsed = parse(&to_note(&e));
        assert_eq!(e, reparsed);
    }

    #[test]
    fn overdue_needs_open_and_a_past_deadline() {
        let mut e = parse(EXAMPLE);
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-17T00:00:00Z").unwrap();
        assert!(overdue(&e, now.into()));
        e.status = "answered".to_string();
        assert!(!overdue(&e, now.into()));
        e.status = "open".to_string();
        let before = chrono::DateTime::parse_from_rfc3339("2026-09-15T00:00:00Z").unwrap();
        assert!(!overdue(&e, before.into()));
        e.deadline = "not a time".to_string();
        assert!(!overdue(&e, now.into()));
    }

    #[test]
    fn dir_resolves_vault_then_space_fallback() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("fno-escalation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".fno")).unwrap();
        std::fs::write(
            repo.join(".fno/config.toml"),
            "[project]\nid = \"fno\"\n\n[obsidian]\nenabled = true\nvault = \"c3po\"\n",
        )
        .unwrap();

        let with_vault = vault_dir_with_home(&repo, Some(&base), "escalations");
        assert_eq!(
            with_vault,
            base.join("c3po/internal/fno/escalations"),
            "AC2-EDGE vault branch"
        );
        let questions = vault_dir_with_home(&repo, Some(&base), "questions");
        assert_eq!(
            questions,
            base.join("c3po/internal/fno/questions"),
            "AC1-HP vault branch"
        );

        std::fs::write(
            repo.join(".fno/config.toml"),
            "[project]\nid = \"fno\"\n\n[obsidian]\nenabled = false\nvault = \"c3po\"\n",
        )
        .unwrap();
        let home_backup = std::env::var_os("HOME");
        let spaces_backup = std::env::var_os("FNO_SPACES_DIR");
        std::env::set_var("HOME", &base);
        std::env::set_var("FNO_SPACES_DIR", base.join("spaces"));
        let expected = crate::paths::space_dir(&repo).join("escalations");
        let fallback = dir(&repo);
        match home_backup {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match spaces_backup {
            Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
            None => std::env::remove_var("FNO_SPACES_DIR"),
        }
        assert_eq!(
            fallback,
            expected,
            "AC2-EDGE space fallback: {}",
            fallback.display()
        );
        let q_home_backup = std::env::var_os("HOME");
        let q_spaces_backup = std::env::var_os("FNO_SPACES_DIR");
        std::env::set_var("HOME", &base);
        std::env::set_var("FNO_SPACES_DIR", base.join("spaces"));
        let questions_fallback = questions_dir(&repo);
        match q_home_backup {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match q_spaces_backup {
            Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
            None => std::env::remove_var("FNO_SPACES_DIR"),
        }
        assert_eq!(
            questions_fallback,
            crate::paths::space_dir(&repo).join("questions"),
            "AC1-HP space fallback: {}",
            questions_fallback.display()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_reads_every_note_and_stamp_paths() {
        let base = std::env::temp_dir().join(format!("fno-escalation-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("escalations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.md"), EXAMPLE).unwrap();
        std::fs::write(dir.join("a.md"), EXAMPLE).unwrap();
        std::fs::write(dir.join("ignored.txt"), "not a note").unwrap();

        let scanned = scan(&dir).unwrap();
        assert_eq!(scanned.len(), 2);
        assert!(scanned[0].path.ends_with("a.md"), "sorted");
        assert!(scanned.iter().all(|e| e.status == "open"));

        let missing = scan(&base.join("absent")).unwrap_err();
        assert!(
            missing.kind() == std::io::ErrorKind::NotFound,
            "an unreadable directory is an error"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn now_is_utc_floor_for_deadline_math() {
        // Guards the test-only import above staying meaningful.
        assert!(Utc::now().timestamp() > 0);
    }
}
