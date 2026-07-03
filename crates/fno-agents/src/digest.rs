//! `fno-agents digest` — the "while you were gone" fold (x-4e2d).
//!
//! A pure read-time fold: given a session and a `--since` timestamp, summarize
//! what happened by reading events.jsonl (gate/loop signal) and ledger.json
//! (cost + PR). NO new storage — every fact already persists for other reasons.
//!
//! Two event envelopes coexist in the same file and both are read:
//!   - Branch A (Python target/hooks): nested `{"type","source","data":{...}}`.
//!     Carries the interesting signal — `loop_check` (pr_state/ci/reviewed/
//!     fingerprint/decision) and `termination` (reason). Filtered by
//!     `data.session_id`.
//!   - Branch B (Rust daemon): flat `{"ts","kind","source",...}`. Lifecycle
//!     only; matched by a flat `session_id` when present.
//!
//! Commit count is derived from HEAD-sha transitions in the `loop_check`
//! fingerprint (`HEAD|pr_state|ci|review_ts`), since events never carry a
//! commit event directly. PR url/number and cost come from the ledger, not
//! events. Blocked episodes come from `loop_check` `decision:"block"`; "who
//! answered" is not stamped anywhere (see SUMMARY follow-up), so resolution is
//! reported as resolved-or-not (a later non-block decision for the session),
//! not by author.
//!
//! Timestamps are RFC3339 UTC with a `Z` suffix throughout the pipeline, so a
//! lexicographic string compare against `--since` is chronologically correct;
//! no date parsing is needed. `ponytail: string compare over Z-suffixed
//! RFC3339, swap for a parsed compare only if a non-Z offset ever appears.`

use crate::paths::AgentsHome;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The typed fold result. Field order is the JSON key order (serde serializes
/// in declaration order); the client reads `lines` to render the overlay.
#[derive(Debug, Serialize, PartialEq)]
pub struct DigestSummary {
    pub session: String,
    pub since: String,
    pub commits: usize,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub pr_state: Option<String>,
    pub ci: Option<String>,
    pub reviewed: bool,
    pub blocked_episodes: usize,
    pub last_block_reason: Option<String>,
    pub resolved: bool,
    pub cost_usd: f64,
    /// Derived current state: `done` if terminated, else `blocked` if the last
    /// decision blocked and was not followed by an allow, else `working`.
    pub state: String,
    /// Count of events.jsonl lines that failed to parse (AC-error).
    pub skipped_lines: usize,
    /// Pre-rendered ranked human lines (attention first). Both the human text
    /// output and the JSON `lines` field share this, so formatting lives once.
    pub lines: Vec<String>,
}

/// Read `<field>` from a `loop_check`-shaped value regardless of envelope:
/// Branch A nests under `/data`, Branch B is flat.
fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
}

/// The event's `type` (Branch A) or `kind` (Branch B).
fn event_kind(v: &Value) -> Option<&str> {
    v.get("type")
        .and_then(|t| t.as_str())
        .or_else(|| v.get("kind").and_then(|k| k.as_str()))
}

/// The event timestamp; `""` if absent (sorts before any real ts, so an
/// undated event is never excluded by `--since`).
fn event_ts(v: &Value) -> &str {
    v.get("ts").and_then(|t| t.as_str()).unwrap_or("")
}

/// Does this event belong to `session`? Branch A carries `data.session_id`,
/// Branch B a flat `session_id`.
fn event_session_matches(v: &Value, session: &str) -> bool {
    field(v, "session_id").and_then(|s| s.as_str()) == Some(session)
}

/// The pure fold. `events_raw` is the newline-joined concatenation of every
/// events.jsonl source; `ledger_raw` is ledger.json content. Both are tolerant:
/// a bad event line bumps `skipped_lines`; an unparseable ledger yields cost 0.
pub fn fold(events_raw: &str, ledger_raw: &str, session: &str, since: &str) -> DigestSummary {
    let mut skipped_lines = 0usize;
    let mut commit_shas: Vec<String> = Vec::new(); // ordered, for transition count
    let mut pr_state: Option<String> = None;
    let mut ci: Option<String> = None;
    let mut reviewed = false;
    let mut blocked_episodes = 0usize;
    let mut last_block_reason: Option<String> = None;
    let mut resolved = false;
    let mut terminated = false;
    let mut last_decision_blocked = false;

    for line in events_raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            skipped_lines += 1;
            continue;
        };
        if !event_session_matches(&v, session) {
            continue;
        }
        if event_ts(&v) < since {
            continue;
        }
        match event_kind(&v) {
            Some("loop_check") => {
                if let Some(fp) = field(&v, "fingerprint").and_then(|f| f.as_str()) {
                    // fingerprint = HEAD_sha|pr_state|ci|latest_review_ts
                    let head = fp.split('|').next().unwrap_or("");
                    if !head.is_empty() && head != "none" {
                        if commit_shas.last().map(String::as_str) != Some(head) {
                            commit_shas.push(head.to_string());
                        }
                    }
                }
                if let Some(s) = field(&v, "pr_state").and_then(|s| s.as_str()) {
                    if s != "none" {
                        pr_state = Some(s.to_string());
                    }
                }
                if let Some(c) = field(&v, "ci").and_then(|c| c.as_str()) {
                    if c != "none" {
                        ci = Some(c.to_string());
                    }
                }
                if let Some(r) = field(&v, "reviewed").and_then(|r| r.as_bool()) {
                    reviewed = r;
                }
                let decision = field(&v, "decision").and_then(|d| d.as_str());
                if decision == Some("block") {
                    blocked_episodes += 1;
                    last_decision_blocked = true;
                    // Prefer the concrete CI failure; fall back to the intent.
                    last_block_reason = field(&v, "ci")
                        .and_then(|c| c.as_str())
                        .filter(|c| *c != "none")
                        .or_else(|| field(&v, "intent").and_then(|i| i.as_str()))
                        .map(str::to_string);
                } else if decision == Some("allow") {
                    if last_decision_blocked {
                        resolved = true;
                    }
                    last_decision_blocked = false;
                }
            }
            Some("termination") | Some("session_finalized") | Some("node_closed") => {
                terminated = true;
            }
            _ => {}
        }
    }

    // commit count = number of HEAD-sha transitions observed in the window.
    // First observed sha is the baseline (not a new commit), so subtract it.
    let commits = commit_shas.len().saturating_sub(1);

    let (pr_number, pr_url, cost_usd) = fold_ledger(ledger_raw, session, since);

    let state = if terminated {
        "done"
    } else if last_decision_blocked {
        "blocked"
    } else {
        "working"
    }
    .to_string();

    let mut summary = DigestSummary {
        session: session.to_string(),
        since: since.to_string(),
        commits,
        pr_number,
        pr_url,
        pr_state,
        ci,
        reviewed,
        blocked_episodes,
        last_block_reason,
        resolved,
        cost_usd,
        state,
        skipped_lines,
        lines: Vec::new(),
    };
    summary.lines = render_lines(&summary);
    summary
}

/// Sum `cost_usd` and pull the latest PR ref from ledger entries matching the
/// session, restricted to `ts >= since`. An entry matches by scalar
/// `session_id` OR membership in its `sessions[]` array (execution rows carry
/// both a provider UUID and the fno session id there).
fn fold_ledger(ledger_raw: &str, session: &str, since: &str) -> (Option<u64>, Option<String>, f64) {
    let Ok(root) = serde_json::from_str::<Value>(ledger_raw) else {
        return (None, None, 0.0);
    };
    // Tolerate both {"entries":[...]} and a bare [...].
    let entries = root
        .get("entries")
        .and_then(|e| e.as_array())
        .or_else(|| root.as_array());
    let Some(entries) = entries else {
        return (None, None, 0.0);
    };

    let mut cost = 0.0_f64;
    let mut pr_number: Option<u64> = None;
    let mut pr_url: Option<String> = None;

    for entry in entries {
        let matches = entry.get("session_id").and_then(|s| s.as_str()) == Some(session)
            || entry
                .get("sessions")
                .and_then(|s| s.as_array())
                .is_some_and(|arr| arr.iter().any(|s| s.as_str() == Some(session)));
        if !matches {
            continue;
        }
        // Timestamp key varies across writers: timestamp | completed | ts.
        let ts = ["timestamp", "completed", "ts"]
            .iter()
            .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("");
        if ts < since {
            continue;
        }
        if let Some(c) = entry.get("cost_usd").and_then(|c| c.as_f64()) {
            cost += c;
        }
        if let Some(n) = entry.get("pr_number").and_then(|n| n.as_u64()) {
            pr_number = Some(n);
        }
        if let Some(u) = entry.get("pr_url").and_then(|u| u.as_str()) {
            pr_url = Some(u.to_string());
        }
    }
    (pr_number, pr_url, cost)
}

/// Ranked short lines, attention (blocked) first. Empty digest → one calm line.
fn render_lines(s: &DigestSummary) -> Vec<String> {
    let mut lines = Vec::new();

    if s.blocked_episodes > 0 {
        let reason = s.last_block_reason.as_deref().unwrap_or("unknown");
        let resolution = if s.resolved { "resolved" } else { "UNRESOLVED" };
        let plural = if s.blocked_episodes == 1 { "" } else { "s" };
        lines.push(format!(
            "! {} block{plural} (last: {reason}) - {resolution}",
            s.blocked_episodes
        ));
    }

    match (s.pr_number, s.pr_state.as_deref()) {
        (Some(n), state) => {
            let state = state.unwrap_or("open");
            let ci = s.ci.as_deref().unwrap_or("?");
            let review = if s.reviewed { "reviewed" } else { "unreviewed" };
            lines.push(format!("PR #{n} {state} - CI {ci} - {review}"));
        }
        (None, _) => lines.push("no PR yet".to_string()),
    }

    let commit_word = if s.commits == 1 { "commit" } else { "commits" };
    lines.push(format!(
        "{} {commit_word} - ${:.2} - state: {}",
        s.commits, s.cost_usd, s.state
    ));

    lines
}

/// Default event/ledger source paths. The Python target/hook events + ledger
/// live in `~/.fno/` (the PARENT of the agents home `~/.fno/agents`); project
/// events additionally in `<cwd>/.fno/events.jsonl`.
fn default_sources(home: &AgentsHome) -> (Vec<PathBuf>, PathBuf) {
    let fno_dir = home
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let global_events = fno_dir.join("events.jsonl");
    let project_events = PathBuf::from(".fno").join("events.jsonl");
    let ledger = fno_dir.join("ledger.json");
    (vec![project_events, global_events], ledger)
}

struct DigestArgs {
    session: String,
    since: String,
    json: bool,
    events_override: Vec<PathBuf>,
    ledger_override: Option<PathBuf>,
}

fn parse_args(rest: &[String]) -> Result<DigestArgs, String> {
    let mut session: Option<String> = None;
    let mut since = String::new();
    let mut json = false;
    let mut events_override: Vec<PathBuf> = Vec::new();
    let mut ledger_override: Option<PathBuf> = None;

    let mut it = expand_eq(rest).into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => session = it.next(),
            "--since" => since = it.next().ok_or("--since needs a value")?,
            "--json" | "-J" => json = true,
            // Hidden test hooks: point the fold at fixture files.
            "--events" => events_override.push(PathBuf::from(
                it.next().ok_or("--events needs a path")?,
            )),
            "--ledger" => ledger_override = Some(PathBuf::from(
                it.next().ok_or("--ledger needs a path")?,
            )),
            other => return Err(format!("unknown digest flag: {other}")),
        }
    }
    let session = match session {
        Some(s) if !s.is_empty() => s,
        _ => return Err("digest needs --session".into()),
    };
    Ok(DigestArgs {
        session,
        since,
        json,
        events_override,
        ledger_override,
    })
}

/// Split `--key=value` into `["--key","value"]`.
fn expand_eq(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    for a in rest {
        if let Some(eq) = a.find('=') {
            if a.starts_with("--") && eq > 2 {
                out.push(a[..eq].to_string());
                out.push(a[eq + 1..].to_string());
                continue;
            }
        }
        out.push(a.clone());
    }
    out
}

/// The `fno-agents digest` verb. Read-only; exits 0 on empty/corrupt input
/// (only a usage error exits 2), so an attach-time caller never sees a failure.
pub async fn run_digest(rest: &[String], home: &AgentsHome) -> i32 {
    let args = match parse_args(rest) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };

    let (default_events, default_ledger) = default_sources(home);
    let event_paths = if args.events_override.is_empty() {
        default_events
    } else {
        args.events_override
    };
    let ledger_path = args.ledger_override.unwrap_or(default_ledger);

    let mut events_raw = String::new();
    for p in &event_paths {
        if let Ok(content) = std::fs::read_to_string(p) {
            events_raw.push_str(&content);
            if !content.ends_with('\n') {
                events_raw.push('\n');
            }
        }
    }
    let ledger_raw = std::fs::read_to_string(&ledger_path).unwrap_or_default();

    let summary = fold(&events_raw, &ledger_raw, &args.session, &args.since);

    if args.json {
        println!(
            "{}",
            serde_json::to_string(&summary).expect("serializing an owned value never fails")
        );
    } else {
        for line in &summary.lines {
            println!("{line}");
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // A loop_check line in the Python (Branch A) envelope.
    fn loop_check(ts: &str, session: &str, head: &str, pr: &str, ci: &str, reviewed: bool, decision: &str) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"loop_check","source":"hook","data":{{"session_id":"{session}","fingerprint":"{head}|{pr}|{ci}|none","pr_state":"{pr}","ci":"{ci}","reviewed":{reviewed},"decision":"{decision}","intent":"promise"}}}}"#
        )
    }

    #[test]
    fn happy_names_pr_blocked_and_cost() {
        // Agent committed twice (base -> c1 -> c2 = 2 transitions), opened PR#42,
        // blocked once then resolved.
        let events = [
            loop_check("2026-07-03T01:00:00Z", "sess-A", "base", "none", "PENDING", false, "block"),
            loop_check("2026-07-03T02:00:00Z", "sess-A", "c1", "OPEN", "PENDING", false, "block"),
            loop_check("2026-07-03T03:00:00Z", "sess-A", "c2", "OPEN", "SUCCESS", true, "allow"),
        ]
        .join("\n");
        let ledger = r#"{"entries":[{"session_id":"sess-A","cost_usd":1.5,"pr_number":42,"pr_url":"https://x/pull/42","completed":"2026-07-03T03:00:00Z"}]}"#;

        let d = fold(&events, ledger, "sess-A", "2026-07-03T00:00:00Z");
        assert_eq!(d.pr_number, Some(42), "PR named");
        assert_eq!(d.commits, 2, "base->c1->c2 = 2 commits");
        assert_eq!(d.blocked_episodes, 2);
        assert!(d.resolved, "a later allow resolved the block");
        assert_eq!(d.cost_usd, 1.5);
        assert!(d.reviewed);
        assert_eq!(d.skipped_lines, 0);
        // The rendered lines carry the PR and the block.
        assert!(d.lines.iter().any(|l| l.contains("#42")), "lines name the PR: {:?}", d.lines);
        assert!(d.lines.iter().any(|l| l.contains("block")), "lines name the block");
    }

    #[test]
    fn corrupt_line_is_skipped_and_counted() {
        let events = [
            loop_check("2026-07-03T02:00:00Z", "sess-A", "c1", "OPEN", "SUCCESS", true, "allow"),
            "{ this is not valid json".to_string(),
            "".to_string(),
        ]
        .join("\n");
        let d = fold(&events, "", "sess-A", "");
        assert_eq!(d.skipped_lines, 1, "one bad line, blank line not counted");
        assert_eq!(d.pr_state.as_deref(), Some("OPEN"));
    }

    #[test]
    fn since_in_future_is_empty() {
        let events = loop_check("2026-07-03T02:00:00Z", "sess-A", "c1", "OPEN", "SUCCESS", true, "allow");
        let d = fold(&events, "", "sess-A", "2099-01-01T00:00:00Z");
        assert_eq!(d.commits, 0);
        assert_eq!(d.blocked_episodes, 0);
        assert!(d.pr_state.is_none());
        assert_eq!(d.cost_usd, 0.0);
        // Still renders a calm "no PR yet" line, exit path stays 0.
        assert!(d.lines.iter().any(|l| l.contains("no PR yet")));
    }

    #[test]
    fn other_session_ignored() {
        let events = [
            loop_check("2026-07-03T02:00:00Z", "sess-A", "c1", "OPEN", "SUCCESS", true, "allow"),
            loop_check("2026-07-03T02:00:00Z", "sess-B", "z9", "MERGED", "SUCCESS", true, "allow"),
        ]
        .join("\n");
        let d = fold(&events, "", "sess-A", "");
        assert_eq!(d.pr_state.as_deref(), Some("OPEN"), "sess-B's MERGED must not leak in");
    }

    #[test]
    fn cost_matches_sessions_array_membership() {
        // Execution rows carry the fno session id in sessions[], not session_id.
        let ledger = r#"[{"sessions":["uuid-1","sess-A"],"cost_usd":23.45,"pr_number":7,"pr_url":"https://x/pull/7","completed":"2026-07-03T03:00:00Z"}]"#;
        let (n, url, cost) = fold_ledger(ledger, "sess-A", "");
        assert_eq!(n, Some(7));
        assert_eq!(url.as_deref(), Some("https://x/pull/7"));
        assert_eq!(cost, 23.45);
    }

    #[test]
    fn ledger_cost_respects_since() {
        let ledger = r#"[{"session_id":"sess-A","cost_usd":9.0,"timestamp":"2026-07-01T00:00:00Z"},{"session_id":"sess-A","cost_usd":1.0,"timestamp":"2026-07-03T00:00:00Z"}]"#;
        let (_, _, cost) = fold_ledger(ledger, "sess-A", "2026-07-02T00:00:00Z");
        assert_eq!(cost, 1.0, "the pre-since entry is excluded from the delta");
    }
}
