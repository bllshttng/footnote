//! The peer-lead blocked reading: each live peer lead's last transcript rows,
//! and the one lead whose tail is a run of Stop-loop blocks over an hour old.
//! A codex lead spun 371 blocked lines over 2.5 days while every check-in
//! printed `live 0/16` and nothing named it, so the check-in now reads the
//! lead's own last turns the way it reads the workers.
//!
//! The read is bounded: only the trailing 512 KiB of each transcript, and
//! only the trailing run of blocked rows counts. A clean or unreadable
//! transcript is never a finding; a failed registry read is a failed
//! READER line, never a quiet zero.

use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};

/// The tail depth a peer transcript is read to. A blocked lead's loop rows
/// sit at the end; nothing older is evidence.
const TAIL_BYTES: u64 = 512 * 1024;

/// A trailing run shorter than this is a one-off, not a loop.
const MIN_RUN: usize = 3;

/// The Stop-loop signatures a blocked lead's tail fills with. The first is
/// the codex goal-arbitration refusal (the measured 371-line loop), the
/// other two the claude Stop-hook block spellings.
const BLOCK_MARKERS: [&str; 3] = [
    "conflicting goal truth",
    "Stop hook error: continue working",
    "Stop hook feedback",
];

/// One blocked peer lead: who, on which harness, since when, how many
/// trailing rows carry the loop.
pub(crate) struct BlockedPeer {
    pub(crate) holder: String,
    pub(crate) harness: String,
    pub(crate) since: String,
    pub(crate) rows: usize,
}

impl BlockedPeer {
    /// The check-in line: the finding and the remedy in one sentence.
    pub(crate) fn line(&self) -> String {
        format!(
            "PEER BLOCKED: {} ({}) looping on Stop-hook refusals since {} ({} trailing blocked rows); \
             fno agents resume {} restores the crowned sandbox and objective",
            self.holder, self.harness, self.since, self.rows, self.holder
        )
    }
}

/// The trailing-run finding over one transcript's tail text, pure so the
/// classifier is testable without a registry or a store. `now` anchors the
/// one-hour floor.
pub(crate) fn blocked_peer(
    text: &str,
    holder: &str,
    harness: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<BlockedPeer> {
    let mut run_rows = 0usize;
    let mut run_start: Option<chrono::DateTime<chrono::Utc>> = None;
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let blocked = BLOCK_MARKERS.iter().any(|m| trimmed.contains(m));
        let ts = serde_json::from_str::<Value>(trimmed)
            .ok()
            .and_then(|value| {
                value
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .and_then(|raw| {
                chrono::DateTime::parse_from_rfc3339(&raw)
                    .ok()
                    .map(|parsed| parsed.with_timezone(&chrono::Utc))
            });
        if blocked {
            run_rows += 1;
            if ts.is_some() {
                run_start = ts;
            }
            continue;
        }
        // A parseable, non-blocked row ends the trailing run; unparsable
        // text (the tail cut mid-row) is skipped, not evidence either way.
        if ts.is_some() {
            break;
        }
    }
    let since = run_start?;
    if run_rows < MIN_RUN {
        return None;
    }
    if (now - since).num_hours() < 1 {
        return None;
    }
    Some(BlockedPeer {
        holder: holder.to_string(),
        harness: harness.to_string(),
        since: since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        rows: run_rows,
    })
}

/// The last `TAIL_BYTES` of one transcript, with the first (partial) line
/// dropped so a cut mid-row never reads as a row.
fn tail_text(path: &std::path::Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(idx) = text.find('\n') {
            text.drain(..=idx);
        }
    }
    Some(text)
}

/// One peer lead's transcript tail when a reader exists and the file reads,
/// else None. Codex reads the rollout, claude the session JSONL; every other
/// harness has no reader, and a missing rollout (an exited lead whose store
/// was cleaned) is a normal skip, not a blind spot.
fn transcript_for(harness: &str, session_id: &str) -> Option<String> {
    let path = match harness {
        "codex" => crate::lead_history::hygiene_transcript_for_holder("codex", session_id),
        "claude" => crate::claude_drive::find_transcript(session_id),
        _ => None,
    };
    tail_text(&path?)
}

/// The reading: every live peer lead's tail read once, the blocked rows
/// returned. The lead running the check-in is not its own peer.
pub(super) fn reading() -> Result<Value, String> {
    let (own_session, _) = crate::claims::resolve_identity();
    let registry =
        crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
            .map_err(|e| format!("registry unreadable: {e}"))?;
    let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
    let mut rows = Vec::new();
    for row in &registry.entries {
        if row
            .crown_scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            continue;
        }
        if !crate::spawn_gate::status_is_liveish(&row.status) {
            continue;
        }
        if own_session
            .as_deref()
            .is_some_and(|own| row.harness_session_id.as_deref() == Some(own))
        {
            continue;
        }
        let Some(session_id) = row.harness_session_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let harness = row.harness.as_deref().unwrap_or("");
        let Some(tail) = transcript_for(harness, session_id) else {
            continue;
        };
        if let Some(blocked) = blocked_peer(&tail, &row.name, harness, now) {
            rows.push(json!({
                "holder": blocked.holder,
                "harness": blocked.harness,
                "since": blocked.since,
                "rows": blocked.rows,
                "line": blocked.line(),
            }));
        }
    }
    rows.sort_by(|a, b| s_str(&a, "since").cmp(&s_str(&b, "since")));
    Ok(json!(rows))
}

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// The render: one line per blocked peer, or a READER FAILED line. A clean
/// read prints nothing - the beat's silence is the good news.
pub(super) fn lines(readings: &[crate::lead_checkin::Reading]) -> Vec<String> {
    let reading = readings.iter().find(|r| r.name == "peer_blocked");
    let Some(reading) = reading else {
        return Vec::new();
    };
    if !reading.ok {
        return vec![format!("READER FAILED peer_blocked: {}", reading.error)];
    }
    reading
        .value
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("line").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The attention rows the beat's change derivation folds in, so a blocked
/// peer never journals as "no change".
pub(super) fn attention(data: &serde_json::Map<String, Value>) -> Vec<String> {
    data.get("peer_blocked_rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("line").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: &str, text: &str) -> String {
        json!({"timestamp": ts, "payload": {"type": "message", "text": text}}).to_string()
    }

    #[test]
    fn a_trailing_block_run_over_an_hour_old_names_the_peer_and_the_remedy() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-05T22:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let old = "2026-10-05T20:00:00Z";
        let fresh = "2026-10-05T21:59:00Z";
        let mut text = vec![
            row("2026-10-05T19:00:00Z", "shipped a wave"),
            row(
                old,
                "conflicting goal truth: expected objective, got resume",
            ),
            row("2026-10-05T20:30:00Z", "Stop hook error: continue working"),
            row(fresh, "conflicting goal truth: got resume again"),
        ]
        .join("\n");
        // A trailing partial row (the tail cut) is skipped, not evidence.
        text.push_str("\n{\"timestamp\": \"2026-10-05T21");
        let blocked = blocked_peer(&text, "rowan", "codex", now).expect("blocked run");
        assert_eq!(blocked.rows, 3);
        assert_eq!(blocked.since, old.to_string());
        let line = blocked.line();
        assert!(line.starts_with("PEER BLOCKED: rowan (codex)"), "{line}");
        assert!(
            line.contains("fno agents resume rowan restores the crowned sandbox"),
            "{line}"
        );

        // Under the floor: the same run one hour younger stays quiet.
        let young = "2026-10-05T21:20:00Z";
        let text = [
            row(
                young,
                "conflicting goal truth: expected objective, got resume",
            ),
            row("2026-10-05T21:30:00Z", "Stop hook error: continue working"),
            row(fresh, "conflicting goal truth: got resume again"),
        ]
        .join("\n");
        assert!(blocked_peer(&text, "rowan", "codex", now).is_none());

        // A short run is a one-off, never a loop.
        let text = [
            row("2026-10-05T19:00:00Z", "worked"),
            row("2026-10-05T20:00:00Z", "conflicting goal truth: got resume"),
            row(fresh, "worked again"),
        ]
        .join("\n");
        assert!(blocked_peer(&text, "rowan", "codex", now).is_none());

        // A clean tail blocks nothing.
        let text = [row("2026-10-05T19:00:00Z", "worked"), row(fresh, "worked")].join("\n");
        assert!(blocked_peer(&text, "rowan", "codex", now).is_none());
    }
}
