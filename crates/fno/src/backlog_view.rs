//! Work-queue reader for the sideline backlog lane (x-6f77).
//!
//! Sibling of [`crate::agents_view`]: an off-loop interval task (in server.rs)
//! parses `~/.fno/graph.json` and hands the core loop a board-ordered card set
//! the sideline renders under a "work queue" header. Same discipline as the
//! registry reader - the core loop and the render path never touch the file;
//! the mtime+len gate skips the 4M read until the graph actually changes (a
//! claim/close mutation bumps mtime, so a card flips to in-flight for free).
//!
//! The graph is dual-language (Python `fno backlog` + the fno-agents daemon)
//! and its FILE is the contract: parsed via `serde_json::Value` with tolerant
//! field access rather than importing the graph crate - the mux needs four
//! fields per node, not the whole model. A malformed document keeps the
//! last-good cards (a torn concurrent write must not blank the lane).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::proto::{BacklogCard, CardState};

/// Board-order cap: the sideline shows the head of the queue, not all 1900
/// nodes. Bounds the wire frame and the render loop; the rest live on the full
/// board (`fno backlog`). Kept generous so a real ready/blocked set is never
/// truncated in practice.
const CARD_CAP: usize = 40;

/// The graph path, resolved as `fno.paths` does: `FNO_GRAPH_JSON` >
/// `$HOME/.fno/graph.json` > `./.fno/graph.json`.
pub fn graph_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_GRAPH_JSON") {
        return PathBuf::from(v);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("graph.json")
}

/// Classify a node's `_status` into a queue state, or `None` to drop it (done /
/// idea / deferred / superseded are not actionable queue work). Authoritative
/// on `_status` alone: a claimed node with a stale `blocked_by` is in-flight,
/// not blocked.
fn classify(status: &str) -> Option<CardState> {
    match status {
        "claimed" | "in-progress" | "in_progress" => Some(CardState::InFlight),
        "ready" | "next" => Some(CardState::Ready),
        "blocked" => Some(CardState::Blocked),
        _ => None,
    }
}

/// Priority rank for the board sort (`p0` first). Unknown sorts last.
fn priority_rank(p: &str) -> u8 {
    match p {
        "p0" => 0,
        "p1" => 1,
        "p2" => 2,
        "p3" => 3,
        _ => 9,
    }
}

/// Derive the board-ordered card set from raw graph JSON. Pure so the ordering
/// and classification are unit-testable without a file. `None` on a malformed
/// document (the caller keeps its last-good cards). The order mirrors the board
/// (`docs/architecture/backlog-board-ordering.md`): project lane (unscoped
/// last), rank band (ranked before unranked, ascending), priority, created_at.
pub fn derive_cards(raw: &str) -> Option<Vec<BacklogCard>> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let entries = doc
        .get("entries")
        .or_else(|| doc.get("nodes"))?
        .as_array()?;

    // (project, is_unscoped, rank, prio, created_at, card) - the tuple carries
    // the sort keys so the comparator never re-reads the JSON.
    let mut rows: Vec<(String, bool, Option<f64>, u8, String, BacklogCard)> =
        Vec::with_capacity(entries.len().min(CARD_CAP * 2));
    for e in entries {
        let status = e.get("_status").and_then(|v| v.as_str()).unwrap_or("");
        let Some(state) = classify(status) else {
            continue;
        };
        let Some(id) = e.get("id").and_then(|v| v.as_str()) else {
            continue; // a node with no id is unrenderable; skip it
        };
        let slug = e.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let priority = e.get("priority").and_then(|v| v.as_str()).unwrap_or("p2");
        // Unscoped (null/absent/empty/whitespace project) sorts into the last
        // lane, like the board's UNSCOPED_LABEL - trim so `""`/`"  "` are not
        // treated as a named lane sorting before real projects.
        let project = e
            .get("project")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let unscoped = project.is_none();
        let rank = e.get("rank").and_then(|v| v.as_f64());
        let created = e.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        rows.push((
            project.unwrap_or("").to_string(),
            unscoped,
            rank,
            priority_rank(priority),
            created.to_string(),
            BacklogCard {
                id: id.to_string(),
                slug: slug.to_string(),
                priority: priority.to_string(),
                state,
                // Routes are a publish-time server join (panes/registry),
                // not graph state - the reader always derives them empty.
                pane_id: None,
                attach_id: None,
                where_hint: None,
            },
        ));
    }

    rows.sort_by(|a, b| {
        // Lane: named projects alphabetical, the unscoped lane last.
        a.1.cmp(&b.1)
            .then_with(|| a.0.cmp(&b.0))
            // Rank band: any finite rank (band 0, ascending) before unranked.
            .then_with(|| rank_band(a.2).cmp(&rank_band(b.2)))
            .then_with(|| match (a.2, b.2) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.3.cmp(&b.3))
            .then_with(|| a.4.cmp(&b.4))
    });

    Some(rows.into_iter().take(CARD_CAP).map(|r| r.5).collect())
}

/// Ranked nodes (band 0) sort ahead of unranked ones (band 1).
fn rank_band(rank: Option<f64>) -> u8 {
    match rank {
        Some(r) if r.is_finite() => 0,
        _ => 1,
    }
}

/// Parse `fno-agents claim sweep --json` stdout into the live-claim map the
/// overlay consumes: node id -> claim holder, for claims whose `state` is
/// `"live"` under a `node:` / `dispatch:` key (x-54fa). Only `"live"` counts
/// as in-flight (Locked 2); the sweep's other states (`stale`/`suspect`/...)
/// never flip a card. `None` on unparseable output so the caller keeps its
/// last-good sweep — a flaky tick must not downgrade in-flight cards.
///
/// The mux deliberately parses only this pinned JSON verdict: claim YAML,
/// classification, and liveness live in `fno-agents` alone.
pub fn live_claims_from_sweep(stdout: &str) -> Option<HashMap<String, String>> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let arr = v.get("claims")?.as_array()?;
    let mut live: HashMap<String, String> = HashMap::new();
    for c in arr {
        if c.get("state").and_then(|s| s.as_str()) != Some("live") {
            continue;
        }
        let Some(key) = c.get("key").and_then(|k| k.as_str()) else {
            continue;
        };
        let holder = c
            .get("holder")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        // A node held by both a `dispatch:` and a `node:` claim keeps the
        // `node:` holder (the worker session) for display; either alone
        // marks the id in-flight.
        if let Some(id) = key.strip_prefix("node:") {
            live.insert(id.to_string(), holder);
        } else if let Some(id) = key.strip_prefix("dispatch:") {
            live.entry(id.to_string()).or_insert(holder);
        }
    }
    Some(live)
}

/// Overlay live lockfile claims onto graph-derived cards (x-54fa): a card
/// whose id holds a live `node:`/`dispatch:` claim renders InFlight,
/// overriding Ready AND Blocked (a claimed node with a stale `blocked_by` is
/// in-flight — this module's documented stance). Pure; ids not in the card
/// set are ignored (no phantom cards), ids join by node id only.
pub fn overlay_claims(cards: &mut [BacklogCard], live: &HashMap<String, String>) {
    for c in cards.iter_mut() {
        if live.contains_key(&c.id) {
            c.state = CardState::InFlight;
        }
    }
}

/// The reader's between-tick memory (mtime-gated document cache + last-sent
/// cards), mirroring [`crate::agents_view::ReaderState`]. The interval task
/// lives in server.rs (it owns the `CoreMsg` sender); this keeps the derivation
/// pure and unit-testable.
#[derive(Default)]
pub struct ReaderState {
    cached_raw: Option<String>,
    cached_stamp: Option<(std::time::SystemTime, u64)>,
    last_sent: Option<Vec<BacklogCard>>,
}

impl ReaderState {
    /// The stamp of the currently-cached document (mtime+len gate: the caller
    /// skips the file read when this matches a fresh stat).
    pub fn cached_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.cached_stamp
    }

    /// One tick: fold in a fresh stat/read (both taken OFF the core loop),
    /// overlay live claims, and return the card set to publish, or `None` when
    /// nothing changed. A malformed document keeps the last-good cards (a torn
    /// concurrent write must not blank the lane); a vanished file empties them.
    ///
    /// `live` is the last-good claim sweep (`None` = no sweep has ever
    /// succeeded: render un-overlaid, today's behavior). The overlay applies
    /// INSIDE the change gate so a claim appearing/releasing republishes even
    /// when the graph file itself is untouched (x-54fa AC1-HP / AC1-EDGE).
    pub fn tick(
        &mut self,
        stamp: Option<(std::time::SystemTime, u64)>,
        read_if_changed: impl FnOnce() -> Option<String>,
        live: Option<&HashMap<String, String>>,
    ) -> Option<Vec<BacklogCard>> {
        if stamp != self.cached_stamp {
            match (read_if_changed(), stamp) {
                // Only commit the new stamp once we have matching content, so a
                // torn read (raced a writer: file exists but read yielded None)
                // is RE-TRIED on the next tick instead of pinning the stale card
                // set until the graph's mtime changes again (gemini HIGH; this
                // improves on the older agents_view reader's advance-anyway).
                (Some(raw), _) => {
                    self.cached_stamp = stamp;
                    self.cached_raw = Some(raw);
                }
                (None, None) => {
                    self.cached_stamp = stamp;
                    self.cached_raw = None; // file vanished: empty the lane
                }
                (None, Some(_)) => {} // torn read: keep last-good AND retry next tick
            }
        }
        let mut cards = match &self.cached_raw {
            Some(raw) => derive_cards(raw)
                .or_else(|| self.last_sent.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        if let Some(live) = live {
            overlay_claims(&mut cards, live);
        }
        if self.last_sent.as_ref() != Some(&cards) {
            self.last_sent = Some(cards.clone());
            Some(cards)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: &str) -> String {
        format!(r#"{{"entries": [{nodes}]}}"#)
    }

    #[test]
    fn only_queue_states_survive_classification() {
        let raw = graph(
            r#"{"id":"a","slug":"ready-one","priority":"p1","_status":"ready"},
               {"id":"b","slug":"blocked-one","priority":"p2","_status":"blocked"},
               {"id":"c","slug":"live-one","priority":"p0","_status":"claimed"},
               {"id":"d","slug":"done-one","priority":"p1","_status":"done"},
               {"id":"e","slug":"idea-one","priority":"p2","_status":"idea"}"#,
        );
        let cards = derive_cards(&raw).unwrap();
        let ids: Vec<_> = cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["c", "a", "b"],
            "done/idea dropped; board order by priority"
        );
        assert_eq!(cards[0].state, CardState::InFlight);
        assert_eq!(cards[1].state, CardState::Ready);
        assert_eq!(cards[2].state, CardState::Blocked);
    }

    #[test]
    fn board_order_project_then_rank_then_priority() {
        // fno before (unscoped); within fno, ranked before unranked; then prio.
        let raw = graph(
            r#"{"id":"unscoped","slug":"u","priority":"p0","_status":"ready"},
               {"id":"fno-unranked","slug":"fu","priority":"p1","_status":"ready","project":"fno"},
               {"id":"fno-ranked","slug":"fr","priority":"p3","_status":"ready","project":"fno","rank":1.0}"#,
        );
        let ids: Vec<_> = derive_cards(&raw)
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        // fno lane first: ranked (band 0) beats the higher-priority unranked one,
        // then the unscoped card last.
        assert_eq!(ids, ["fno-ranked", "fno-unranked", "unscoped"]);
    }

    #[test]
    fn empty_or_whitespace_project_sorts_as_unscoped_last() {
        // A `""`/whitespace project must land in the unscoped lane (last), not
        // sort as a named lane before real projects (gemini/codex P2).
        let raw = graph(
            r#"{"id":"blank","slug":"b","priority":"p0","_status":"ready","project":"  "},
               {"id":"fno","slug":"f","priority":"p3","_status":"ready","project":"fno"}"#,
        );
        let ids: Vec<_> = derive_cards(&raw)
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(
            ids,
            ["fno", "blank"],
            "named lane first; blank-project last"
        );
    }

    #[test]
    fn malformed_document_is_none_not_empty() {
        assert!(derive_cards("not json").is_none());
        // last-good is kept across a torn write by ReaderState.
        let mut st = ReaderState::default();
        let good = graph(r#"{"id":"a","slug":"s","priority":"p1","_status":"ready"}"#);
        let s1 = Some((std::time::SystemTime::UNIX_EPOCH, good.len() as u64));
        assert_eq!(st.tick(s1, || Some(good.clone()), None).unwrap().len(), 1);
        // A changed stamp but a torn (None) read keeps the last-good card AND
        // does not commit the new stamp, so the read is retried next tick.
        let s2 = Some((std::time::SystemTime::UNIX_EPOCH, 999));
        assert!(st.tick(s2, || None, None).is_none()); // last-good unchanged -> no republish
                                                       // Retry at the same stamp now succeeds with two cards -> republished
                                                       // (proves the torn read did not pin the stale set).
        let two = graph(
            r#"{"id":"a","slug":"s","priority":"p1","_status":"ready"},
               {"id":"b","slug":"t","priority":"p2","_status":"blocked"}"#,
        );
        assert_eq!(st.tick(s2, || Some(two.clone()), None).unwrap().len(), 2);
    }

    // ---- claims overlay (x-54fa) -----------------------------------------

    fn live(ids: &[(&str, &str)]) -> HashMap<String, String> {
        ids.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn overlay_flips_ready_and_blocked_to_in_flight() {
        // AC1-HP: ready + live claim renders InFlight. AC2-EDGE: overlay
        // beats Blocked. Ids not in the live map are untouched; live ids not
        // in the card set are ignored (no phantom cards).
        let raw = graph(
            r#"{"id":"x-rdy","slug":"r","priority":"p1","_status":"ready"},
               {"id":"x-blk","slug":"b","priority":"p2","_status":"blocked","blocked_by":["x-rdy"]},
               {"id":"x-free","slug":"f","priority":"p2","_status":"ready"}"#,
        );
        let mut cards = derive_cards(&raw).unwrap();
        overlay_claims(
            &mut cards,
            &live(&[
                ("x-rdy", "target-session:abc"),
                ("x-blk", "dispatch-node:1"),
                ("x-ghost", "nobody"),
            ]),
        );
        let by_id = |id: &str| cards.iter().find(|c| c.id == id).unwrap().state;
        assert_eq!(by_id("x-rdy"), CardState::InFlight);
        assert_eq!(by_id("x-blk"), CardState::InFlight);
        assert_eq!(by_id("x-free"), CardState::Ready);
        assert_eq!(cards.len(), 3, "no phantom card for x-ghost");
    }

    #[test]
    fn overlay_change_republishes_without_graph_change() {
        // AC1-HP/AC1-EDGE: a claim appearing (and later releasing) flips the
        // card within a tick even though the graph stamp never moves.
        let mut st = ReaderState::default();
        let raw = graph(r#"{"id":"x-a","slug":"s","priority":"p1","_status":"ready"}"#);
        let s = Some((std::time::SystemTime::UNIX_EPOCH, raw.len() as u64));
        let first = st.tick(s, || Some(raw.clone()), None).unwrap();
        assert_eq!(first[0].state, CardState::Ready);
        // Claim appears: same stamp, no re-read, card republishes InFlight.
        let claimed = live(&[("x-a", "target-session:abc")]);
        let flipped = st.tick(s, || None, Some(&claimed)).unwrap();
        assert_eq!(flipped[0].state, CardState::InFlight);
        // Unchanged claim set: no republish.
        assert!(st.tick(s, || None, Some(&claimed)).is_none());
        // Claim released: card reverts to Ready and republishes.
        let released = live(&[]);
        let reverted = st.tick(s, || None, Some(&released)).unwrap();
        assert_eq!(reverted[0].state, CardState::Ready);
    }

    #[test]
    fn sweep_parse_takes_only_live_node_and_dispatch_claims() {
        let stdout = r#"{"claims":[
            {"key":"node:x-a","state":"live","holder":"target-session:abc","host":"h","pid":1},
            {"key":"dispatch:x-a","state":"live","holder":"dispatch-node:9","host":"h","pid":9},
            {"key":"dispatch:x-b","state":"live","holder":"advance:2","host":"h","pid":2},
            {"key":"node:x-c","state":"stale","holder":"gone","host":"h","pid":3},
            {"key":"node:x-d","state":"suspect","holder":"maybe","host":"h","pid":4}
        ]}"#;
        let live = live_claims_from_sweep(stdout).unwrap();
        // Only live claims count; node: holder preferred over dispatch:.
        assert_eq!(live.len(), 2);
        assert_eq!(live["x-a"], "target-session:abc");
        assert_eq!(live["x-b"], "advance:2");
        // Unparseable output is None (keep last-good), not an empty map.
        assert!(live_claims_from_sweep("not json").is_none());
        assert!(live_claims_from_sweep(r#"{"no_claims":1}"#).is_none());
    }
}
