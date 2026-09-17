//! The read/begin reply splice: how one full-graph read reply becomes wire
//! bytes without re-copying the cached tree. The frame equals
//! `encode(TAG_RESPONSE, to_vec(handle_request(...)))` (AC6); everything a
//! splice refuses falls back to the handle_request path so reply shapes
//! never change.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use serde_json::Value;

use super::encode;
use super::TAG_RESPONSE;
use super::{canonical_row_digests, read_graph_gated, remember_snapshot, GraphRead, StoreState};

/// The full-graph read replies spliced from the cache's serialized views.
/// `read`, `begin` and api `rows` are the replies whose bodies are one
/// cached byte run; a rows reply built as a Value would deep-copy the whole
/// tree per request (measured 934 MB idle).
pub(super) enum SplicedReply {
    Read {
        id: u64,
        entries: Arc<Vec<u8>>,
    },
    Begin {
        id: u64,
        version: String,
        digests: Arc<Vec<u8>>,
        entries: Arc<Vec<u8>>,
    },
    Rows {
        id: u64,
        rows: Arc<Vec<u8>>,
        version: i64,
    },
}

/// The cache's serialized views ride as shared pieces, written without a
/// copy; only the small envelope is per-request.
enum SplicePiece {
    Shared(Arc<Vec<u8>>),
    Inline(Vec<u8>),
}

impl SplicePiece {
    fn len(&self) -> usize {
        match self {
            SplicePiece::Shared(bytes) => bytes.len(),
            SplicePiece::Inline(bytes) => bytes.len(),
        }
    }

    fn write_to(&self, stream: &mut UnixStream) -> std::io::Result<()> {
        match self {
            SplicePiece::Shared(bytes) => stream.write_all(bytes),
            SplicePiece::Inline(bytes) => stream.write_all(bytes),
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        match self {
            SplicePiece::Shared(bytes) => bytes.as_ref().clone(),
            SplicePiece::Inline(bytes) => bytes.clone(),
        }
    }
}

impl SplicedReply {
    /// (prefix, pieces, suffix): the cache's byte runs ride as shared
    /// pieces, written without a copy; only the small envelope is
    /// per-request.
    fn pieces(&self) -> (Vec<u8>, Vec<SplicePiece>, std::borrow::Cow<'static, [u8]>) {
        use std::borrow::Cow;
        match self {
            SplicedReply::Read { id, entries } => (
                format!(r#"{{"id":{id},"ok":true,"result":{{"entries":"#).into_bytes(),
                vec![SplicePiece::Shared(Arc::clone(entries))],
                Cow::Borrowed(&b"}}"[..]),
            ),
            SplicedReply::Begin {
                id,
                version,
                digests,
                entries,
            } => (
                format!(r#"{{"id":{id},"ok":true,"result":{{"version":"#).into_bytes(),
                vec![
                    SplicePiece::Inline(serde_json::to_vec(version).unwrap_or_default()),
                    SplicePiece::Inline(br#","base_digests":"#.to_vec()),
                    SplicePiece::Shared(Arc::clone(digests)),
                    SplicePiece::Inline(br#","entries":"#.to_vec()),
                    SplicePiece::Shared(Arc::clone(entries)),
                ],
                Cow::Borrowed(&b"}}"[..]),
            ),
            SplicedReply::Rows { id, rows, version } => (
                format!(r#"{{"id":{id},"ok":true,"result":{{"rows":"#).into_bytes(),
                vec![
                    SplicePiece::Shared(Arc::clone(rows)),
                    SplicePiece::Inline(format!(r#","version":{version}"#).into_bytes()),
                ],
                Cow::Borrowed(&b"}}"[..]),
            ),
        }
    }

    pub(super) fn write_to(&self, stream: &mut UnixStream) -> std::io::Result<()> {
        let (prefix, pieces, suffix) = self.pieces();
        let payload_len =
            prefix.len() + pieces.iter().map(SplicePiece::len).sum::<usize>() + suffix.len();
        let mut head = [0u8; 5];
        head[0] = TAG_RESPONSE;
        head[1..5].copy_from_slice(&(payload_len as u32).to_le_bytes());
        stream.write_all(&head)?;
        stream.write_all(&prefix)?;
        for piece in &pieces {
            piece.write_to(stream)?;
        }
        stream.write_all(&suffix)?;
        stream.flush()
    }

    /// The assembled frame, for the byte-equality tests only.
    #[cfg(test)]
    pub(super) fn frame(&self) -> Vec<u8> {
        let (prefix, pieces, suffix) = self.pieces();
        let mut out = Vec::new();
        let payload_len =
            prefix.len() + pieces.iter().map(SplicePiece::len).sum::<usize>() + suffix.len();
        out.push(TAG_RESPONSE);
        out.extend_from_slice(&(payload_len as u32).to_le_bytes());
        out.extend_from_slice(&prefix);
        for piece in &pieces {
            out.extend_from_slice(&piece.bytes());
        }
        out.extend_from_slice(&suffix);
        out
    }
}

/// The splice attempt for one request frame: `read` (never keep_malformed),
/// `begin`, and the api `rows` op. Anything else - another method, a
/// request that is not JSON, an uncached (Fresh) read, a store error -
/// returns None and the caller falls through to handle_request, so error
/// shapes and odd methods keep their exact reply. A spliced `begin` still
/// remembers its snapshot for the commit_rows conflict path.
pub(super) fn splice_reply(state: &StoreState, payload: &[u8]) -> Option<SplicedReply> {
    let req: Value = serde_json::from_slice(payload).ok()?;
    let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
    let method = req.get("method").and_then(Value::as_str)?;
    let mut rows_op = false;
    match method {
        "read" => {
            if req
                .get("params")
                .map(|p| {
                    p.get("keep_malformed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
            {
                return None;
            }
        }
        "begin" => {}
        "api" => {
            // Only the plain rows op: it is the one api reply whose body is
            // the whole cached byte run.
            if req
                .get("params")
                .map(|p| p.get("op").and_then(Value::as_str) == Some("rows"))
                .unwrap_or(false)
            {
                rows_op = true;
            } else {
                return None;
            }
        }
        _ => return None,
    }
    let _gate = state.gate.read().unwrap_or_else(|e| e.into_inner());
    match read_graph_gated(state, false).ok()? {
        GraphRead::Fresh(..) => None,
        GraphRead::Cached(graph) => {
            let entries = graph
                .entries_json
                .get_or_init(|| Arc::new(serde_json::to_vec(&*graph.entries).unwrap_or_default()))
                .clone();
            if entries.is_empty() {
                return None;
            }
            if rows_op {
                let version =
                    crate::backlog::api::version(&crate::backlog::api::Store::new(&state.graph))
                        .ok()?;
                return Some(SplicedReply::Rows {
                    id,
                    rows: entries,
                    version,
                });
            }
            if method == "read" {
                return Some(SplicedReply::Read { id, entries });
            }
            let version = graph.version.clone();
            let digests = graph
                .base_digests_json
                .get_or_init(|| {
                    Arc::new(
                        serde_json::to_vec(&canonical_row_digests(&graph.entries))
                            .unwrap_or_default(),
                    )
                })
                .clone();
            remember_snapshot(state, &version, &graph.entries);
            Some(SplicedReply::Begin {
                id,
                version,
                digests,
                entries,
            })
        }
    }
}
