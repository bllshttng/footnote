//! `fno backlog session backfill`: fill a session row's missing
//! `started_at` or `ended_at` from a recorded fact (user ruling 2026-09-24).
//! A dry run by default; `--apply` writes. It never overwrites a stamp, and
//! a row with no transcript on disk keeps its gap. It is its own verb, never
//! part of the schema-4 migration, because the migration invents no time.
//!
//! The start is the first invocation of the phase's verb in the session's
//! transcript whose arguments name the node (its id or its plan file): a
//! typed `/fno:<verb>` or `$fno:<verb>` that opens a user message, or a
//! Skill tool call naming `fno:<verb>`. A session that holds the phase for
//! one node only takes the verb's first invocation, named or not. A session
//! that holds it for several nodes stamps only a node its own call names;
//! the rest keep their gap and count as unmatched. The end is the last
//! event before a later call of another phase, or of the same phase for
//! another node, or the transcript's last event once it has been idle for a
//! day. A ship row ends at its merge commit (`phase_close`). A do row never
//! ends here: an open do row can be live work, and it ends through its
//! gated settle.

use std::collections::{BTreeMap, HashMap, HashSet};
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
        "target" | "execute" | "do" => Some("execute"),
        "review" => Some("review"),
        "pr" | "ship" => Some("ship"),
        _ => None,
    }
}

/// One transcript event: its UTC time and, when it invokes an fno verb, the
/// phase that verb opens and the call's argument text.
type Event = (String, Option<(&'static str, String)>);

/// A session's transcript events and whether it is idle; None when no
/// transcript is on disk.
type TranscriptRead = Option<(Vec<Event>, bool)>;

/// One transcript read: each timestamped event, with the call it carries.
struct Transcript {
    events: Vec<Event>,
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
        eprintln!("session-backfill: --graph is required");
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
                "  {phase:<9} start {}/{} filled   end {}/{} filled   no transcript {}   unmatched {}",
                n("start_filled"),
                n("start_missing"),
                n("end_filled"),
                n("end_missing"),
                n("no_transcript"),
                n("unmatched"),
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
    let phase = ["think", "blueprint", "execute", "review", "ship"]
        .into_iter()
        .find(|p| *p == phase)
        .unwrap_or("other");
    *counts.entry(phase).or_default().entry(key).or_default() += 1;
}

/// A row's non-empty text field.
fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn session_rows(entry: &Value) -> impl Iterator<Item = &Value> {
    entry
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Whether instant `a` is at or before `b`. A stored stamp may read `Z` or
/// `+00:00`, with or without fractions, so the two compare as instants; a
/// stamp that does not parse compares as text.
fn at_or_before(a: &str, b: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a <= b,
        _ => a <= b,
    }
}

/// Whether `args` names `name` as a whole token, so x-1 never matches x-12.
fn mentions(args: &str, name: &str) -> bool {
    let token = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    args.match_indices(name)
        .any(|(at, _)| !args[..at].ends_with(token) && !args[at + name.len()..].starts_with(token))
}

/// The fills each gapped row can take from its transcript, and the counts
/// per phase. `events_of(harness, session_id)` answers the transcript's
/// events and whether it is idle, or None when none is on disk.
fn plan(
    entries: &[Value],
    events_of: &mut dyn FnMut(&str, &str) -> TranscriptRead,
) -> (Vec<(String, SessionFill)>, Counts) {
    let mut fills = Vec::new();
    let mut counts = Counts::new();
    // The nodes each session holds a phase for: a session that worked
    // several nodes in one phase must name the node in the call.
    let mut holders: HashMap<(&str, &str), HashSet<&str>> = HashMap::new();
    for entry in entries {
        let Some(node) = entry_id(entry) else {
            continue;
        };
        for row in session_rows(entry) {
            if let (Some(phase), Some(sid)) = (text(row, "phase"), text(row, "session_id")) {
                holders.entry((phase, sid)).or_default().insert(node);
            }
        }
    }
    for entry in entries {
        let Some(node) = entry_id(entry) else {
            continue;
        };
        let plan_file = entry
            .get("plan_path")
            .and_then(Value::as_str)
            .and_then(|path| Path::new(path).file_name()?.to_str());
        let names =
            |args: &str| mentions(args, node) || plan_file.is_some_and(|f| mentions(args, f));
        for row in session_rows(entry) {
            let field = |key: &str| text(row, key);
            let (Some(phase), Some(harness), Some(sid)) =
                (field("phase"), field("harness"), field("session_id"))
            else {
                continue;
            };
            let shared = holders
                .get(&(phase, sid))
                .is_some_and(|nodes| nodes.len() > 1);
            let (started, ended) = (field("started_at"), field("ended_at"));
            // A ship row's end is its merge (phase_close); a do row's end is
            // its gated settle.
            let wants_end = ended.is_none() && !matches!(phase, "ship" | "execute");
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
            let calls = |event: &Event, named: bool| {
                event
                    .1
                    .as_ref()
                    .is_some_and(|(p, args)| *p == phase && (!named || names(args)))
            };
            let first = events.iter().position(|e| calls(e, false));
            let open = match (events.iter().position(|e| calls(e, true)), first) {
                (Some(open), _) => open,
                (None, Some(first)) if !shared => first,
                (None, first) => {
                    if first.is_some() {
                        bump(&mut counts, phase, "unmatched");
                    }
                    continue;
                }
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
            if started.is_none() && ended.is_none_or(|end| at_or_before(start, end)) {
                fill.started_at = Some(start.to_string());
                bump(&mut counts, phase, "start_filled");
            }
            if wants_end {
                let next = events[open + 1..]
                    .iter()
                    .position(|(_, call)| {
                        call.as_ref()
                            .is_some_and(|(p, args)| *p != phase || (shared && !names(args)))
                    })
                    .map(|i| open + 1 + i);
                let end = match next {
                    Some(next) => Some(events[next - 1].0.as_str()),
                    None if idle => events.last().map(|(ts, _)| ts.as_str()),
                    None => None,
                };
                let from = started.unwrap_or(start);
                if let Some(end) = end.filter(|end| at_or_before(from, end)) {
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
        let call = invocation(&event).and_then(|(verb, args)| Some((phase_of(verb)?, args)));
        events.push((ts, call));
    }
    Some(Transcript {
        events,
        idle: now.saturating_sub(modified) >= IDLE_SECS,
    })
}

/// The fno verb an event invokes and the call's argument text: a typed
/// command, a message that opens with the verb, or a Skill tool call. Text
/// that only mentions a verb (injected context, a quoted launch line)
/// invokes nothing.
fn invocation(event: &Value) -> Option<(&str, String)> {
    let message = event.get("message");
    match event.get("type").and_then(Value::as_str)? {
        "user" => {
            let text = message?.get("content")?.as_str()?;
            if let Some((_, rest)) = text.split_once("<command-name>/fno:") {
                let args = text
                    .split_once("<command-args>")
                    .and_then(|(_, tail)| tail.split_once("</command-args>"))
                    .map_or("", |(args, _)| args);
                return Some((verb_token(rest), args.to_string()));
            }
            leading_verb(text).map(split_call)
        }
        "assistant" => message?
            .get("content")?
            .as_array()?
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter(|item| item.get("name").and_then(Value::as_str) == Some("Skill"))
            .find_map(|item| {
                let verb = item
                    .pointer("/input/skill")?
                    .as_str()?
                    .strip_prefix("fno:")?;
                let args = item.pointer("/input/args").and_then(Value::as_str);
                Some((verb_token(verb), args.unwrap_or_default().to_string()))
            }),
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
            leading_verb(text).map(split_call)
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

/// A leading call split into its verb and the text after it.
fn split_call(rest: &str) -> (&str, String) {
    let verb = verb_token(rest);
    (verb, rest[verb.len()..].to_string())
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

    fn ev(ts: &str, phase: Option<&'static str>) -> Event {
        (ts.to_string(), phase.map(|p| (p, String::new())))
    }

    fn call(ts: &str, phase: &'static str, args: &str) -> Event {
        (ts.to_string(), Some((phase, args.to_string())))
    }

    #[test]
    fn invocations_are_typed_commands_leading_verbs_and_skill_calls() {
        let typed = json!({"type": "user", "message": {"content": "<command-message>fno:target</command-message>\n<command-name>/fno:target</command-name>\n<command-args>x-7</command-args>"}});
        let codex = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "$fno:blueprint x-1"}]}});
        let skill = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "fno:think", "args": "x-2"}}]}});
        let mention = json!({"type": "user", "message": {"content": "launch: /fno:target x-1"}});
        let injected = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "<skills>\n$fno:review reviews</skills>"}]}});
        assert_eq!(invocation(&typed), Some(("target", "x-7".to_string())));
        assert_eq!(invocation(&codex), Some(("blueprint", " x-1".to_string())));
        assert_eq!(invocation(&skill), Some(("think", "x-2".to_string())));
        assert_eq!(invocation(&mention), None);
        assert_eq!(invocation(&injected), None);
    }

    #[test]
    fn a_session_that_worked_several_nodes_stamps_each_by_its_own_call() {
        let row = json!([{"phase": "blueprint", "harness": "claude", "session_id": "k"}]);
        let entries = vec![
            json!({"id": "x-1", "sessions": row}),
            json!({"id": "x-2", "plan_path": "plans/two.md", "sessions": row}),
            json!({"id": "x-3", "sessions": row}),
        ];
        let events = vec![
            call("2026-09-01T00:00:00Z", "blueprint", "x-1"),
            ev("2026-09-01T00:30:00Z", None),
            call("2026-09-01T01:00:00Z", "blueprint", "plans/two.md"),
            ev("2026-09-01T01:30:00Z", None),
            call("2026-09-01T02:00:00Z", "blueprint", "x-12"),
            ev("2026-09-01T02:30:00Z", None),
        ];
        let (fills, counts) = plan(&entries, &mut |_, _| Some((events.clone(), true)));
        let got: Vec<(&str, Option<&str>, Option<&str>)> = fills
            .iter()
            .map(|(node, f)| {
                (
                    node.as_str(),
                    f.started_at.as_deref(),
                    f.ended_at.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "x-1",
                    Some("2026-09-01T00:00:00Z"),
                    Some("2026-09-01T00:30:00Z")
                ),
                (
                    "x-2",
                    Some("2026-09-01T01:00:00Z"),
                    Some("2026-09-01T01:30:00Z")
                ),
            ]
        );
        // The x-12 call names another node, so x-3 has no call of its own.
        assert_eq!(counts["blueprint"]["unmatched"], 1);
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
    fn stamps_in_two_spellings_compare_as_instants() {
        assert!(at_or_before(
            "2026-09-01T01:00:05Z",
            "2026-09-01T01:00:05.300+00:00"
        ));
        assert!(!at_or_before(
            "2026-09-01T01:00:05.700Z",
            "2026-09-01T01:00:05.300+00:00"
        ));
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
        crate::graph_store::seed_rows(
            &graph,
            &[json!({
                "id": "x-f", "slug": "f", "type": "feature", "title": "f",
                "status": "idea", "priority": "p2",
                "sessions": [{"phase": "think", "harness": "claude", "session_id": "s", "ended_at": "2026-09-01T09:00:00Z"}]
            })],
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
        assert_eq!(
            api::session_backfill(&store, &fills).unwrap(),
            0,
            "idempotent"
        );
    }
}
