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

/// Below this age, a pile of unharvested carve-outs or stale claims is not yet
/// a needs-me item - only a pile that has actually gone stale belongs in the
/// queue (x-801b measured 44 carve-out rows, oldest 29 days; x-e3be measured
/// 573 claim files, oldest specimens 56-75 days). Both legs are "somebody
/// should look at this pile" signals, read from durable on-disk state rather
/// than a recent event, so neither is windowed by `since` at all.
const DECISION_STALE_FLOOR_SECS: u64 = 7 * 24 * 60 * 60;

/// One reason a session needs a human, resolved and ready to render. `kind` is
/// a stable string (`review_wedged` | `budget_stop`) the client maps to its own
/// severity enum; the fold does not rank (the client owns the full 6-kind
/// order, of which this leg populates two).
///
/// A kind the client does not map is dropped from the operator view by its
/// `_ => continue` arm, which is how `mail_delivery_miss` stays out of the
/// panel while still flowing into the journal for a wake consumer to read.
/// Emitting a new kind here is therefore a quiet change on the operator
/// surface, never a loud one: to SHOW a new kind, map it there too.
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

/// The latest loop_check state observed for a session. "Latest" is by
/// `(epoch, seq)`, NOT file order: events from a project + global events.jsonl
/// are concatenated, so a later-in-file line can be older in time; comparing by
/// parsed epoch (with a monotonic fold `seq` tiebreak for same-second events)
/// keeps the truly newest state per kind.
#[derive(Default, Clone)]
struct LoopState {
    decision: String,
    intent: String,
    ci: String,
    pr_state: String,
    reviewed: bool,
    fires: u64,
    ts: String,
    epoch: u64,
    seq: usize,
}

/// The latest termination observed for a session (same `(epoch, seq)` ordering).
#[derive(Default, Clone)]
struct TermState {
    reason: String,
    ts: String,
    epoch: u64,
    seq: usize,
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
    // Monotonic fold position, breaking same-second ties in true stream order.
    let mut seq = 0usize;
    // mail_escalation events carry no session_id (they are about mail between
    // agents, not a target session), so they cannot enter the session-keyed
    // accumulator and the session gate below would drop them. They get their own
    // per-recipient accumulator (latest (epoch, seq) wins) and render
    // squadless-live in the client.
    // Keyed on (recipient, kind), not recipient alone. A standing question and a
    // delivery miss to the same handle are different things, and the client
    // renders one while dropping the other, so collapsing them would let a later
    // miss erase an escalation a human still owes an answer to.
    let mut mail_escalations: HashMap<(String, String), (u64, usize, NeedItem)> = HashMap::new();
    // operator_question / operator_question_closed, keyed by question_id (not
    // per-recipient-latest-wins like mail_escalation above): several distinct
    // decisions can be open on the operator at once, so every open question_id
    // gets its own row. Mirrors `outstanding/core.py::read_open_questions`.
    let mut questions: HashMap<String, (u64, usize, NeedItem)> = HashMap::new();
    let mut closed_questions: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in events_raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // torn/malformed tail line: skip, never abort (digest precedent)
        };
        let ts = event_ts(&v);
        let kind = event_kind(&v);
        // operator_question/_closed are exempt from the `since` window: a
        // question does not expire (no auto-close, x-99dc), so time-windowing
        // it here would silently resurrect the exact failure that node fixed
        // for the SessionStart block - just inside the mux queue instead.
        let windowed = !matches!(
            kind,
            Some("operator_question") | Some("operator_question_closed")
        );
        if windowed && !in_window(ts, since) {
            continue;
        }
        if kind == Some("operator_question") {
            let Some(qid) = str_field(&v, "question_id") else {
                continue;
            };
            let question = str_field(&v, "question").unwrap_or("");
            let evidence = str_field(&v, "ask").unwrap_or(question);
            let node = str_field(&v, "node").map(str::to_string);
            let name = node
                .clone()
                .or_else(|| str_field(&v, "cwd").map(|c| basename(c).to_string()));
            let session_id = str_field(&v, "session_id").unwrap_or(qid).to_string();
            let epoch = to_epoch_lenient(ts).unwrap_or(0);
            seq += 1;
            if questions
                .get(qid)
                .is_none_or(|(e, s, _)| (epoch, seq) >= (*e, *s))
            {
                questions.insert(
                    qid.to_string(),
                    (
                        epoch,
                        seq,
                        NeedItem {
                            kind: "operator_question".to_string(),
                            session_id,
                            node,
                            name,
                            title: None,
                            ts: ts.to_string(),
                            evidence: evidence.to_string(),
                            // Stamped always-live by stamp_liveness (no node claim).
                            live: false,
                        },
                    ),
                );
            }
            continue;
        }
        if kind == Some("operator_question_closed") {
            if let Some(qid) = str_field(&v, "question_id") {
                closed_questions.insert(qid.to_string());
            }
            continue;
        }
        // mail_escalation is folded before the session gate: it carries no
        // session_id (it is mail between agents), so the gate below would drop
        // it. One NeedItem per (recipient, kind), latest of that pair wins; the
        // kind comes from the row's reason.
        if kind == Some("mail_escalation") {
            let Some(recipient) = str_field(&v, "recipient") else {
                continue;
            };
            let reason = str_field(&v, "reason").unwrap_or("");
            let sender = str_field(&v, "sender").unwrap_or("");
            let summary = str_field(&v, "summary").unwrap_or("");
            // A reachable-miss is an agent-to-agent DELIVERY failure: the
            // recipient was reachable, the live inject missed, the durable copy
            // is queued. It needs a retry or a wake, not an operator decision,
            // so it gets its own kind and the client's `_ => continue` arm keeps
            // it out of the needs panel by construction rather than by a filter
            // someone can forget to apply. attended-miss stays a question:
            // there the operator IS the attended recipient.
            // Named need_kind, not kind: the enclosing `kind` is the EVENT
            // type, and shadowing it here reads as the same thing twice.
            let need_kind = if reason == "reachable-miss" {
                "mail_delivery_miss"
            } else {
                "mail_question"
            };
            let epoch = to_epoch_lenient(ts).unwrap_or(0);
            seq += 1;
            // Same (epoch, seq) ordering as the session accumulator: a cross-source
            // concat never lets an older line clobber a newer escalation.
            let acc_key = (recipient.to_string(), need_kind.to_string());
            if mail_escalations
                .get(&acc_key)
                .is_none_or(|(e, s, _)| (epoch, seq) >= (*e, *s))
            {
                mail_escalations.insert(
                    acc_key,
                    (
                        epoch,
                        seq,
                        NeedItem {
                            kind: need_kind.to_string(),
                            // No target session; the recipient handle is the row's
                            // stable identity (id_key) and the roster join key.
                            session_id: recipient.to_string(),
                            node: None,
                            name: Some(recipient.to_string()),
                            title: None,
                            ts: ts.to_string(),
                            evidence: format!("{reason}: {sender} -> {recipient}: {summary}"),
                            // Stamped always-live by stamp_liveness (no node claim).
                            live: false,
                        },
                    ),
                );
            }
            continue;
        }
        let Some(sid) = str_field(&v, "session_id") else {
            continue; // an event with no session can't be joined to a row
        };
        if !matches!(
            kind,
            Some("loop_check") | Some("termination") | Some("loop_terminated")
        ) {
            continue;
        }
        let epoch = to_epoch_lenient(ts).unwrap_or(0);
        seq += 1;
        let acc = sessions.entry(sid.to_string()).or_default();
        match kind {
            Some("loop_check") => {
                // Keep the newest by (epoch, seq): a later-in-file but older-in-
                // time line (cross-source concat) never clobbers newer state.
                if acc
                    .latest_loop
                    .as_ref()
                    .is_none_or(|c| (epoch, seq) >= (c.epoch, c.seq))
                {
                    acc.latest_loop = Some(LoopState {
                        decision: str_field(&v, "decision").unwrap_or("").to_string(),
                        intent: str_field(&v, "intent").unwrap_or("").to_string(),
                        ci: str_field(&v, "ci").unwrap_or("").to_string(),
                        pr_state: str_field(&v, "pr_state").unwrap_or("").to_string(),
                        reviewed: field(&v, "reviewed")
                            .and_then(|r| r.as_bool())
                            .unwrap_or(false),
                        fires: field(&v, "fires").and_then(|f| f.as_u64()).unwrap_or(0),
                        ts: ts.to_string(),
                        epoch,
                        seq,
                    });
                }
            }
            _ => {
                if acc
                    .latest_term
                    .as_ref()
                    .is_none_or(|c| (epoch, seq) >= (c.epoch, c.seq))
                {
                    acc.latest_term = Some(TermState {
                        reason: str_field(&v, "reason").unwrap_or("").to_string(),
                        ts: ts.to_string(),
                        epoch,
                        seq,
                    });
                }
            }
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
    for (_, (_, _, item)) in mail_escalations {
        items.push(item);
    }
    for (qid, (_, _, item)) in questions {
        if closed_questions.contains(&qid) {
            continue;
        }
        items.push(item);
    }
    // `kind` is part of the sort key because it is part of the mail accumulator's
    // key: one recipient can now yield both a mail_question and a
    // mail_delivery_miss, and for mail rows session_id IS the recipient, so those
    // two tie on (ts, session_id). `sort_by` is stable, so a tie would fall back
    // to push order, which comes from HashMap iteration and is randomized per
    // process. That makes `needs --json` reorder run to run and anything
    // diffing it flaky.
    items.sort_by(|a, b| {
        a.ts.cmp(&b.ts)
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| a.kind.cmp(&b.kind))
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
    // Order by (epoch, seq), the same key the accumulator kept: epoch first (so
    // a fractional Python-isoformat stop is not misordered against a Z loop_check
    // by a lexical string compare), then the monotonic fold seq so a same-second
    // loop_check that RE-ARMS after a termination wins over the stop (codex P2).
    let terminated = match (&acc.latest_term, &acc.latest_loop) {
        (Some(t), Some(l)) => (t.epoch, t.seq) >= (l.epoch, l.seq),
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
    // `intent == "promise"` is load-bearing: loopcheck Step 5 also blocks a
    // still-WORKING session that has opened a green PR with `intent:"none"`
    // (no promise, no backstop) - that is not wedged on review, it just has not
    // promised yet. Only a promise-intent block on a green OPEN unreviewed PR
    // that keeps re-firing is a review wedge (codex P2).
    let wedged = l.decision == "block"
        && l.intent == "promise"
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
/// `~/.fno/events.jsonl` + `~/.fno/questions.jsonl` + `~/.fno/ledger.json`.
fn default_sources(home: &AgentsHome) -> (Vec<PathBuf>, PathBuf) {
    let fno_dir = home
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let global_events = fno_dir.join("events.jsonl");
    let questions = fno_dir.join("questions.jsonl");
    let project_events = PathBuf::from(".fno").join("events.jsonl");
    let ledger = fno_dir.join("ledger.json");
    (vec![project_events, global_events, questions], ledger)
}

/// Stamp each item's `live` bit from its node claim (x-feec 1.4): an item whose
/// node holds a Live or Suspect claim (a suspect TTL-unexpired claim still
/// protects the slot) renders even without a roster row; an unclaimed or
/// node-less one stays `live=false` and the client drops it when unjoined. This
/// is the IO half of the fold, kept out of the pure [`fold`] so it stays testable.
fn stamp_liveness(mut items: Vec<NeedItem>) -> Vec<NeedItem> {
    for item in &mut items {
        // A mail escalation is always-live by design: it carries no node claim
        // (it is about mail between agents, not a target session), so the
        // node-keyed stamp below would mark it dead and the client's
        // squadless-render branch would drop it -- the silent eat this closes.
        // The live bit here is an honest "surface this with no roster row"
        // label, not a session-liveness claim.
        // Same reasoning for operator_question (no node claim behind a
        // question either) and for the aggregate carveout_stale/stale_claims
        // rows (no single node owns a pile of carve-outs or claims).
        // mail_delivery_miss rides the same rule: the client drops it from the
        // operator view, but it stays in the JSON for a wake consumer, and a
        // node-keyed stamp would label it dead when nothing was ever claimed.
        // `worker_refused` carries no node either (it is registry-derived,
        // not ledger-resolved), and unlike those rows it needs no honest
        // fiction: the item exists ONLY because `refused_worker_items` just
        // probed the row and it answered -- refused-but-reachable IS live.
        if matches!(
            item.kind.as_str(),
            "mail_question"
                | "mail_delivery_miss"
                | "operator_question"
                | "carveout_stale"
                | "stale_claims"
                | "worker_refused"
        ) {
            item.live = true;
            continue;
        }
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

/// One unharvested-carve-out summary row, when the pile has actually gone
/// stale (>= [`DECISION_STALE_FLOOR_SECS`] since the oldest entry). Reads
/// `.fno/carveouts.jsonl` directly (one JSON object per line, same append-only
/// convention as `events.jsonl`) rather than the events fold: a carve-out has
/// no natural expiry, so this is never windowed by `since` at all.
///
/// One aggregate row, not one per carve-out: x-801b measured 44 rows at once,
/// and a needs-me queue flooded with individual rows would just get truncated
/// by the client's worst-first cap anyway. "N carve-outs, oldest Xd" is what
/// actually answers "should I go clean this up."
pub fn carveout_age_item(carveouts_raw: &str, now: u64) -> Option<NeedItem> {
    let mut count = 0u64;
    let mut oldest: Option<(u64, String)> = None;
    for line in carveouts_raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue; // malformed line: skip, never abort
        };
        let Some(ts) = v.get("ts").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(epoch) = to_epoch_lenient(ts) else {
            continue;
        };
        // `backfill` rows are the /pr-merged slot's, not the retro sweep's
        // (it skips them entirely), so counting them would mint a Decision
        // row whose advertised remedy cannot clear it.
        if v.get("kind").and_then(|k| k.as_str()) == Some("backfill") {
            continue;
        }
        count += 1;
        if oldest.as_ref().is_none_or(|(e, _)| epoch < *e) {
            oldest = Some((epoch, ts.to_string()));
        }
    }
    let (oldest_epoch, oldest_ts) = oldest?;
    let age = now.saturating_sub(oldest_epoch);
    if age < DECISION_STALE_FLOOR_SECS {
        return None;
    }
    Some(NeedItem {
        kind: "carveout_stale".to_string(),
        // No target session; this is a pile-level signal. A stable constant
        // key so the client's cursor re-anchor (keyed on session_id) does not
        // drift row identity across folds.
        session_id: "carveouts".to_string(),
        node: None,
        name: None,
        title: None,
        ts: oldest_ts,
        evidence: format!(
            "{count} unharvested carve-out(s), oldest {}d - fno retro sweep-carveouts --apply",
            age / 86_400
        ),
        live: false, // stamped true by stamp_liveness
    })
}

/// A claim's identity + age, read by the IO layer from the claims directory
/// (mirrors what `client_verbs::claim_sweep_payload` already scans) and handed
/// to [`stale_claim_item`], which stays pure and testable.
pub struct ClaimAge {
    pub key: String,
    pub holder: String,
    pub acquired_at_ms: i64,
    pub state: crate::claims::ClaimState,
}

/// One stale-claim summary row, when the oldest `Stale` claim has actually
/// gone stale for a while (>= [`DECISION_STALE_FLOOR_SECS`]). A `Stale` claim
/// (dead pid, TTL expired) that is minutes old is normal churn; one that is
/// weeks old is an orphaned lock nobody cleaned up (x-e3be measured 573 claim
/// files, 570 dead pids, oldest specimens 56-75 days).
///
/// One aggregate row, not one per claim, for the same reason as
/// [`carveout_age_item`]: 570 individual rows would just be truncated by the
/// client's worst-first cap.
pub fn stale_claim_item(claims: &[ClaimAge], now_ms: i64) -> Option<NeedItem> {
    let oldest = claims
        .iter()
        .filter(|c| matches!(c.state, crate::claims::ClaimState::Stale))
        .min_by_key(|c| c.acquired_at_ms)?;
    let stale_count = claims
        .iter()
        .filter(|c| matches!(c.state, crate::claims::ClaimState::Stale))
        .count();
    let age_secs = ((now_ms - oldest.acquired_at_ms).max(0) / 1000) as u64;
    if age_secs < DECISION_STALE_FLOOR_SECS {
        return None;
    }
    Some(NeedItem {
        kind: "stale_claims".to_string(),
        session_id: "claims".to_string(),
        node: None,
        name: None,
        title: None,
        // The oldest claim's acquire time, so band sorting orders the row by
        // its true age instead of an empty string floating it above every
        // fresher operator question.
        ts: chrono::DateTime::from_timestamp_millis(oldest.acquired_at_ms)
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default(),
        evidence: format!(
            "{stale_count} stale claim(s), oldest {}d ({}, holder {})",
            age_secs / 86_400,
            oldest.key,
            oldest.holder
        ),
        live: false, // stamped true by stamp_liveness
    })
}

/// One item per registry row the progress axis classifies `refused`
/// (x-cbd9 Task 3.4): alive, reachable, and unable to think, because its
/// endpoint was handed a model it cannot serve. Nothing else rotates this
/// state today -- `status` reads `live`, the pid is real, and every roster
/// reader that stops at reachability keeps it forever. This is the leg that
/// makes the progress axis actionable rather than decorative.
///
/// Reads the live registry directly (IO layer, like [`carveout_age_item`] /
/// [`stale_claim_item`]), never from `events.jsonl`: a refusal is a current
/// FACT about a row, not an event that happened once. One truth probe per
/// row -- the same probe `fno agents list` already pays for `status` -- so
/// this leg costs nothing a live roster read did not already cost.
/// Best-effort: an unreadable registry or a probe failure degrades to no
/// items, never a crash of `fno agents needs`.
fn refused_worker_items(home: &AgentsHome) -> Vec<NeedItem> {
    let registry = match crate::daemon::load_registry_asserted(&home.registry_json()) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut items = Vec::new();
    for e in &registry.entries {
        let handle = crate::daemon::registry_truth_handle(e);
        let probe = crate::claude_ask::family1_truth_probe(&handle);
        let (progress, _basis) = crate::daemon::progress_from_truth(
            probe.as_ref(),
            e.harness_name(),
            e.route_settings_path.as_deref(),
        );
        if progress != "refused" {
            continue;
        }
        let model = probe
            .as_ref()
            .and_then(|p| p.observed_model.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown model");
        items.push(NeedItem {
            kind: "worker_refused".to_string(),
            session_id: e
                .harness_session_id
                .clone()
                .unwrap_or_else(|| e.name.clone()),
            // No ledger resolution here (this leg is registry-derived, not
            // event-derived, unlike every other item in this file) -- the
            // client joins on `name` instead, same as `carveout_age_item` /
            // `stale_claim_item`.
            node: None,
            name: Some(e.name.clone()),
            title: Some(format!("{} refused: answering as {model}", e.name)),
            ts: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            evidence: format!(
                "{} is alive and reachable but answering as {model}, not a claude model",
                e.name
            ),
            live: false, // stamped by stamp_liveness
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

/// Scan `dir` for `node:` / `dispatch:` claim files and their age, for
/// [`stale_claim_item`]. Same prefix pair `client_verbs::claim_sweep_payload`
/// scans (an orphaned dispatch lock is as dead as an orphaned node lock), and
/// the same read primitives. Fail-open: a missing/unreadable dir or an
/// unparseable lockfile yields fewer entries, never an error - same posture
/// as `claim_sweep_payload`.
fn scan_claim_ages(dir: &Path) -> Vec<ClaimAge> {
    let node_pfx = crate::claims::encode_key("node:");
    let dispatch_pfx = crate::claims::encode_key("dispatch:");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".lock")
            || !(name.starts_with(&node_pfx) || name.starts_with(&dispatch_pfx))
        {
            continue;
        }
        let Ok(rec) = crate::claims::read_claim_file(&entry.path()) else {
            continue; // gone-away or corrupted: skip, never abort
        };
        if !(rec.key.starts_with("node:") || rec.key.starts_with("dispatch:")) {
            continue; // filename lied about its own key: exclude like the sweep does
        }
        let state = crate::claims::classify(&rec, None);
        out.push(ClaimAge {
            key: rec.key,
            holder: rec.holder,
            acquired_at_ms: rec.acquired_at,
            state,
        });
    }
    out
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
    let explicit_events = !args.events_override.is_empty();
    let mut event_paths = if explicit_events {
        args.events_override
    } else {
        default_events
    };
    // `fno outstanding ask` appends to the CANONICAL checkout's
    // `.fno/events.jsonl` (never a linked worktree's), while the default
    // project journal above is cwd-relative and this verb inherits the
    // caller's cwd. Without the canonical journal in the set, a question
    // asked from a worktree is invisible to the fold.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let canonical = crate::paths::canonical_repo_root(&cwd);
    if let Some(root) = &canonical {
        let canonical_events = root.join(".fno").join("events.jsonl");
        let cwd_events = cwd.join(".fno").join("events.jsonl");
        if !explicit_events
            && canonical_events != cwd_events
            && !event_paths.contains(&canonical_events)
        {
            event_paths.push(canonical_events);
        }
    }
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
    let mut items = fold(&events_raw, &ledger_raw, since, args.fires_floor);

    // Carve-out-age and stale-claim legs: durable on-disk state, not events,
    // so they are read directly here (IO layer) rather than folded from
    // `events_raw`, and never windowed by `since` (see the doc comments on
    // `carveout_age_item` / `stale_claim_item`). The ledger lives under the
    // canonical checkout's `.fno/` (`resolve_carveout_root` on the write
    // side); a worktree only sees it through a skip-if-missing symlink, so
    // read the canonical path directly and fall back to cwd outside a repo.
    let carveouts_path = canonical
        .map(|r| r.join(".fno").join("carveouts.jsonl"))
        .unwrap_or_else(|| PathBuf::from(".fno").join("carveouts.jsonl"));
    let carveouts_raw = std::fs::read_to_string(&carveouts_path).unwrap_or_default();
    if let Some(item) = carveout_age_item(&carveouts_raw, now_secs()) {
        items.push(item);
    }
    if let Some(dir) = crate::claims::claims_dir_for(None) {
        let ages = scan_claim_ages(&dir);
        if let Some(item) = stale_claim_item(&ages, crate::claims::now_ms()) {
            items.push(item);
        }
    }
    items.extend(refused_worker_items(home));

    let items = stamp_liveness(items);

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

    // A promise-intent loop_check (the review-wedge case). Non-wedge tests care
    // about the other fields, so a promise intent is the convenient default.
    fn loop_check(
        ts: &str,
        session: &str,
        decision: &str,
        ci: &str,
        pr_state: &str,
        reviewed: bool,
        fires: u64,
    ) -> String {
        loop_check_i(
            ts, session, decision, "promise", ci, pr_state, reviewed, fires,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn loop_check_i(
        ts: &str,
        session: &str,
        decision: &str,
        intent: &str,
        ci: &str,
        pr_state: &str,
        reviewed: bool,
        fires: u64,
    ) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"loop_check","source":"hook","data":{{"session_id":"{session}","decision":"{decision}","intent":"{intent}","ci":"{ci}","pr_state":"{pr_state}","reviewed":{reviewed},"fires":{fires}}}}}"#
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

    fn mail_escalation(
        ts: &str,
        reason: &str,
        sender: &str,
        recipient: &str,
        summary: &str,
    ) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"mail_escalation","source":"target","data":{{"reason":"{reason}","sender":"{sender}","recipient":"{recipient}","summary":"{summary}"}}}}"#
        )
    }

    #[test]
    fn mail_escalation_folds_to_mail_question_without_a_session() {
        // The trap this node closes: a mail_escalation carries no session_id, so
        // the session gate and the loop_check kind filter would both drop it. The
        // fold arm handles it before the gate and emits a mail_question row keyed
        // by recipient (the row identity, not a target session).
        let events = mail_escalation(
            "2026-07-03T02:00:00Z",
            "question",
            "etl",
            "web",
            "which schema?",
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "mail_question");
        assert_eq!(items[0].name.as_deref(), Some("web"));
        assert_eq!(items[0].session_id, "web");
        assert!(items[0].evidence.contains("question"));
        assert!(items[0].evidence.contains("etl -> web"));
    }

    #[test]
    fn mail_escalation_is_stamped_always_live_with_no_node() {
        // No node claim -> the node-keyed stamp would mark it dead and the
        // client's squadless-render branch would drop it. stamp_liveness marks
        // mail_question always-live so it renders with no roster row.
        let events = mail_escalation(
            "2026-07-03T02:00:00Z",
            "attended-miss",
            "ops",
            "claude-9a06",
            "need you",
        );
        let items = stamp_liveness(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].node, None);
        assert!(
            items[0].live,
            "mail_question is always-live even with no node"
        );
    }

    #[test]
    fn mail_escalation_latest_per_recipient_wins() {
        let events = format!(
            "{}\n{}\n",
            mail_escalation("2026-07-03T02:00:00Z", "question", "etl", "web", "old"),
            mail_escalation("2026-07-03T03:00:00Z", "attended-miss", "ops", "web", "new"),
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1, "one row per recipient");
        assert!(
            items[0].evidence.contains("new"),
            "latest (epoch, seq) wins"
        );
    }

    #[test]
    fn reachable_miss_folds_to_its_own_kind() {
        // Measured 2026-08-17: three of twelve operator rows were the king's own
        // outbound mail that missed a reachable recipient, sorted beside a real
        // question. A miss needs a retry or a wake, not a human.
        let events = mail_escalation(
            "2026-07-03T02:00:00Z",
            "reachable-miss",
            "web",
            "019f48e1",
            "ping",
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "mail_delivery_miss");
    }

    #[test]
    fn question_and_reachable_miss_do_not_share_a_kind() {
        // The question row is the positive control: a fold that stopped emitting
        // anything at all would satisfy a bare "the miss is not a question"
        // assertion and read as proof of a split that is not there.
        let events = format!(
            "{}\n{}\n",
            mail_escalation(
                "2026-07-03T02:00:00Z",
                "question",
                "etl",
                "web",
                "which auth?"
            ),
            mail_escalation(
                "2026-07-03T03:00:00Z",
                "reachable-miss",
                "sender",
                "9a06",
                "ping"
            ),
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 2, "one row per recipient, both present");
        let mut kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
        kinds.sort();
        assert_eq!(kinds, vec!["mail_delivery_miss", "mail_question"]);
    }

    #[test]
    fn a_later_delivery_miss_does_not_erase_a_standing_question() {
        // The trap the kind split opens if the map stays keyed on recipient
        // alone: latest-wins would replace the question row with a
        // mail_delivery_miss, which the client drops, so an escalation the code
        // itself calls a human's problem vanishes from the panel. A worker asks
        // `ops` a question, then anyone else's send to `ops` misses, and the
        // question is gone. The debounce is per (sender, recipient), so a
        // second sender never suppresses the miss.
        let events = format!(
            "{}\n{}\n",
            mail_escalation(
                "2026-07-03T02:00:00Z",
                "question",
                "etl",
                "ops",
                "which auth?"
            ),
            mail_escalation(
                "2026-07-03T03:00:00Z",
                "reachable-miss",
                "web",
                "ops",
                "ping"
            ),
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        let question = items
            .iter()
            .find(|i| i.kind == "mail_question")
            .expect("the standing question survives a later delivery miss");
        assert!(question.evidence.contains("which auth?"));
        assert!(
            items.iter().any(|i| i.kind == "mail_delivery_miss"),
            "and the miss is still folded, under its own kind"
        );
    }

    #[test]
    fn one_recipient_two_kinds_sorts_deterministically() {
        // Keying the mail accumulator on (recipient, kind) lets one handle yield
        // two rows. For mail rows session_id IS the recipient, so with an equal
        // ts they tie on both of the old sort keys, and a stable sort then falls
        // back to push order, which comes from HashMap iteration and is
        // randomized per process. Folding the same input repeatedly has to give
        // one order, or `needs --json` reorders run to run.
        let events = format!(
            "{}\n{}\n",
            mail_escalation(
                "2026-07-03T02:00:00Z",
                "question",
                "etl",
                "ops",
                "which auth?"
            ),
            mail_escalation(
                "2026-07-03T02:00:00Z",
                "reachable-miss",
                "web",
                "ops",
                "ping"
            ),
        );
        let first: Vec<String> = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR)
            .into_iter()
            .map(|i| i.kind)
            .collect();
        assert_eq!(first.len(), 2, "both rows survive the fold");
        for _ in 0..24 {
            let again: Vec<String> = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR)
                .into_iter()
                .map(|i| i.kind)
                .collect();
            assert_eq!(again, first, "same input, same order");
        }
    }

    #[test]
    fn attended_miss_stays_a_question() {
        // The operator IS the attended recipient, so an attended-miss is still a
        // human's problem. Only the machine-to-machine miss moves.
        let events = mail_escalation(
            "2026-07-03T02:00:00Z",
            "attended-miss",
            "ops",
            "claude-9a06",
            "need you",
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "mail_question");
    }

    #[test]
    fn same_second_rearm_after_termination_is_not_terminated() {
        // A Budget stop then a same-second re-armed loop_check: the loop's higher
        // fold seq wins the (epoch, seq) tiebreak, so the session reads as live
        // again (review_wedged), not budget-stopped (codex P2).
        let events = [
            termination("2026-07-03T02:00:00Z", "s", "Budget"),
            loop_check(
                "2026-07-03T02:00:00Z",
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
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "review_wedged");
    }

    #[test]
    fn newer_state_survives_older_line_from_a_later_source() {
        // Simulate project+global concat where the LATER-in-file line is OLDER:
        // an old allow appended after a newer block must not clobber the block.
        let events = [
            loop_check(
                "2026-07-03T05:00:00Z",
                "s",
                "block",
                "SUCCESS",
                "OPEN",
                false,
                9,
            ),
            loop_check(
                "2026-07-03T01:00:00Z",
                "s",
                "allow",
                "SUCCESS",
                "OPEN",
                true,
                3,
            ),
        ]
        .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(
            items.len(),
            1,
            "the newer block state survives the older line"
        );
        assert_eq!(items[0].kind, "review_wedged");
    }

    #[test]
    fn intent_none_block_is_not_wedged() {
        // A still-WORKING session that opened a green OPEN PR blocks with
        // intent:none (no promise yet); it is not wedged on review (codex P2).
        let events = loop_check_i(
            "2026-07-03T02:00:00Z",
            "s",
            "block",
            "none",
            "SUCCESS",
            "OPEN",
            false,
            9,
        );
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

    // --- operator_question (x-e3be: NeedKind::Decision producer) --------------

    fn operator_question(ts: &str, qid: &str, question: &str, node: Option<&str>) -> String {
        let node_field = node
            .map(|n| format!(r#","node":"{n}""#))
            .unwrap_or_default();
        format!(
            r#"{{"ts":"{ts}","type":"operator_question","source":"target","data":{{"question_id":"{qid}","question":"{question}"{node_field}}}}}"#
        )
    }

    fn operator_question_closed(ts: &str, qid: &str) -> String {
        format!(
            r#"{{"ts":"{ts}","type":"operator_question_closed","source":"target","data":{{"question_id":"{qid}"}}}}"#
        )
    }

    #[test]
    fn open_operator_question_folds_to_decision_producer_kind() {
        let events = operator_question(
            "2026-07-03T02:00:00Z",
            "q-abc",
            "auto-merge or hold?",
            Some("x-e3be"),
        );
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "operator_question");
        assert_eq!(items[0].node.as_deref(), Some("x-e3be"));
        assert!(items[0].evidence.contains("auto-merge or hold?"));
    }

    #[test]
    fn question_index_is_a_default_source_and_ask_is_folded_as_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join(".fno");
        let home = AgentsHome::at(state_dir.join("agents"));
        std::fs::create_dir_all(&state_dir).unwrap();
        let index = state_dir.join("questions.jsonl");
        std::fs::write(
            &index,
            r#"{"ts":"2026-07-03T02:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-index-only","question":"long context","ask":"pick alpha or beta"}}
"#,
        )
        .unwrap();

        let (sources, _) = default_sources(&home);
        let events = sources
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);

        assert!(
            sources.contains(&index),
            "the machine-wide question index is read"
        );
        assert_eq!(items.len(), 1, "the index-only row reaches the needs fold");
        assert_eq!(items[0].session_id, "q-index-only");
        assert_eq!(items[0].evidence, "pick alpha or beta");
    }

    #[test]
    fn closed_operator_question_is_dropped() {
        let events = [
            operator_question("2026-07-03T02:00:00Z", "q-abc", "auto-merge or hold?", None),
            operator_question_closed("2026-07-03T03:00:00Z", "q-abc"),
        ]
        .join("\n");
        assert!(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR).is_empty());
    }

    #[test]
    fn operator_question_survives_a_56_day_old_since_window() {
        // The defect this leg closes: a 24h-windowed fold hides a question
        // that has been open for weeks. `since` here is "now" (2099), so a
        // windowed kind would be excluded - operator_question must not be.
        let events = operator_question("2026-06-01T00:00:00Z", "q-old", "still waiting", None);
        let since = crate::state::rfc3339_like_to_secs("2099-01-01T00:00:00Z").unwrap();
        let items = fold(&events, "", since, DEFAULT_FIRES_FLOOR);
        assert_eq!(
            items.len(),
            1,
            "operator_question bypasses the since window"
        );
        assert_eq!(items[0].kind, "operator_question");
    }

    #[test]
    fn multiple_open_questions_each_get_their_own_row() {
        // Unlike mail_escalation's latest-per-recipient fold, several distinct
        // decisions can be outstanding on the operator at once.
        let events = [
            operator_question("2026-07-03T02:00:00Z", "q-1", "decision one", None),
            operator_question("2026-07-03T02:05:00Z", "q-2", "decision two", None),
        ]
        .join("\n");
        let items = fold(&events, "", ALL, DEFAULT_FIRES_FLOOR);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn loop_check_stays_windowed_when_operator_question_is_exempt() {
        // The window-bypass restructure must not accidentally widen the window
        // for the existing kinds - only operator_question/_closed are exempt.
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
    fn operator_question_always_live_with_no_node() {
        let events = operator_question("2026-07-03T02:00:00Z", "q-abc", "which?", None);
        let items = stamp_liveness(fold(&events, "", ALL, DEFAULT_FIRES_FLOOR));
        assert!(items[0].live, "operator_question is always-live");
    }

    // --- carveout_age_item ------------------------------------------------

    fn carveout(ts: &str, kind: &str) -> String {
        format!(r#"{{"ts":"{ts}","kind":"{kind}","id":"c-1"}}"#)
    }

    #[test]
    fn stale_carveout_pile_produces_one_aggregate_item() {
        let raw = [
            carveout("2026-06-01T00:00:00Z", "oos-bug"),
            carveout("2026-06-15T00:00:00Z", "deferred"),
            // The /pr-merged slot's rows are not sweep-harvestable; counting
            // one would mint a row the advertised remedy cannot clear.
            carveout("2026-06-20T00:00:00Z", "backfill"),
        ]
        .join("\n");
        let now = crate::state::rfc3339_like_to_secs("2026-07-15T00:00:00Z").unwrap();
        let item = carveout_age_item(&raw, now).expect("29-day-old pile is stale");
        assert_eq!(item.kind, "carveout_stale");
        assert!(item.evidence.contains("2 unharvested"));
        assert!(item.evidence.contains("44d"));
    }

    #[test]
    fn fresh_carveout_pile_produces_no_item() {
        let raw = carveout("2026-07-14T12:00:00Z", "oos-bug");
        let now = crate::state::rfc3339_like_to_secs("2026-07-15T00:00:00Z").unwrap();
        assert!(carveout_age_item(&raw, now).is_none());
    }

    #[test]
    fn empty_carveout_ledger_produces_no_item() {
        let now = crate::state::rfc3339_like_to_secs("2026-07-15T00:00:00Z").unwrap();
        assert!(carveout_age_item("", now).is_none());
    }

    // --- stale_claim_item ---------------------------------------------------

    fn claim_age(
        key: &str,
        holder: &str,
        acquired_at_ms: i64,
        state: crate::claims::ClaimState,
    ) -> ClaimAge {
        ClaimAge {
            key: key.to_string(),
            holder: holder.to_string(),
            acquired_at_ms,
            state,
        }
    }

    #[test]
    fn old_stale_claim_produces_one_aggregate_item() {
        let now_ms = 1_800_000_000_000_i64; // an arbitrary "now"
        let fifty_six_days_ms = 56 * 24 * 60 * 60 * 1000;
        let claims = vec![claim_age(
            "node:x-orphan",
            "dead-holder",
            now_ms - fifty_six_days_ms,
            crate::claims::ClaimState::Stale,
        )];
        let item = stale_claim_item(&claims, now_ms).expect("56-day-old stale claim");
        assert_eq!(item.kind, "stale_claims");
        assert!(item.evidence.contains("node:x-orphan"));
        assert!(item.evidence.contains("dead-holder"));
        assert!(item.evidence.contains("56d"));
        // ts carries the oldest claim's acquire time (band sort orders by age),
        // never an empty string that floats the row above fresher questions.
        // 56 days before the test's now (1.8e12 ms) lands in 2026-11.
        assert!(item.ts.starts_with("2026-11-20T"), "ts={}", item.ts);
    }

    #[test]
    fn live_claim_never_counts_toward_staleness() {
        let now_ms = 1_800_000_000_000_i64;
        let fifty_six_days_ms = 56 * 24 * 60 * 60 * 1000;
        let claims = vec![claim_age(
            "node:x-active",
            "live-holder",
            now_ms - fifty_six_days_ms,
            crate::claims::ClaimState::Live,
        )];
        assert!(stale_claim_item(&claims, now_ms).is_none());
    }

    #[test]
    fn recently_stale_claim_produces_no_item() {
        let now_ms = 1_800_000_000_000_i64;
        let claims = vec![claim_age(
            "node:x-recent",
            "h",
            now_ms - 60_000,
            crate::claims::ClaimState::Stale,
        )];
        assert!(stale_claim_item(&claims, now_ms).is_none());
    }
}
