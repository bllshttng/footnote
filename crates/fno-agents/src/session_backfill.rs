//! `fno backlog session backfill`: fill a session row's missing
//! `started_at` or `ended_at` from a recorded fact (user ruling 2026-09-24).
//! A dry run by default; `--apply` writes. It never overwrites a stamp, and
//! a row with no transcript on disk keeps its gap. It is its own verb, never
//! part of the schema-4 migration, because the migration invents no time.
//!
//! The start is the first invocation of the phase's verb in the session's
//! transcript: a typed `/fno:<verb>` or `$fno:<verb>` that opens a user
//! message, or a Skill tool call naming `fno:<verb>`. The end is the last
//! event before a later phase verb, or the transcript's last event once it
//! has been idle for a day. A ship row ends at its merge commit
//! (`phase_close`). A do row never ends here: an open do row can be live
//! work, and it ends through its gated settle.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::backlog::api::{self, SessionFill, Store};
use crate::graph_store::entry_id;

/// A transcript quiet this long is a finished session.
const IDLE_SECS: u64 = 86_400;

/// The phase an fno verb opens.
fn phase_of(verb: &str) -> Option<&'static str> {
    match verb {
        "think" => Some("think"),
        "blueprint" => Some("blueprint"),
        "target" | "execute" | "do" => Some("do"),
        "review" => Some("review"),
        "pr" | "ship" => Some("ship"),
        _ => None,
    }
}

/// One transcript read: each timestamped event, with the phase it opens.
struct Transcript {
    events: Vec<(String, Option<&'static str>)>,
    idle: bool,
}

pub fn run(args: &[String]) -> i32 {
    let mut graph: Option<PathBuf> = None;
    let (mut apply, mut as_json) = (false, false);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--graph" => graph = it.next().map(PathBuf::from),
            "--apply" => apply = true,
            "--json" | "-J" => as_json = true,
            other => {
                eprintln!("session-backfill: unknown argument {other:?}");
                return 2;
            }
        }
    }
    let Some(graph) = graph else {
        eprintln!("session-backfill: --graph <graph.json> is required");
        return 2;
    };
    let store = Store::new(&graph);
    let entries = match api::rows(&store) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("session-backfill: graph unreadable: {}", err.0);
            return 1;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let codex = codex_rollouts();
    let mut cache: HashMap<(String, String), Option<Transcript>> = HashMap::new();
    let (fills, mut counts) = plan(&entries, &mut |harness, sid| {
        cache
            .entry((harness.to_string(), sid.to_string()))
            .or_insert_with(|| {
                let path = match harness {
                    "claude" => crate::claude_drive::find_transcript(sid),
                    "codex" => codex.get(&sid.to_ascii_lowercase()).cloned(),
                    _ => None,
                }?;
                read_transcript(&path, now)
            })
            .as_ref()
            .map(|t| (t.events.clone(), t.idle))
    });
    let ships = crate::phase_close::ship_backfill_plan(&entries);
    for _ in &ships {
        bump(&mut counts, "ship", "end_filled");
    }
    let mut refused = Vec::new();
    let (mut stamps, mut ship_ends) = (0, 0);
    if apply {
        match api::session_backfill(&store, &fills) {
            Ok(n) => stamps = n,
            Err(err) => refused.push(json!({"write": "stamps", "error": err.0})),
        }
        match api::phase_end(&store, "ship", &ships) {
            Ok(n) => ship_ends = n,
            Err(err) => refused.push(json!({"write": "ship ends", "error": err.0})),
        }
    }
    let report = json!({
        "applied": apply,
        "phases": counts,
        "written": {"stamps": stamps, "ship_ends": ship_ends},
        "refused": refused,
    });
    if as_json {
        println!("{report}");
    } else {
        println!(
            "session backfill ({})",
            if apply {
                "applied"
            } else {
                "dry run; --apply writes"
            }
        );
        for (phase, c) in &counts {
            let n = |k: &str| c.get(k).copied().unwrap_or(0);
            println!(
                "  {phase:<9} start {}/{} filled   end {}/{} filled   no transcript {}",
                n("start_filled"),
                n("start_missing"),
                n("end_filled"),
                n("end_missing"),
                n("no_transcript"),
            );
        }
        if apply {
            println!("  written: {stamps} stamps, {ship_ends} ship ends");
        }
        for r in &refused {
            println!("  refused {r}");
        }
    }
    i32::from(!refused.is_empty())
}

type Counts = BTreeMap<&'static str, BTreeMap<&'static str, u64>>;

fn bump(counts: &mut Counts, phase: &str, key: &'static str) {
    let phase = ["think", "blueprint", "do", "review", "ship"]
        .into_iter()
        .find(|p| *p == phase)
        .unwrap_or("other");
    *counts.entry(phase).or_default().entry(key).or_default() += 1;
}

/// The fills each gapped row can take from its transcript, and the counts
/// per phase. `events_of(harness, session_id)` answers the transcript's
/// events and whether it is idle, or None when none is on disk.
fn plan(
    entries: &[Value],
    events_of: &mut dyn FnMut(&str, &str) -> Option<(Vec<(String, Option<&'static str>)>, bool)>,
) -> (Vec<(String, SessionFill)>, Counts) {
    let mut fills = Vec::new();
    let mut counts = Counts::new();
    for entry in entries {
        let Some(node) = entry_id(entry) else {
            continue;
        };
        let rows = entry.get("sessions").and_then(Value::as_array);
        for row in rows.into_iter().flatten() {
            let field = |key: &str| {
                row.get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            };
            let (Some(phase), Some(harness), Some(sid)) =
                (field("phase"), field("harness"), field("session_id"))
            else {
                continue;
            };
            let (started, ended) = (field("started_at"), field("ended_at"));
            // A ship row's end is its merge (phase_close); a do row's end is
            // its gated settle.
            let wants_end = ended.is_none() && !matches!(phase, "ship" | "do");
            if started.is_none() {
                bump(&mut counts, phase, "start_missing");
            }
            if ended.is_none() {
                bump(&mut counts, phase, "end_missing");
            }
            if started.is_some() && !wants_end {
                continue;
            }
            let Some((events, idle)) = events_of(harness, sid) else {
                bump(&mut counts, phase, "no_transcript");
                continue;
            };
            let Some(open) = events.iter().position(|(_, p)| *p == Some(phase)) else {
                continue;
            };
            let mut fill = SessionFill {
                phase: phase.to_string(),
                harness: harness.to_string(),
                session_id: sid.to_string(),
                started_at: None,
                ended_at: None,
                ended_by: "transcript".to_string(),
            };
            let start = events[open].0.as_str();
            if started.is_none() && ended.is_none_or(|end| start <= end) {
                fill.started_at = Some(start.to_string());
                bump(&mut counts, phase, "start_filled");
            }
            if wants_end {
                let next = events[open + 1..]
                    .iter()
                    .position(|(_, p)| p.is_some_and(|p| p != phase))
                    .map(|i| open + 1 + i);
                let end = match next {
                    Some(next) => Some(events[next - 1].0.as_str()),
                    None if idle => events.last().map(|(ts, _)| ts.as_str()),
                    None => None,
                };
                let from = started.unwrap_or(start);
                if let Some(end) = end.filter(|end| *end >= from) {
                    fill.ended_at = Some(end.to_string());
                    bump(&mut counts, phase, "end_filled");
                }
            }
            if fill.started_at.is_some() || fill.ended_at.is_some() {
                fills.push((node.to_string(), fill));
            }
        }
    }
    (fills, counts)
}

fn read_transcript(path: &Path, now: u64) -> Option<Transcript> {
    let file = std::fs::File::open(path).ok()?;
    let modified = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut events = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(ts) = event
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::phase_close::utc)
        else {
            continue;
        };
        events.push((ts, invoked_verb(&event).and_then(phase_of)));
    }
    Some(Transcript {
        events,
        idle: now.saturating_sub(modified) >= IDLE_SECS,
    })
}

/// The fno verb an event invokes: a typed command, a message that opens
/// with the verb, or a Skill tool call. Text that only mentions a verb
/// (injected context, a quoted launch line) invokes nothing.
fn invoked_verb(event: &Value) -> Option<&str> {
    let message = event.get("message");
    match event.get("type").and_then(Value::as_str)? {
        "user" => {
            let text = message?.get("content")?.as_str()?;
            text.split_once("<command-name>/fno:")
                .map(|(_, rest)| rest)
                .or_else(|| leading_verb(text))
                .map(verb_token)
        }
        "assistant" => message?
            .get("content")?
            .as_array()?
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter(|item| item.get("name").and_then(Value::as_str) == Some("Skill"))
            .find_map(|item| item.pointer("/input/skill")?.as_str()?.strip_prefix("fno:"))
            .map(verb_token),
        "response_item" => {
            let payload = event.get("payload")?;
            if payload.get("role").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let text = payload
                .get("content")?
                .as_array()?
                .iter()
                .find_map(|c| c.get("text").and_then(Value::as_str))?;
            leading_verb(text).map(verb_token)
        }
        _ => None,
    }
}

fn leading_verb(text: &str) -> Option<&str> {
    let text = text.trim_start();
    text.strip_prefix("/fno:")
        .or_else(|| text.strip_prefix("$fno:"))
}

fn verb_token(rest: &str) -> &str {
    let end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Codex rollout files by lowercased session id, one store walk.
fn codex_rollouts() -> HashMap<String, PathBuf> {
    let Some(root) = crate::codex_store::codex_home().map(|h| h.join("sessions")) else {
        return HashMap::new();
    };
    crate::daemon::index_tree(&root, 0)
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| name.starts_with("rollout-"))
        .filter_map(|(_, path)| {
            let stem = path.file_stem()?.to_str()?;
            Some((
                crate::provenance::rollout_session_id(stem).to_ascii_lowercase(),
                path,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: &str, phase: Option<&'static str>) -> (String, Option<&'static str>) {
        (ts.to_string(), phase)
    }

    #[test]
    fn invocations_are_typed_commands_leading_verbs_and_skill_calls() {
        let typed = json!({"type": "user", "message": {"content": "<command-message>fno:target</command-message>\n<command-name>/fno:target</command-name>"}});
        let codex = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "$fno:blueprint x-1"}]}});
        let skill = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "fno:think"}}]}});
        let mention = json!({"type": "user", "message": {"content": "launch: /fno:target x-1"}});
        let injected = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "<skills>\n$fno:review reviews</skills>"}]}});
        assert_eq!(invoked_verb(&typed), Some("target"));
        assert_eq!(invoked_verb(&codex), Some("blueprint"));
        assert_eq!(invoked_verb(&skill), Some("think"));
        assert_eq!(invoked_verb(&mention), None);
        assert_eq!(invoked_verb(&injected), None);
    }

    #[test]
    fn a_gapped_row_takes_its_start_and_end_from_the_transcript() {
        let entries = vec![json!({"id": "x-1", "sessions": [
            {"phase": "blueprint", "harness": "claude", "session_id": "s", "ended_at": "2026-09-01T02:00:00Z"},
            {"phase": "think", "harness": "claude", "session_id": "s"},
            {"phase": "do", "harness": "claude", "session_id": "s", "started_at": "2026-09-01T03:00:00Z"},
            {"phase": "review", "harness": "claude", "session_id": "gone"}
        ]})];
        let events = vec![
            ev("2026-09-01T00:00:00Z", None),
            ev("2026-09-01T00:10:00Z", Some("think")),
            ev("2026-09-01T00:50:00Z", None),
            ev("2026-09-01T01:00:00Z", Some("blueprint")),
            ev("2026-09-01T01:30:00Z", None),
            ev("2026-09-01T03:00:00Z", Some("do")),
            ev("2026-09-01T05:00:00Z", None),
        ];
        let (fills, counts) = plan(&entries, &mut |_, sid| {
            (sid == "s").then(|| (events.clone(), true))
        });
        let got: Vec<(&str, Option<&str>, Option<&str>)> = fills
            .iter()
            .map(|(_, f)| {
                (
                    f.phase.as_str(),
                    f.started_at.as_deref(),
                    f.ended_at.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                // The blueprint keeps its recorded end and gains its start.
                ("blueprint", Some("2026-09-01T01:00:00Z"), None),
                // Think ends at the last event before the blueprint opens.
                (
                    "think",
                    Some("2026-09-01T00:10:00Z"),
                    Some("2026-09-01T00:50:00Z")
                ),
            ]
        );
        assert_eq!(counts["review"]["no_transcript"], 1);
        assert_eq!(counts["do"]["end_missing"], 1);
        assert_eq!(
            counts["do"].get("end_filled"),
            None,
            "an open do row is never ended here"
        );
    }

    #[test]
    fn a_live_transcript_gives_a_start_but_no_end() {
        let entries = vec![json!({"id": "x-1", "sessions": [
            {"phase": "review", "harness": "codex", "session_id": "s"}
        ]})];
        let events = vec![
            ev("2026-09-01T00:00:00Z", Some("review")),
            ev("2026-09-01T00:05:00Z", None),
        ];
        let (fills, _) = plan(&entries, &mut |_, _| Some((events.clone(), false)));
        assert_eq!(
            fills[0].1.started_at.as_deref(),
            Some("2026-09-01T00:00:00Z")
        );
        assert_eq!(fills[0].1.ended_at, None);
    }

    #[test]
    fn apply_fills_gaps_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            json!({"entries": [{
                "id": "x-f", "slug": "f", "type": "feature", "title": "f",
                "status": "idea", "priority": "p2",
                "sessions": [{"phase": "think", "harness": "claude", "session_id": "s", "ended_at": "2026-09-01T09:00:00Z"}]
            }]})
            .to_string(),
        )
        .unwrap();
        let store = Store::new(&graph);
        let fill = SessionFill {
            phase: "think".into(),
            harness: "claude".into(),
            session_id: "s".into(),
            started_at: Some("2026-09-01T08:00:00Z".into()),
            ended_at: Some("2026-09-01T10:00:00Z".into()),
            ended_by: "transcript".into(),
        };
        let fills = [("x-f".to_string(), fill)];
        assert_eq!(api::session_backfill(&store, &fills).unwrap(), 1);
        let row = api::rows(&store).unwrap().remove(0);
        assert_eq!(
            row["sessions"][0]["started_at"],
            json!("2026-09-01T08:00:00Z")
        );
        assert_eq!(
            row["sessions"][0]["ended_at"],
            json!("2026-09-01T09:00:00Z")
        );
        assert_eq!(api::session_backfill(&store, &fills).unwrap(), 0, "idempotent");
    }
}
