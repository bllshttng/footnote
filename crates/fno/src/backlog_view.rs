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
        // Unscoped (null/absent project) sorts into the last lane, like the board.
        let project = e.get("project").and_then(|v| v.as_str());
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

    /// One tick: fold in a fresh stat/read (both taken OFF the core loop) and
    /// return the card set to publish, or `None` when nothing changed. A
    /// malformed document keeps the last-good cards (a torn concurrent write
    /// must not blank the lane); a vanished file empties them.
    pub fn tick(
        &mut self,
        stamp: Option<(std::time::SystemTime, u64)>,
        read_if_changed: impl FnOnce() -> Option<String>,
    ) -> Option<Vec<BacklogCard>> {
        if stamp != self.cached_stamp {
            self.cached_stamp = stamp;
            match (read_if_changed(), stamp) {
                (Some(raw), _) => self.cached_raw = Some(raw),
                (None, None) => self.cached_raw = None,
                (None, Some(_)) => {} // read raced a writer: keep last-good
            }
        }
        let cards = match &self.cached_raw {
            Some(raw) => derive_cards(raw)
                .or_else(|| self.last_sent.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
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
    fn malformed_document_is_none_not_empty() {
        assert!(derive_cards("not json").is_none());
        // last-good is kept across a torn write by ReaderState.
        let mut st = ReaderState::default();
        let good = graph(r#"{"id":"a","slug":"s","priority":"p1","_status":"ready"}"#);
        let s1 = Some((std::time::SystemTime::UNIX_EPOCH, good.len() as u64));
        assert_eq!(st.tick(s1, || Some(good.clone())).unwrap().len(), 1);
        // A changed stamp but an unreadable (raced) read keeps the last-good card.
        let s2 = Some((std::time::SystemTime::UNIX_EPOCH, 999));
        assert!(st.tick(s2, || None).is_none()); // unchanged -> no republish
    }
}
