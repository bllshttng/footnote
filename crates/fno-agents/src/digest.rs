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
//! The `--since` bound has two forms (see [`Since`]): the CLI `--since <ts>`
//! compares lexicographically (correct for the Z-suffixed RFC3339 in the log,
//! no date math), while `--since-epoch <secs>` — what the attach overlay passes
//! so it can hand the detach time without synthesizing an RFC3339 string —
//! parses each row's ts to epoch seconds for a numeric compare.

use crate::paths::AgentsHome;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
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

/// Read `<field>` from an event value regardless of envelope: the unified
/// shape nests under `/data`, the retired flat shape is top-level. The flat
/// fallback covers the mixed-binary + rotated-history window (x-2901); drop it
/// once the daemon fleet has restarted on the post-cut binary.
fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
}

/// The event's `type` (unified) with a `kind` fallback for retired flat lines
/// during the mixed-binary window (x-2901); drop the fallback post-cut.
fn event_kind(v: &Value) -> Option<&str> {
    v.get("type")
        .and_then(|t| t.as_str())
        .or_else(|| v.get("kind").and_then(|k| k.as_str()))
}

/// The event timestamp; `""` if absent.
fn event_ts(v: &Value) -> &str {
    v.get("ts").and_then(|t| t.as_str()).unwrap_or("")
}

/// The `--since` bound. `Epoch` (what the attach overlay passes: the detach
/// time in epoch seconds) parses each row's RFC3339 ts to epoch and compares
/// numerically, so the digest scopes to the absence window without the caller
/// synthesizing an RFC3339 string. `Str` is the CLI `--since <ts>` form,
/// compared lexicographically (correct for the Z-suffixed RFC3339 in the log).
#[derive(Clone, Copy)]
enum Since<'a> {
    Str(&'a str),
    Epoch(u64),
}

impl Since<'_> {
    /// Is a row with timestamp `ts` inside the window? An undated/unparseable
    /// row is included (never silently dropped by the bound).
    fn includes(&self, ts: &str) -> bool {
        match self {
            Since::Str(s) => ts >= *s,
            Since::Epoch(e) => to_epoch_lenient(ts).is_none_or(|secs| secs >= *e),
        }
    }

    fn label(&self) -> String {
        match self {
            Since::Str(s) => s.to_string(),
            Since::Epoch(e) => format!("epoch:{e}"),
        }
    }
}

/// Parse an RFC3339-ish timestamp to epoch seconds, tolerating the ledger's
/// non-strict forms. `rfc3339_like_to_secs` requires exactly `...SSZ` (20 bytes,
/// trailing Z), but ledger `completed`/`timestamp` are Python
/// `datetime.now().isoformat()` (`2026-07-02T17:11:14.397919`, no Z, fractional
/// seconds). Truncating to the second and appending `Z` reuses the same strict
/// parser, so the `--since-epoch` bound actually filters ledger rows instead of
/// letting every row through as unparseable.
fn to_epoch_lenient(ts: &str) -> Option<u64> {
    crate::state::rfc3339_like_to_secs(ts).or_else(|| {
        let secs = ts.get(..19)?;
        crate::state::rfc3339_like_to_secs(&format!("{secs}Z"))
    })
}

/// Does this event's session id fall in `set`? Branch A carries
/// `data.session_id`, Branch B a flat `session_id`.
fn event_session_in(v: &Value, set: &HashSet<&str>) -> bool {
    field(v, "session_id")
        .and_then(|s| s.as_str())
        .is_some_and(|s| set.contains(s))
}

/// The pure fold. `events_raw` is the newline-joined concatenation of every
/// events.jsonl source; `ledger_raw` is ledger.json content. Both are tolerant:
/// a bad event line bumps `skipped_lines`; an unparseable ledger yields cost 0.
pub fn fold(events_raw: &str, ledger_raw: &str, selector: &str, since: &str) -> DigestSummary {
    fold_since(events_raw, ledger_raw, selector, Since::Str(since))
}

fn fold_since(
    events_raw: &str,
    ledger_raw: &str,
    selector: &str,
    since: Since<'_>,
) -> DigestSummary {
    // Resolve the ledger FIRST: the selector may be a real fno session id, or a
    // node id / title / worktree basename (what the mux client hands us from the
    // focused pane's cwd). The ledger maps any of those to the concrete session
    // id(s), which is how the event fold below finds the loop_check lines.
    let ledger = resolve_from_ledger(ledger_raw, selector, since);
    let mut session_ids: HashSet<&str> = ledger.session_ids.iter().map(String::as_str).collect();
    session_ids.insert(selector);

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
    // The same event can be mirrored into more than one source (project +
    // global events.jsonl), so an exact-duplicate matched line is folded once -
    // else a mirrored block spell would double the episode count and the commit
    // transitions. Only session-matched lines are held, so this stays small.
    let mut seen_lines: HashSet<&str> = HashSet::new();

    for line in events_raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            skipped_lines += 1;
            continue;
        };
        if !event_session_in(&v, &session_ids) {
            continue;
        }
        if !since.includes(event_ts(&v)) {
            continue;
        }
        if !seen_lines.insert(line) {
            continue; // exact duplicate of an already-folded event
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
                    // Count EPISODES (transitions into blocked), not rows: a
                    // stop-hook fires loop_check repeatedly through one block
                    // spell, so only the first block after a non-block decision
                    // starts a new episode. A fresh episode is unresolved until a
                    // following allow (reset so `resolved` reflects the LAST
                    // episode: block -> allow -> block again reads UNRESOLVED).
                    if !last_decision_blocked {
                        blocked_episodes += 1;
                        resolved = false;
                    }
                    last_decision_blocked = true;
                    // Name CI only when it's the actual blocker (a failure or a
                    // pending run); a green/absent CI means the block was review
                    // or promise related, so the intent is the informative reason
                    // (avoids a misleading "blocked on SUCCESS").
                    let ci_blocking = field(&v, "ci")
                        .and_then(|c| c.as_str())
                        .filter(|c| c.starts_with("FAILURE") || *c == "PENDING");
                    last_block_reason = ci_blocking
                        .or_else(|| field(&v, "intent").and_then(|i| i.as_str()))
                        .filter(|r| *r != "none")
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

    let state = if terminated {
        "done"
    } else if last_decision_blocked {
        "blocked"
    } else {
        "working"
    }
    .to_string();

    let mut summary = DigestSummary {
        session: selector.to_string(),
        since: since.label(),
        commits,
        pr_number: ledger.pr_number,
        pr_url: ledger.pr_url,
        pr_state,
        ci,
        reviewed,
        blocked_episodes,
        last_block_reason,
        resolved,
        cost_usd: ledger.cost_usd,
        state,
        skipped_lines,
        lines: Vec::new(),
    };
    summary.lines = render_lines(&summary);
    summary
}

/// What the ledger resolves for a selector.
#[derive(Default)]
struct LedgerFold {
    /// Concrete fno session ids discovered from matched entries (used to find
    /// the matching events).
    session_ids: HashSet<String>,
    pr_number: Option<u64>,
    pr_url: Option<String>,
    cost_usd: f64,
}

/// The basename of a `/`-separated path value (worktree/root_path), lowercased
/// comparison left to the caller.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Does this ledger entry match `selector`? An entry matches by scalar
/// `session_id`, membership in its `sessions[]` array (execution rows carry both
/// a provider UUID and the fno session id there), `graph_node_id`, `title`, or
/// the basename of `worktree` / `root_path` (so a node id like `x-4e2d`, which
/// is also the worktree directory name, resolves).
fn ledger_entry_matches(entry: &Value, selector: &str) -> bool {
    if entry.get("session_id").and_then(|s| s.as_str()) == Some(selector) {
        return true;
    }
    if entry
        .get("sessions")
        .and_then(|s| s.as_array())
        .is_some_and(|arr| arr.iter().any(|s| s.as_str() == Some(selector)))
    {
        return true;
    }
    for key in ["graph_node_id", "title"] {
        if entry.get(key).and_then(|v| v.as_str()) == Some(selector) {
            return true;
        }
    }
    for key in ["worktree", "root_path"] {
        if entry
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|p| basename(p) == selector)
        {
            return true;
        }
    }
    false
}

/// Resolve session ids + sum `cost_usd` + pull the latest PR ref from ledger
/// entries matching `selector`, restricted to `ts >= since`. Tolerant: an
/// unparseable ledger yields an empty fold.
fn resolve_from_ledger(ledger_raw: &str, selector: &str, since: Since<'_>) -> LedgerFold {
    let mut out = LedgerFold::default();
    let Ok(root) = serde_json::from_str::<Value>(ledger_raw) else {
        return out;
    };
    // Tolerate both {"entries":[...]} and a bare [...].
    let Some(entries) = root
        .get("entries")
        .and_then(|e| e.as_array())
        .or_else(|| root.as_array())
    else {
        return out;
    };

    for entry in entries {
        if !ledger_entry_matches(entry, selector) {
            continue;
        }
        // Collect the concrete session id(s) even from a pre-`since` entry, so
        // the event fold can still find in-window events for a node resolved via
        // an older ledger row.
        if let Some(sid) = entry.get("session_id").and_then(|s| s.as_str()) {
            out.session_ids.insert(sid.to_string());
        }
        if let Some(arr) = entry.get("sessions").and_then(|s| s.as_array()) {
            for s in arr.iter().filter_map(|s| s.as_str()) {
                out.session_ids.insert(s.to_string());
            }
        }
        // Timestamp key varies across writers: timestamp | completed | ts.
        let ts = ["timestamp", "completed", "ts"]
            .iter()
            .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("");
        if !since.includes(ts) {
            continue;
        }
        if let Some(c) = entry.get("cost_usd").and_then(|c| c.as_f64()) {
            out.cost_usd += c;
        }
        if let Some(n) = entry.get("pr_number").and_then(|n| n.as_u64()) {
            out.pr_number = Some(n);
        }
        if let Some(u) = entry.get("pr_url").and_then(|u| u.as_str()) {
            out.pr_url = Some(u.to_string());
        }
    }
    out
}

/// True when nothing worth interrupting an attach for happened in the window:
/// no commits, no PR, no blocks, no cost, and not terminated. Such a fold
/// renders zero lines, so the client shows no overlay (a long absence with no
/// activity is silent, not a "no PR yet / 0 commits" nag).
fn is_empty_digest(s: &DigestSummary) -> bool {
    s.commits == 0
        && s.pr_number.is_none()
        && s.blocked_episodes == 0
        && s.cost_usd == 0.0
        && s.state != "done"
        && s.state != "blocked"
}

/// Ranked short lines, attention (blocked) first. An empty digest renders
/// nothing (the caller suppresses the overlay).
fn render_lines(s: &DigestSummary) -> Vec<String> {
    if is_empty_digest(s) {
        return Vec::new();
    }
    let mut lines = Vec::new();

    if s.blocked_episodes > 0 {
        let reason = s.last_block_reason.as_deref().unwrap_or("gate");
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
    /// `--since-epoch <secs>`: the absence-window bound in epoch seconds (what
    /// the attach overlay passes). Wins over `--since` when both are given.
    since_epoch: Option<u64>,
    json: bool,
    events_override: Vec<PathBuf>,
    ledger_override: Option<PathBuf>,
}

fn parse_args(rest: &[String]) -> Result<DigestArgs, String> {
    let mut session: Option<String> = None;
    let mut since = String::new();
    let mut since_epoch: Option<u64> = None;
    let mut json = false;
    let mut events_override: Vec<PathBuf> = Vec::new();
    let mut ledger_override: Option<PathBuf> = None;

    let mut it = expand_eq(rest).into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => session = it.next(),
            "--since" => since = it.next().ok_or("--since needs a value")?,
            "--since-epoch" => {
                since_epoch = Some(
                    it.next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or("--since-epoch needs a non-negative integer")?,
                )
            }
            "--json" | "-J" => json = true,
            // Hidden test hooks: point the fold at fixture files.
            "--events" => {
                events_override.push(PathBuf::from(it.next().ok_or("--events needs a path")?))
            }
            "--ledger" => {
                ledger_override = Some(PathBuf::from(it.next().ok_or("--ledger needs a path")?))
            }
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
        since_epoch,
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

    let since = match args.since_epoch {
        Some(e) => Since::Epoch(e),
        None => Since::Str(&args.since),
    };
    let summary = fold_since(&events_raw, &ledger_raw, &args.session, since);

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
    fn loop_check(
        ts: &str,
        session: &str,
        head: &str,
        pr: &str,
        ci: &str,
        reviewed: bool,
        decision: &str,
    ) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"loop_check","source":"hook","data":{{"session_id":"{session}","fingerprint":"{head}|{pr}|{ci}|none","pr_state":"{pr}","ci":"{ci}","reviewed":{reviewed},"decision":"{decision}","intent":"promise"}}}}"#
        )
    }

    #[test]
    fn happy_names_pr_blocked_and_cost() {
        // Agent committed twice (base -> c1 -> c2 = 2 transitions), opened PR#42,
        // blocked once then resolved.
        let events = [
            loop_check(
                "2026-07-03T01:00:00Z",
                "sess-A",
                "base",
                "none",
                "PENDING",
                false,
                "block",
            ),
            loop_check(
                "2026-07-03T02:00:00Z",
                "sess-A",
                "c1",
                "OPEN",
                "PENDING",
                false,
                "block",
            ),
            loop_check(
                "2026-07-03T03:00:00Z",
                "sess-A",
                "c2",
                "OPEN",
                "SUCCESS",
                true,
                "allow",
            ),
        ]
        .join("\n");
        let ledger = r#"{"entries":[{"session_id":"sess-A","cost_usd":1.5,"pr_number":42,"pr_url":"https://x/pull/42","completed":"2026-07-03T03:00:00Z"}]}"#;

        let d = fold(&events, ledger, "sess-A", "2026-07-03T00:00:00Z");
        assert_eq!(d.pr_number, Some(42), "PR named");
        assert_eq!(d.commits, 2, "base->c1->c2 = 2 commits");
        // Two consecutive block rows are ONE spell (episode), then resolved.
        assert_eq!(d.blocked_episodes, 1);
        assert!(d.resolved, "a later allow resolved the block");
        assert_eq!(d.cost_usd, 1.5);
        assert!(d.reviewed);
        assert_eq!(d.skipped_lines, 0);
        // The rendered lines carry the PR and the block.
        assert!(
            d.lines.iter().any(|l| l.contains("#42")),
            "lines name the PR: {:?}",
            d.lines
        );
        assert!(
            d.lines.iter().any(|l| l.contains("block")),
            "lines name the block"
        );
    }

    #[test]
    fn corrupt_line_is_skipped_and_counted() {
        let events = [
            loop_check(
                "2026-07-03T02:00:00Z",
                "sess-A",
                "c1",
                "OPEN",
                "SUCCESS",
                true,
                "allow",
            ),
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
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "sess-A",
            "c1",
            "OPEN",
            "SUCCESS",
            true,
            "allow",
        );
        let d = fold(&events, "", "sess-A", "2099-01-01T00:00:00Z");
        assert_eq!(d.commits, 0);
        assert_eq!(d.blocked_episodes, 0);
        assert!(d.pr_state.is_none());
        assert_eq!(d.cost_usd, 0.0);
        // Nothing happened in-window -> an empty digest renders no lines, so the
        // client shows no overlay (a long, quiet absence is silent).
        assert!(
            d.lines.is_empty(),
            "empty digest renders nothing: {:?}",
            d.lines
        );
    }

    #[test]
    fn other_session_ignored() {
        let events = [
            loop_check(
                "2026-07-03T02:00:00Z",
                "sess-A",
                "c1",
                "OPEN",
                "SUCCESS",
                true,
                "allow",
            ),
            loop_check(
                "2026-07-03T02:00:00Z",
                "sess-B",
                "z9",
                "MERGED",
                "SUCCESS",
                true,
                "allow",
            ),
        ]
        .join("\n");
        let d = fold(&events, "", "sess-A", "");
        assert_eq!(
            d.pr_state.as_deref(),
            Some("OPEN"),
            "sess-B's MERGED must not leak in"
        );
    }

    #[test]
    fn cost_matches_sessions_array_membership() {
        // Execution rows carry the fno session id in sessions[], not session_id.
        let ledger = r#"[{"sessions":["uuid-1","sess-A"],"cost_usd":23.45,"pr_number":7,"pr_url":"https://x/pull/7","completed":"2026-07-03T03:00:00Z"}]"#;
        let l = resolve_from_ledger(ledger, "sess-A", Since::Str(""));
        assert_eq!(l.pr_number, Some(7));
        assert_eq!(l.pr_url.as_deref(), Some("https://x/pull/7"));
        assert_eq!(l.cost_usd, 23.45);
    }

    #[test]
    fn ledger_cost_respects_since() {
        let ledger = r#"[{"session_id":"sess-A","cost_usd":9.0,"timestamp":"2026-07-01T00:00:00Z"},{"session_id":"sess-A","cost_usd":1.0,"timestamp":"2026-07-03T00:00:00Z"}]"#;
        let l = resolve_from_ledger(ledger, "sess-A", Since::Str("2026-07-02T00:00:00Z"));
        assert_eq!(
            l.cost_usd, 1.0,
            "the pre-since entry is excluded from the delta"
        );
    }

    #[test]
    fn node_id_selector_resolves_via_ledger_to_events() {
        // The mux client hands a node id (worktree basename), not a session id.
        // The ledger row maps the node -> its fno session id, which then finds
        // the loop_check events.
        let ledger = r#"[{"graph_node_id":"x-4e2d","worktree":"/w/footnote/x-4e2d","session_id":"20260703T-abc","cost_usd":2.0,"pr_number":99,"pr_url":"https://x/pull/99","completed":"2026-07-03T03:00:00Z"}]"#;
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "20260703T-abc",
            "c1",
            "OPEN",
            "SUCCESS",
            true,
            "block",
        );
        let d = fold(&events, ledger, "x-4e2d", "");
        assert_eq!(
            d.pr_number,
            Some(99),
            "PR resolved from the ledger by node id"
        );
        assert_eq!(d.cost_usd, 2.0);
        assert_eq!(
            d.pr_state.as_deref(),
            Some("OPEN"),
            "events found via the resolved session id"
        );
        assert_eq!(d.blocked_episodes, 1);
    }

    #[test]
    fn mirrored_events_are_folded_once() {
        // The same block spell mirrored into both event sources (project +
        // global, concatenated) must not double the episode count.
        let one = loop_check(
            "2026-07-03T01:00:00Z",
            "s",
            "a",
            "OPEN",
            "FAILURE:x",
            false,
            "block",
        );
        let events = [one.clone(), one].join("\n");
        let d = fold(&events, "", "s", "");
        assert_eq!(d.blocked_episodes, 1, "the duplicate line is folded once");
    }

    #[test]
    fn repeated_block_rows_are_one_episode() {
        // A stop-hook fires loop_check repeatedly through one block spell.
        let events = [
            loop_check(
                "2026-07-03T01:00:00Z",
                "s",
                "a",
                "OPEN",
                "FAILURE:x",
                false,
                "block",
            ),
            loop_check(
                "2026-07-03T01:05:00Z",
                "s",
                "a",
                "OPEN",
                "FAILURE:x",
                false,
                "block",
            ),
            loop_check(
                "2026-07-03T01:10:00Z",
                "s",
                "a",
                "OPEN",
                "FAILURE:x",
                false,
                "block",
            ),
        ]
        .join("\n");
        let d = fold(&events, "", "s", "");
        assert_eq!(
            d.blocked_episodes, 1,
            "three block rows, one unresolved spell"
        );
    }

    #[test]
    fn ledger_epoch_filter_parses_non_z_microsecond_ts() {
        // Ledger `completed` is Python isoformat (no Z, fractional). The epoch
        // bound must still parse it, or the since-window would leak old rows.
        let ledger =
            r#"[{"session_id":"s","cost_usd":5.0,"completed":"2026-07-01T00:00:00.123456"}]"#;
        let after = crate::state::rfc3339_like_to_secs("2026-07-02T00:00:00Z").unwrap();
        let l = resolve_from_ledger(ledger, "s", Since::Epoch(after));
        assert_eq!(
            l.cost_usd, 0.0,
            "the pre-window microsecond-ts row is excluded"
        );
    }

    #[test]
    fn reblock_after_resolve_reads_unresolved() {
        // block -> allow (resolved) -> block again, still blocked at the end.
        // `resolved` must reflect the LAST episode (unresolved), not the earlier
        // resolution (gemini high-priority finding).
        let events = [
            loop_check(
                "2026-07-03T01:00:00Z",
                "s",
                "a",
                "OPEN",
                "FAILURE:x",
                false,
                "block",
            ),
            loop_check(
                "2026-07-03T02:00:00Z",
                "s",
                "b",
                "OPEN",
                "SUCCESS",
                true,
                "allow",
            ),
            loop_check(
                "2026-07-03T03:00:00Z",
                "s",
                "c",
                "OPEN",
                "FAILURE:y",
                false,
                "block",
            ),
        ]
        .join("\n");
        let d = fold(&events, "", "s", "");
        assert_eq!(d.blocked_episodes, 2);
        assert!(!d.resolved, "the current (last) block is unresolved");
        assert_eq!(d.state, "blocked");
        assert!(d.lines.iter().any(|l| l.contains("UNRESOLVED")));
    }

    #[test]
    fn since_epoch_scopes_to_the_absence_window() {
        // Two loop_checks: one before the "detach", one after. An epoch bound at
        // the detach time must exclude the earlier one.
        let events = [
            loop_check(
                "2026-07-03T01:00:00Z",
                "sess-A",
                "old",
                "OPEN",
                "PENDING",
                false,
                "block",
            ),
            loop_check(
                "2026-07-03T05:00:00Z",
                "sess-A",
                "new",
                "OPEN",
                "SUCCESS",
                true,
                "allow",
            ),
        ]
        .join("\n");
        // 2026-07-03T03:00:00Z == 1751511600 epoch.
        let detach = crate::state::rfc3339_like_to_secs("2026-07-03T03:00:00Z").unwrap();
        let d = fold_since(&events, "", "sess-A", Since::Epoch(detach));
        assert_eq!(
            d.blocked_episodes, 0,
            "the pre-detach block is out of window"
        );
        assert!(d.reviewed, "the in-window allow still counts");
        assert!(d.since.starts_with("epoch:"));
    }

    #[test]
    fn worktree_basename_selector_matches() {
        let entry: Value =
            serde_json::from_str(r#"{"worktree":"/Users/x/conductor/workspaces/footnote/x-4e2d"}"#)
                .unwrap();
        assert!(ledger_entry_matches(&entry, "x-4e2d"));
        assert!(!ledger_entry_matches(&entry, "footnote"));
    }
}
