//! The one owner of "which backlog node does this registry row work"
//! (x-5a62, law d-bbcd48b5).
//!
//! The retirement sweep used to key provenance on ONE route, the reverse
//! join from `node.sessions[]` (x-c672). When the forward write into
//! `sessions[]` was missed, nothing else was consulted, and 26 of 44 live
//! rows kept for `no provenance`. The law withdrew that: provenance is
//! RECOVERED from any declared source, the sources cross-check rather than
//! merely substitute, and the retirement records WHICH source answered.
//!
//! The declared order: `Sessions` (the reverse join), `Registry` (the stored
//! `node` field), `Name` (the node id embedded in the row name),
//! `TranscriptFirst` (the first user message, which carries the dispatch
//! brief), `TranscriptLast`. The first source that answers owns the verdict;
//! every later source naming the SAME node corroborates; a later source
//! naming a DIFFERENT node is a conflict and the row is held - two witnesses
//! that disagree are not evidence, and a wrong retirement must be impossible
//! rather than merely rare.
//!
//! There is deliberately no mux-pane source: measured on the live registry,
//! the transcript route already resolves every row a name cannot, and the
//! rows nothing resolves genuinely work no node. The enum is an ordered
//! declared list, so adding `MuxPane` is one variant and one arm.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::gc_sweep::GraphRead;
use crate::graph_store::WorkState;
use crate::state::RegistryEntry;

/// Which provenance source answered. The order IS the declared cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSource {
    Sessions,
    Registry,
    Name,
    TranscriptFirst,
    TranscriptLast,
}

impl NodeSource {
    /// The token the retire basis prints, so a wrong retirement is auditable
    /// by reading one line.
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeSource::Sessions => "sessions",
            NodeSource::Registry => "registry",
            NodeSource::Name => "name",
            NodeSource::TranscriptFirst => "transcript-first",
            NodeSource::TranscriptLast => "transcript-last",
        }
    }
}

/// The cascade verdict for one row: the node, which source answered, every
/// source that agreed, and the first conflict that holds the row.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NodeRoute {
    pub node: Option<String>,
    pub source: Option<NodeSource>,
    pub agreeing: Vec<NodeSource>,
    pub conflict: Option<(NodeSource, String)>,
}

/// Read the transcript bounds the same way `gc::transcript_age_s` picks the
/// freshest match: a session can leave stubs in other project dirs, and a
/// stub must not read as the transcript.
fn newest<'a>(paths: &'a [PathBuf]) -> Option<&'a Path> {
    paths
        .iter()
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
        .map(|p| p.as_path())
}

/// Read at most 256 KiB from one end of the file, as whole lines (the same
/// bound `truth_status._TAIL_BYTES` sets on the Python reads).
const BOUND_BYTES: u64 = 256 * 1024;

fn bound_lines(path: &Path, from_head: bool) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = if from_head {
        0
    } else {
        len.saturating_sub(BOUND_BYTES)
    };
    if start > 0 {
        let Ok(_) = file.seek(SeekFrom::Start(start)) else {
            return Vec::new();
        };
    }
    let mut raw: Vec<u8> = Vec::with_capacity(BOUND_BYTES as usize);
    if file.take(BOUND_BYTES).read_to_end(&mut raw).is_err() {
        return Vec::new();
    }
    let dropped_first = !from_head && start > 0;
    let text = String::from_utf8_lossy(&raw);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if dropped_first && !lines.is_empty() {
        lines.remove(0); // a partial leading line is not a message
    }
    lines
}

/// The first token shaped `[a-z][a-z0-9]*-[0-9a-f]{4,}` that names a graph
/// id. The id-set check is the guard: slug words that only look like ids
/// never answer.
fn first_node_token(text: &str, ids: &HashSet<String>) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_lowercase() {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_lowercase() || bytes[j].is_ascii_digit()) {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'-' {
            let mut k = j + 1;
            while k < bytes.len() && bytes[k].is_ascii_hexdigit() {
                k += 1;
            }
            if k - (j + 1) >= 4 {
                let candidate = &text[start..k];
                if ids.contains(candidate) {
                    return Some(candidate.to_string());
                }
            }
        }
        i = j.max(i + 1);
    }
    None
}

/// The first user message names the node: the dispatch brief travels in it.
fn transcript_first(paths: Option<&[PathBuf]>, ids: &HashSet<String>) -> Option<String> {
    let path = newest(paths?)?;
    for line in bound_lines(path, true) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("user") {
            continue;
        }
        if let Some(node) = first_node_token(&line, ids) {
            return Some(node);
        }
    }
    None
}

/// The last message, scanned back, is the second witness.
fn transcript_last(paths: Option<&[PathBuf]>, ids: &HashSet<String>) -> Option<String> {
    let path = newest(paths?)?;
    for line in bound_lines(path, false).into_iter().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("message").is_none() {
            continue;
        }
        if let Some(node) = first_node_token(&line, ids) {
            return Some(node);
        }
    }
    None
}

/// x-1379's name route, ported verbatim from
/// `git show 76144e5c1:cli/src/fno/agents/retirement.py`. Only tokens 1 and
/// 2 are consulted, so a hex-looking slug word in a later position, such as
/// `feed` in `t-d15a-feed-timeout`, is never read as an id. A bare hex that
/// matches two graph ids is ambiguous and answers nothing.
fn name_route(name: &str, ids: &HashSet<String>) -> Option<String> {
    let tokens: Vec<&str> = name.split('-').collect();
    if tokens.len() < 2 {
        return None;
    }
    let joined = tokens[1..tokens.len().min(3)].join("-");
    if ids.contains(&joined) {
        return Some(joined);
    }
    let mut hex_index: HashMap<&str, &str> = HashMap::new();
    let mut ambiguous: HashSet<&str> = HashSet::new();
    for id in ids {
        let hex_part = id.rsplit('-').next().unwrap_or(id);
        if let Some(prev) = hex_index.get(hex_part) {
            if *prev != id.as_str() {
                ambiguous.insert(hex_part);
            }
        }
        hex_index.insert(hex_part, id);
    }
    let bare = tokens[1];
    if ambiguous.contains(bare) {
        return None;
    }
    hex_index.get(bare).map(|id| id.to_string())
}

/// Run the cascade for one row. `graph` supplies every graph-side source;
/// `transcripts` is the store's matches for the row, newest-read.
pub fn resolve(
    e: &RegistryEntry,
    sid: &str,
    graph: &GraphRead,
    transcripts: Option<&[PathBuf]>,
) -> NodeRoute {
    let ids: HashSet<String> = graph.statuses.keys().cloned().collect();
    let key = crate::graph_store::work_state_key(sid);
    let answers: [Option<String>; 5] = [
        graph
            .index
            .get(&key)
            .and_then(|rows| rows.first())
            .map(|(node, _)| node.clone()),
        e.node.clone(),
        name_route(&e.name, &ids),
        transcript_first(transcripts, &ids),
        transcript_last(transcripts, &ids),
    ];
    let sources = [
        NodeSource::Sessions,
        NodeSource::Registry,
        NodeSource::Name,
        NodeSource::TranscriptFirst,
        NodeSource::TranscriptLast,
    ];
    let mut route = NodeRoute::default();
    for (source, answer) in sources.into_iter().zip(answers) {
        let Some(node) = answer else { continue };
        match &route.node {
            None => {
                route.node = Some(node);
                route.source = Some(source);
            }
            Some(same) if *same == node => route.agreeing.push(source),
            Some(other) => {
                route.conflict = Some((source, node));
                let _ = other;
                break;
            }
        }
    }
    route
}

impl NodeRoute {
    /// The work verdict the resolved node's stored status gives. A resolved
    /// node missing from the statuses map fails OPEN: an unreadable node is
    /// never a done one.
    pub fn work_state(&self, statuses: &HashMap<String, String>) -> WorkState {
        let Some(node) = &self.node else {
            return WorkState::NoProvenance;
        };
        match statuses.get(node).map(String::as_str) {
            Some("done") => WorkState::AllDone {
                nodes: vec![node.clone()],
            },
            Some(status) => WorkState::Open {
                node: node.clone(),
                status: status.to_string(),
            },
            None => WorkState::Open {
                node: node.clone(),
                status: "unknown".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RegistryEntry;

    fn entry(name: &str, node: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            node: node.map(str::to_string),
            ..Default::default()
        }
    }

    fn graph(statuses: &[(&str, &str)]) -> GraphRead {
        GraphRead {
            statuses: statuses
                .iter()
                .map(|(id, s)| (id.to_string(), s.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn ids_of(statuses: &[(&str, &str)]) -> HashSet<String> {
        statuses
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<HashSet<String>>()
    }

    fn write_transcript(dir: &std::path::Path, lines: &[&str]) -> PathBuf {
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    // AC1-HP: the name route resolves what the reverse join missed.
    #[test]
    fn name_route_resolves_node_the_sessions_join_missed() {
        let g = graph(&[("x-c5bf", "done")]);
        let e = entry("target-x-c5bf-gc-sweep-reap-guard-checks-ide", None);
        let route = resolve(&e, "01a078c3-not-anywhere", &g, None);
        assert_eq!(route.node.as_deref(), Some("x-c5bf"));
        assert_eq!(route.source, Some(NodeSource::Name));
    }

    // The registry field outranks the name (x-1379's rule, ported).
    #[test]
    fn registry_field_outranks_name() {
        let g = graph(&[("x-aaaa", "done"), ("x-bbbb", "done")]);
        let e = entry("target-x-aaaa-row", Some("x-aaaa"));
        let route = resolve(&e, "", &g, None);
        assert_eq!(route.node.as_deref(), Some("x-aaaa"));
        assert_eq!(route.source, Some(NodeSource::Registry));
        assert_eq!(route.agreeing, vec![NodeSource::Name]);
    }

    // The sessions join outranks everything; later sources corroborate.
    #[test]
    fn sessions_join_outranks_and_collects_agreement() {
        let mut g = graph(&[("x-aaaa", "done")]);
        g.index.insert(
            "sid-1".to_string(),
            vec![("x-aaaa".to_string(), "done".to_string())],
        );
        let e = entry("target-x-aaaa-row", Some("x-aaaa"));
        let route = resolve(&e, "SID-1", &g, None);
        assert_eq!(route.source, Some(NodeSource::Sessions));
        assert_eq!(route.agreeing, vec![NodeSource::Registry, NodeSource::Name]);
    }

    // AC2-HP: the first user message carries the dispatch brief and names
    // the node.
    #[test]
    fn transcript_first_resolves_when_name_cannot() {
        let tmp = std::env::temp_dir().join(format!("node-route-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = write_transcript(
            &tmp,
            &[
                r#"{"type":"assistant","message":{"content":"started"}}"#,
                r#"{"type":"user","message":{"content":"execute plan for node x-9d11 now"}}"#,
            ],
        );
        let ids = ids_of(&[("x-9d11", "done")]);
        assert_eq!(
            transcript_first(Some(&[path.clone()]), &ids).as_deref(),
            Some("x-9d11")
        );
        let g = graph(&[("x-9d11", "done")]);
        let e = entry("planner-row-no-id-in-name", None);
        let route = resolve(&e, "sid-none", &g, Some(&[path]));
        assert_eq!(route.source, Some(NodeSource::TranscriptFirst));
        std::fs::remove_dir_all(&tmp).ok();
    }

    // AC3-HP: a later source naming the SAME node corroborates.
    #[test]
    fn agreeing_source_is_recorded() {
        let tmp = std::env::temp_dir().join(format!("node-route-agree-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = write_transcript(
            &tmp,
            &[r#"{"type":"user","message":{"content":"work x-aaaa"}}"#],
        );
        let g = graph(&[("x-aaaa", "done")]);
        let e = entry("target-x-aaaa-row", None);
        let route = resolve(&e, "sid-none", &g, Some(&[path]));
        assert_eq!(route.source, Some(NodeSource::Name));
        assert_eq!(
            route.agreeing,
            vec![NodeSource::TranscriptFirst, NodeSource::TranscriptLast]
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // AC4-EDGE: two witnesses that disagree are not evidence.
    #[test]
    fn conflicting_source_holds_the_row() {
        let g = graph(&[("x-aaaa", "done"), ("x-bbbb", "done")]);
        // the registry field is the conflicting witness
        let e = entry("target-x-aaaa-row", Some("x-bbbb"));
        let route = resolve(&e, "sid-none", &g, None);
        assert_eq!(route.node.as_deref(), Some("x-bbbb"));
        assert_eq!(route.source, Some(NodeSource::Registry));
        assert_eq!(
            route.conflict,
            Some((NodeSource::Name, "x-aaaa".to_string()))
        );
        assert!(route.agreeing.is_empty());
    }

    // AC8-EDGE: a bare hex matching two ids is ambiguous and answers
    // nothing; the cascade falls through.
    #[test]
    fn ambiguous_bare_hex_answers_nothing_and_falls_through() {
        let g = graph(&[("x-feed", "done"), ("y-feed", "done")]);
        let e = entry("target-feed-row", None);
        let route = resolve(&e, "sid-none", &g, None);
        assert_eq!(route.node, None);
        assert_eq!(route.source, None);
        assert_eq!(route.work_state(&g.statuses), WorkState::NoProvenance);
    }

    // A hex-looking slug word in a later position is never read as an id
    // (x-1379's rule, ported).
    #[test]
    fn slug_hex_in_later_position_is_ignored() {
        let g = graph(&[("x-d15a", "done"), ("x-feed", "done")]);
        let e = entry("t-x-d15a-feed-timeout", None);
        let route = resolve(&e, "sid-none", &g, None);
        assert_eq!(route.node.as_deref(), Some("x-d15a"));
        assert_eq!(route.source, Some(NodeSource::Name));
    }

    // AC7-EDGE shape: no source resolves, nothing is invented.
    #[test]
    fn no_source_resolves_to_no_provenance() {
        let g = graph(&[("x-aaaa", "done")]);
        let e = entry("codex-turnend-probe", None);
        let route = resolve(&e, "sid-none", &g, None);
        assert_eq!(route, NodeRoute::default());
        assert_eq!(route.work_state(&g.statuses), WorkState::NoProvenance);
    }

    // work_state maps the stored status, failing OPEN on a missing node.
    #[test]
    fn work_state_maps_status_and_fails_open() {
        let statuses = [("x-aaaa".to_string(), "done".to_string())]
            .into_iter()
            .collect();
        let done = NodeRoute {
            node: Some("x-aaaa".into()),
            source: Some(NodeSource::Name),
            ..Default::default()
        };
        assert_eq!(
            done.work_state(&statuses),
            WorkState::AllDone {
                nodes: vec!["x-aaaa".into()]
            }
        );
        let open = NodeRoute {
            node: Some("x-bbbb".into()),
            source: Some(NodeSource::Name),
            ..Default::default()
        };
        assert!(matches!(open.work_state(&statuses), WorkState::Open { .. }));
    }

    // A missing or unreadable transcript answers nothing and is not an error.
    #[test]
    fn unreadable_transcript_answers_nothing() {
        let ids = ids_of(&[("x-aaaa", "done")]);
        assert_eq!(
            transcript_first(Some(&[PathBuf::from("/nonexistent/x")]), &ids),
            None
        );
        assert_eq!(
            transcript_last(Some(&[PathBuf::from("/nonexistent/x")]), &ids),
            None
        );
        assert_eq!(transcript_first(None, &ids), None);
    }

    // The tail bound drops a partial leading line; a full line at the bound
    // still resolves.
    #[test]
    fn transcript_last_scans_back_over_messages() {
        let tmp = std::env::temp_dir().join(format!("node-route-last-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = write_transcript(
            &tmp,
            &[
                r#"{"type":"user","message":{"content":"first x-aaaa"}}"#,
                r#"{"type":"assistant","message":{"content":"last word x-bbbb"}}"#,
            ],
        );
        let ids = ids_of(&[("x-aaaa", "done"), ("x-bbbb", "done")]);
        assert_eq!(
            transcript_last(Some(&[path]), &ids).as_deref(),
            Some("x-bbbb")
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
