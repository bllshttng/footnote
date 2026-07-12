//! `fno-agents needs` - the needs-me-queue events-fold leg (x-feec).
//!
//! A pure read-time fold over events.jsonl producing the two event-derived
//! attention reasons the mux client cannot see from live badges alone:
//!   - `review_wedged`: a green OPEN PR whose loop keeps blocking on review that
//!     will not self-heal (the codex usage-limit lesson - surface it EARLY).
//!   - `budget_stop`: a loop that terminated on `Budget` / `NoProgress` and
//!     needs a human to re-arm.
//!
//! Unlike [`crate::digest`] (which folds ONE session's activity), this folds
//! ALL sessions and emits at most one [`NeedItem`] per session - the worst
//! reason its latest events imply. Each item is resolved to a node/name/title
//! via the ledger bridge so the client can join it to a sideline row.
//!
//! The two event envelopes (Python nested `{"type","data":{...}}` and the
//! retired Rust flat `{"kind",...}`) are both read, same as digest. Read-only:
//! the verb writes nothing and is rerunnable at will.

use crate::paths::AgentsHome;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default fold window when `--since-epoch` is absent: the last 24h.
const DEFAULT_WINDOW_SECS: u64 = 24 * 60 * 60;

/// `fires` floor for `review_wedged`: the loop must have re-checked at least
/// this many times before a green-PR block counts as wedged (a fresh block
/// during a normal review wait is not yet a wedge). Hardcoded heuristic, not a
/// config knob - tune the const if it misfires (ponytail: no config for a value
/// that never changes); a hidden `--fires-floor` overrides it for tests.
const DEFAULT_FIRES_FLOOR: u64 = 2;

/// One reason a session needs a human, resolved and ready to render. `kind` is
/// a stable string (`review_wedged` | `budget_stop`) the client maps to its own
/// severity enum; the fold does not rank (the client owns the full 6-kind
/// order, of which this leg populates two).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NeedItem {
    pub kind: String,
    pub session_id: String,
    /// The graph node id, when the ledger resolves one.
    pub node: Option<String>,
    /// A display name to join a sideline row on (worktree basename or node id).
    pub name: Option<String>,
    pub title: Option<String>,
    /// The deciding event's timestamp (the wedge's loop_check, or the stop).
    pub ts: String,
    /// A one-line human reason.
    pub evidence: String,
    /// Does this item's node hold a live (or suspect) claim? Stamped by the IO
    /// layer, not the pure fold. The client renders an item that joins no roster
    /// row only when it is `live`, so a dead session's stale stop never nags.
    pub live: bool,
}

/// Read `<field>` regardless of envelope: nested under `/data` (unified) or
/// top-level (retired flat). Mirrors [`crate::digest`] - kept local so this
/// module stays a self-contained leaf (x-7fdd: no function-local cross-imports).
fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
}

fn event_kind(v: &Value) -> Option<&str> {
    v.get("type")
        .and_then(|t| t.as_str())
        .or_else(|| v.get("kind").and_then(|k| k.as_str()))
}

fn event_ts(v: &Value) -> &str {
    v.get("ts").and_then(|t| t.as_str()).unwrap_or("")
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    field(v, key).and_then(|f| f.as_str())
}

/// The basename of a `/`-separated path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Parse an RFC3339-ish ts to epoch seconds, tolerating the ledger's non-strict
/// (`...isoformat()`, no Z, fractional) forms. Same lenient parse as digest.
fn to_epoch_lenient(ts: &str) -> Option<u64> {
    crate::state::rfc3339_like_to_secs(ts).or_else(|| {
        let secs = ts.get(..19)?;
        crate::state::rfc3339_like_to_secs(&format!("{secs}Z"))
    })
}

/// A row's ts is in-window when it parses to `>= since` (an unparseable ts is
/// included - never silently dropped by the bound).
fn in_window(ts: &str, since: u64) -> bool {
    to_epoch_lenient(ts).is_none_or(|secs| secs >= since)
}

/// The latest loop_check state observed for a session (later event wins).
#[derive(Default, Clone)]
struct LoopState {
    decision: String,
    ci: String,
    pr_state: String,
    reviewed: bool,
    fires: u64,
    ts: String,
}

/// The latest termination observed for a session.
#[derive(Default, Clone)]
struct TermState {
    reason: String,
    ts: String,
}

#[derive(Default)]
struct SessionAcc {
    latest_loop: Option<LoopState>,
    latest_term: Option<TermState>,
}

/// The pure fold. `events_raw` is the newline-joined concatenation of every
/// events.jsonl source; `ledger_raw` is ledger.json. Emits one [`NeedItem`] per
/// qualifying session, sorted `(ts, session_id)` for deterministic output.
pub fn fold(events_raw: &str, ledger_raw: &str, since: u64, fires_floor: u64) -> Vec<NeedItem> {
    let mut sessions: HashMap<String, SessionAcc> = HashMap::new();

    for line in events_raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // torn/malformed tail line: skip, never abort (digest precedent)
        };
        let ts = event_ts(&v);
        if !in_window(ts, since) {
            continue;
        }
        let Some(sid) = str_field(&v, "session_id") else {
            continue; // an event with no session can't be joined to a row
        };
        match event_kind(&v) {
            Some("loop_check") => {
                let acc = sessions.entry(sid.to_string()).or_default();
                acc.latest_loop = Some(LoopState {
                    decision: str_field(&v, "decision").unwrap_or("").to_string(),
                    ci: str_field(&v, "ci").unwrap_or("").to_string(),
                    pr_state: str_field(&v, "pr_state").unwrap_or("").to_string(),
                    reviewed: field(&v, "reviewed")
                        .and_then(|r| r.as_bool())
                        .unwrap_or(false),
                    fires: field(&v, "fires").and_then(|f| f.as_u64()).unwrap_or(0),
                    ts: ts.to_string(),
                });
            }
            Some("termination") | Some("loop_terminated") => {
                let acc = sessions.entry(sid.to_string()).or_default();
                acc.latest_term = Some(TermState {
                    reason: str_field(&v, "reason").unwrap_or("").to_string(),
                    ts: ts.to_string(),
                });
            }
            _ => {}
        }
    }

    let ledger = LedgerIndex::parse(ledger_raw);
    let mut items: Vec<NeedItem> = Vec::new();
    for (sid, acc) in &sessions {
        if let Some((kind, ts, evidence)) = classify(acc, fires_floor) {
            let (node, name, title) = ledger.resolve(sid);
            items.push(NeedItem {
                kind: kind.to_string(),
                session_id: sid.clone(),
                node,
                name,
                title,
                ts,
                evidence,
                live: false, // stamped by the IO layer; the fold stays pure
            });
        }
    }
    items.sort_by(|a, b| {
        a.ts.cmp(&b.ts)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    items
}

/// The reason a session's latest events imply, or `None` when nothing needs a
/// human. Termination is terminal, so a session that ended on `Budget` /
/// `NoProgress` is a `budget_stop`; any other termination (DonePRGreen, NoWork,
/// Interrupted, ...) means nothing needs me. A still-live loop whose latest
/// check is a green OPEN unreviewed block past the fires floor is `review_wedged`
/// - a later `allow` or a termination clears it (the latest event wins).
fn classify(acc: &SessionAcc, fires_floor: u64) -> Option<(&'static str, String, String)> {
    // Compare by epoch, not lexically: a Python-isoformat termination ts
    // (`...00.5`, no Z) would sort BEFORE a same-second Z-suffixed loop_check
    // (`.` 46 < `Z` 90) and misclassify a real stop as still-looping. Fall back
    // to a lexical compare only when a ts is unparseable.
    let terminated = match (&acc.latest_term, &acc.latest_loop) {
        (Some(t), Some(l)) => match (to_epoch_lenient(&t.ts), to_epoch_lenient(&l.ts)) {
            (Some(ts), Some(ls)) => ts >= ls,
            _ => t.ts >= l.ts,
        },
        (Some(_), None) => true,
        (None, _) => false,
    };
    if terminated {
        let t = acc.latest_term.as_ref()?;
        return match t.reason.as_str() {
            "Budget" | "NoProgress" => Some((
                "budget_stop",
                t.ts.clone(),
                format!("loop stopped: {}", t.reason),
            )),
            _ => None,
        };
    }
    let l = acc.latest_loop.as_ref()?;
    let wedged = l.decision == "block"
        && l.ci == "SUCCESS"
        && l.pr_state == "OPEN"
        && !l.reviewed
        && l.fires >= fires_floor;
    if wedged {
        return Some((
            "review_wedged",
            l.ts.clone(),
            format!("green PR wedged on review ({} checks)", l.fires),
        ));
    }
    None
}

/// A minimal ledger index: maps a session id to its node/name/title. Reuses the
/// digest bridge's match keys (scalar `session_id`, `sessions[]` membership,
/// `graph_node_id`, `worktree`/`root_path` basename) but inverted - given a
/// session, return its display identity.
struct LedgerIndex {
    entries: Vec<Value>,
}

impl LedgerIndex {
    fn parse(ledger_raw: &str) -> Self {
        let entries = serde_json::from_str::<Value>(ledger_raw)
            .ok()
            .and_then(|root| {
                root.get("entries")
                    .and_then(|e| e.as_array())
                    .or_else(|| root.as_array())
                    .cloned()
            })
            .unwrap_or_default();
        LedgerIndex { entries }
    }

    fn entry_has_session(entry: &Value, sid: &str) -> bool {
        if entry.get("session_id").and_then(|s| s.as_str()) == Some(sid) {
            return true;
        }
        entry
            .get("sessions")
            .and_then(|s| s.as_array())
            .is_some_and(|arr| arr.iter().any(|s| s.as_str() == Some(sid)))
    }

    /// `(node, name, title)` for a session. `name` prefers the worktree basename
    /// (what a sideline orphan row carries), else the node id. All `None` when
    /// unresolved - the client renders a session-id-only squadless row then.
    fn resolve(&self, sid: &str) -> (Option<String>, Option<String>, Option<String>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|e| Self::entry_has_session(e, sid))
        else {
            return (None, None, None);
        };
        let node = entry
            .get("graph_node_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let name = ["worktree", "root_path"]
            .iter()
            .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
            .map(|p| basename(p).to_string())
            .or_else(|| node.clone());
        (node, name, title)
    }
}

struct NeedsArgs {
    since_epoch: Option<u64>,
    fires_floor: u64,
    json: bool,
    events_override: Vec<PathBuf>,
    ledger_override: Option<PathBuf>,
}

fn parse_args(rest: &[String]) -> Result<NeedsArgs, String> {
    let mut since_epoch: Option<u64> = None;
    let mut fires_floor = DEFAULT_FIRES_FLOOR;
    let mut json = false;
    let mut events_override: Vec<PathBuf> = Vec::new();
    let mut ledger_override: Option<PathBuf> = None;

    let mut it = expand_eq(rest).into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since-epoch" => {
                since_epoch = Some(
                    it.next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or("--since-epoch needs a non-negative integer")?,
                )
            }
            "--fires-floor" => {
                fires_floor = it
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or("--fires-floor needs a non-negative integer")?
            }
            "--json" | "-J" => json = true,
            "--events" => {
                events_override.push(PathBuf::from(it.next().ok_or("--events needs a path")?))
            }
            "--ledger" => {
                ledger_override = Some(PathBuf::from(it.next().ok_or("--ledger needs a path")?))
            }
            other => return Err(format!("unknown needs flag: {other}")),
        }
    }
    Ok(NeedsArgs {
        since_epoch,
        fires_floor,
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

/// Default event/ledger sources: project `.fno/events.jsonl` + global
/// `~/.fno/events.jsonl` + `~/.fno/ledger.json` (the digest layout).
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

/// Stamp each item's `live` bit from its node claim (x-feec 1.4): an item whose
/// node holds a Live or Suspect claim (a suspect TTL-unexpired claim still
/// protects the slot) renders even without a roster row; an unclaimed or
/// node-less one stays `live=false` and the client drops it when unjoined. This
/// is the IO half of the fold, kept out of the pure [`fold`] so it stays testable.
fn stamp_liveness(mut items: Vec<NeedItem>) -> Vec<NeedItem> {
    for item in &mut items {
        item.live = item.node.as_deref().is_some_and(|n| {
            let (state, _) = crate::claims::status(&format!("node:{n}"), None);
            matches!(
                state,
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
            )
        });
    }
    items
}

/// Current epoch seconds; `0` if the clock is somehow before the epoch.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `fno-agents needs` verb. Read-only; exits 0 on empty/corrupt input (only
/// a usage error exits 2), so the overlay caller never sees a failure it must
/// handle beyond a nonzero exit.
pub async fn run_needs(rest: &[String], home: &AgentsHome) -> i32 {
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

    let since = args
        .since_epoch
        .unwrap_or_else(|| now_secs().saturating_sub(DEFAULT_WINDOW_SECS));
    let items = stamp_liveness(fold(&events_raw, &ledger_raw, since, args.fires_floor));

    if args.json {
        println!(
            "{}",
            serde_json::to_string(&items).expect("serializing an owned value never fails")
        );
    } else {
        for item in &items {
            let name = item.name.as_deref().unwrap_or(&item.session_id);
            println!("{} {} - {}", item.kind, name, item.evidence);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loop_check(
        ts: &str,
        session: &str,
        decision: &str,
        ci: &str,
        pr_state: &str,
        reviewed: bool,
        fires: u64,
    ) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"loop_check","source":"hook","data":{{"session_id":"{session}","decision":"{decision}","ci":"{ci}","pr_state":"{pr_state}","reviewed":{reviewed},"fires":{fires}}}}}"#
        )
    }

    fn termination(ts: &str, session: &str, reason: &str) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"termination","source":"hook","data":{{"session_id":"{session}","reason":"{reason}"}}}}"#
        )
    }

    // The whole default window: since=0 lets every fixture ts through.
    const ALL: u64 = 0;

    #[test]
    fn green_open_unreviewed_block_is_review_wedged() {
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "s",
            "block",
            "SUCCESS",
            "OPEN",
            false,
            5,
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "review_wedged");
        assert_eq!(items[0].session_id, "s");
        assert!(items[0].evidence.contains("5 checks"));
    }

    #[test]
    fn budget_termination_is_budget_stop() {
        let events = termination("2026-07-03T02:00:00Z", "s", "Budget");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "budget_stop");
        assert!(items[0].evidence.contains("Budget"));
    }

    #[test]
    fn noprogress_termination_is_budget_stop() {
        let events = termination("2026-07-03T02:00:00Z", "s", "NoProgress");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items[0].kind, "budget_stop");
    }

    #[test]
    fn done_pr_green_termination_yields_nothing() {
        let events = termination("2026-07-03T02:00:00Z", "s", "DonePRGreen");
        assert!(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR).is_empty());
    }

    #[test]
    fn merged_pr_block_is_not_wedged() {
        // The real-data false positive: a MERGED PR whose loop still fires is
        // done, not wedged on review. pr_state OPEN gate excludes it.
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "s",
            "block",
            "SUCCESS",
            "MERGED",
            false,
            144,
        );
        assert!(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR).is_empty());
    }

    #[test]
    fn later_allow_clears_the_wedge() {
        let events = [
            loop_check(
                "2026-07-03T02:00:00Z",
                "s",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                5,
            ),
            loop_check(
                "2026-07-03T03:00:00Z",
                "s",
                "allow",
                "SUCCESS",
                "OPEN",
                true,
                5,
            ),
        ]
        .join("\n");
        assert!(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR).is_empty());
    }

    #[test]
    fn termination_after_wedge_wins() {
        // A green-block session that then terminates on DonePRGreen is done.
        let events = [
            loop_check(
                "2026-07-03T02:00:00Z",
                "s",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                5,
            ),
            termination("2026-07-03T03:00:00Z", "s", "DonePRGreen"),
        ]
        .join("\n");
        assert!(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR).is_empty());
    }

    #[test]
    fn wedge_after_a_stale_budget_stop_reads_as_wedge() {
        // A budget stop followed by a fresh loop (re-armed) is live again.
        let events = [
            termination("2026-07-03T02:00:00Z", "s", "Budget"),
            loop_check(
                "2026-07-03T03:00:00Z",
                "s",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                9,
            ),
        ]
        .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items[0].kind, "review_wedged");
    }

    #[test]
    fn termination_with_fractional_ts_still_wins_over_z_loop_check() {
        // Lexically ".5" < "Z", so a same-second fractional termination would
        // sort BEFORE the loop_check and misclassify a real stop; epoch compare
        // fixes it (gemini HIGH finding).
        let events = [
            loop_check(
                "2026-07-03T02:00:00Z",
                "s",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                5,
            ),
            termination("2026-07-03T02:00:00.5", "s", "Budget"),
        ]
        .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].kind, "budget_stop",
            "the termination wins despite its fractional ts"
        );
    }

    #[test]
    fn fires_below_floor_is_not_wedged() {
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "s",
            "block",
            "SUCCESS",
            "OPEN",
            false,
            1,
        );
        assert!(fold(&events, "", ALL, 2).is_empty());
    }

    #[test]
    fn since_window_excludes_old_events() {
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "s",
            "block",
            "SUCCESS",
            "OPEN",
            false,
            5,
        );
        let future = crate::state::rfc3339_like_to_secs("2099-01-01T00:00:00Z").unwrap();
        assert!(fold(&events, "", future, DEFAULT_FIRES_FLOOR).is_empty());
    }

    #[test]
    fn malformed_line_is_skipped_not_aborted() {
        let events = [
            "{ this is not valid json".to_string(),
            loop_check(
                "2026-07-03T02:00:00Z",
                "s",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                5,
            ),
        ]
        .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1, "the good line still folds");
    }

    #[test]
    fn one_item_per_session_latest_wins() {
        // Two sessions, each with a distinct reason.
        let events = [
            loop_check(
                "2026-07-03T02:00:00Z",
                "a",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                5,
            ),
            termination("2026-07-03T02:30:00Z", "b", "Budget"),
        ]
        .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 2);
        // Sorted by ts: the wedge (02:00) before the budget stop (02:30).
        assert_eq!(items[0].kind, "review_wedged");
        assert_eq!(items[1].kind, "budget_stop");
    }

    #[test]
    fn ledger_resolves_node_name_title() {
        let events = loop_check(
            "2026-07-03T02:00:00Z",
            "sess-x",
            "block",
            "SUCCESS",
            "OPEN",
            false,
            5,
        );
        let ledger = r#"{"entries":[{"session_id":"sess-x","graph_node_id":"x-feec","title":"needs queue","worktree":"/w/footnote/x-feec"}]}"#;
        let items = fold(&events, ledger, ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items[0].node.as_deref(), Some("x-feec"));
        assert_eq!(items[0].name.as_deref(), Some("x-feec"));
        assert_eq!(items[0].title.as_deref(), Some("needs queue"));
    }

    #[test]
    fn ledger_resolves_via_sessions_array() {
        let events = termination("2026-07-03T02:00:00Z", "fno-sess", "Budget");
        let ledger = r#"[{"sessions":["uuid-1","fno-sess"],"graph_node_id":"x-1","worktree":"/w/footnote/x-1"}]"#;
        let items = fold(&events, ledger, ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items[0].node.as_deref(), Some("x-1"));
    }

    #[test]
    fn unresolved_session_renders_id_only() {
        let events = termination("2026-07-03T02:00:00Z", "ghost", "Budget");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items[0].node, None);
        assert_eq!(items[0].name, None);
        assert_eq!(items[0].session_id, "ghost");
    }
}
