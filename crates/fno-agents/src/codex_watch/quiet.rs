//! Read-confirmed recovery for unfinished workers. Runs under the settle arm's
//! existing cadence and one-in-flight gate; a quiet transcript is not death.

use crate::paths::AgentsHome;
use crate::state::RegistryEntry;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SENDER: &str = "fno/quiet-recovery";
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Episode {
    activity: i64,
    read_stage: u8,
    pending: Option<(String, i64, u8)>,
    escalated: bool,
    #[serde(default)]
    recovered: bool,
    #[serde(default)]
    recovery_reason: Option<String>,
}

#[derive(Default)]
struct Tail {
    activity: i64,
    started_at: Option<i64>,
    failed: bool,
    read: bool,
}

fn tail(path: &Path, marker: Option<&str>) -> Option<Tail> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Tail::default();
    for line in text.lines().skip(usize::from(size > TAIL_BYTES)) {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if marker.is_some_and(|marker| line.contains(marker)) {
            out.read = true;
            continue;
        }
        let payload = row.get("payload").unwrap_or(&row);
        let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
        let event = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let timestamp = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp());
        if event == "task_started" {
            out.failed = false;
            out.started_at = out.started_at.or(timestamp);
        }
        if event == "task_complete" {
            out.failed = payload.get("error").is_some_and(|e| !e.is_null());
        }
        if matches!(kind, "response_item" | "assistant" | "user" | "tool")
            || matches!(event, "item_completed")
        {
            if let Some(at) = timestamp {
                out.activity = out.activity.max(at);
            }
        }
    }
    (out.activity > 0 || out.started_at.is_some() || out.read || out.failed).then_some(out)
}

#[derive(Debug, PartialEq)]
enum Action {
    Wait,
    Nudge(u8),
    Recover,
    Help,
}

fn action(episode: &mut Episode, tail: &Tail, now: i64, live: bool, dead: bool) -> Action {
    if episode.activity == 0 {
        episode.activity = tail.started_at.unwrap_or_default();
    }
    if tail.activity > episode.activity {
        *episode = Episode {
            activity: tail.activity,
            ..Default::default()
        };
    }
    if episode.escalated {
        return Action::Wait;
    }
    if dead {
        return if episode.recovered {
            Action::Help
        } else {
            Action::Recover
        };
    }
    if let Some((_, sent, stage)) = episode.pending.as_ref() {
        if tail.read {
            episode.read_stage = *stage;
            episode.pending = None;
        } else if now.saturating_sub(*sent) >= 600 {
            return if episode.recovered {
                Action::Help
            } else {
                Action::Recover
            };
        } else {
            return Action::Wait;
        }
    }
    if !live {
        return Action::Wait;
    }
    if episode.activity == 0 && !tail.failed {
        return Action::Wait;
    }
    if tail.failed && episode.read_stage == 0 {
        return Action::Nudge(1);
    }
    let quiet = now.saturating_sub(episode.activity);
    if quiet >= 2700 && episode.read_stage >= 2 {
        Action::Help
    } else if quiet >= 1800 && episode.read_stage == 1 {
        Action::Nudge(2)
    } else if quiet >= 900 && episode.read_stage == 0 {
        Action::Nudge(1)
    } else {
        Action::Wait
    }
}

fn command(args: &[&str], cwd: &str) -> (i32, String, String) {
    crate::burn_watch::run_command(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), cwd)
}

fn help(entry: &RegistryEntry, scope: &str, node: &str, reason: &str) -> Result<(), String> {
    let text = format!("control: <help class=\"stuck\" reason=\"unfinished-worker\">{node}: {} in {}: {reason}. Review the unfinished work and take ownership of this node.</help>", entry.name, entry.cwd);
    let (code, out, error) = command(
        &[
            "fno",
            "agents",
            "mail",
            "send",
            "--from-name",
            SENDER,
            "--origin",
            "scheduler",
            "--to-lead",
            scope,
            &text,
        ],
        &entry.cwd,
    );
    if crate::mail_inject::mail_send_accepted(code, &out) {
        Ok(())
    } else {
        Err(format!("lead delivery refused: {out} {error}"))
    }
}

fn send_recovery(entry: &RegistryEntry, sid: &str, text: &str) -> Result<(), String> {
    let (code, out, error) = command(
        &[
            "fno",
            "agents",
            "mail",
            "send",
            "--from-name",
            SENDER,
            "--origin",
            "scheduler",
            sid,
            text,
        ],
        &entry.cwd,
    );
    if crate::mail_inject::mail_send_accepted(code, &out)
        || crate::mail_inject::mail_send_receipt(&out).contains("durable")
    {
        Ok(())
    } else {
        Err(format!("recovery message refused: {out} {error}"))
    }
}

fn restore(entry: &RegistryEntry, sid: &str, text: &str) -> Result<(), String> {
    let receipt =
        crate::receipt::build_reap_receipt(entry, None, crate::receipt::Writer::RegistryWrite)?;
    if !Path::new(&receipt.cwd).is_dir() {
        return Err(format!(
            "recorded worktree is gone; resume: {}",
            receipt.resume
        ));
    }
    if let Some(mux) = entry.mux.as_ref() {
        let (code, out, error) = command(
            &[
                "fno",
                "mux",
                "workspace",
                "restore",
                "--session",
                &mux.session,
                "--member-session",
                sid,
                "--json",
            ],
            &receipt.cwd,
        );
        let answer: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
        if code == 0
            && answer
                .get("members")
                .and_then(Value::as_array)
                .is_some_and(|members| {
                    members.iter().any(|row| {
                        matches!(
                            row.get("outcome").and_then(Value::as_str),
                            Some("resumed" | "focused")
                        )
                    })
                })
        {
            return send_recovery(entry, sid, text);
        }
        return Err(format!(
            "workspace restore refused: {out} {error}; resume: {}",
            receipt.resume
        ));
    }
    let (code, out, error) = command(
        &["fno", "agents", "resume", sid, "--message", text],
        &receipt.cwd,
    );
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "thread resume refused: {out} {error}; resume: {}",
            receipt.resume
        ))
    }
}

fn salvage(entry: &RegistryEntry, sid: &str, now: i64) -> Result<String, String> {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let index_path = temp.path().join("index");
    let run = |args: &[&str]| -> Result<String, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(&entry.cwd)
            .env("GIT_INDEX_FILE", &index_path)
            .args(args);
        let out =
            crate::bounded_cmd::output_with_timeout_result(cmd, 30).map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    };
    let head = run(&["rev-parse", "HEAD"])?.trim().to_string();
    run(&["read-tree", "HEAD"])?;
    run(&["add", "--update"])?;
    let untracked = run(&["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut add = vec!["add", "--"];
    add.extend(untracked.split('\0').filter(|name| !name.is_empty()));
    if add.len() > 2 {
        run(&add)?;
    }
    let tree = run(&["write-tree"])?.trim().to_string();
    let commit = run(&[
        "commit-tree",
        &tree,
        "-p",
        &head,
        "-m",
        "chore: preserve unfinished worker changes",
    ])?
    .trim()
    .to_string();
    let branch = format!("refs/heads/recovery/{sid}-{now}");
    run(&[
        "update-ref",
        &branch,
        &commit,
        "0000000000000000000000000000000000000000",
    ])?;
    Ok(format!(
        "salvaged {branch} at {commit}; original index and worktree preserved"
    ))
}

pub(super) fn run(home: &AgentsHome) -> Result<(), String> {
    let registry = crate::state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let rows = crate::graph_store::read_rows_where(
        &crate::gc_sweep::graph_path(home),
        &crate::backlog::RowQuery {
            fields: Some(
                [
                    "id",
                    "status",
                    "merge_status",
                    "sessions",
                    "parent",
                    "project",
                    "type",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let watches = crate::watch_expiry::read_evidence(home, now * 1000)?;
    let stopped: std::collections::HashSet<_> = watches
        .iter()
        .filter(|e| e.kind == "agent_stopped")
        .filter_map(|e| e.session_id.as_deref())
        .collect();
    let mut stores = crate::gc_inventory::HarnessStoreIndex::default();
    let mut loaded = None;
    let mut codex_daemon_dead = None;
    let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
    let dir = home.root().join("quiet-recovery");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let settle_watches = super::settle_watches(&watches);
    let mut owners = None;
    let mut acted = 0;
    for entry in &registry.entries {
        let result = (|| -> Result<(), String> {
            let Some(sid) = entry
                .harness_session_id
                .as_deref()
                .filter(|s| crate::resume_wake::is_uuid_shaped(s))
            else {
                return Ok(());
            };
            let Some(node) = entry.node.as_deref() else {
                return Ok(());
            };
            let Some(row) = rows
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(node))
            else {
                return Ok(());
            };
            if matches!(
                row.get("status").and_then(Value::as_str),
                Some("done" | "superseded" | "deferred")
            ) || row.get("merge_status").and_then(Value::as_str) == Some("merged")
            {
                return Ok(());
            }
            if settle_watches.iter().any(|w| {
                (w.watch.session_id == sid || w.codex_thread_id.as_deref() == Some(sid))
                    && now * 1000 < w.watch.expires_at_ms
                    && crate::watch_expiry::is_current_watch(&w.watch, &watches)
            }) {
                return Ok(());
            }
            let (claim_state, claim) = crate::claims::status(&format!("node:{node}"), None);
            let own = claim.as_ref().is_some_and(|c| {
                c.session_id.as_deref() == Some(sid) || c.holder == format!("target-session:{sid}")
            });
            if matches!(
                claim_state,
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
            ) && !own
            {
                return Ok(());
            }
            if claim_state == crate::claims::ClaimState::Corrupted {
                return Ok(());
            }
            let state_path = dir.join(format!("{sid}.json"));
            let mut episode: Episode = match std::fs::read(&state_path) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Episode::default(),
                Err(e) => return Err(e.to_string()),
            };
            let path = entry
                .transcript_path
                .as_deref()
                .filter(|p| Path::new(p).is_file())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    stores
                        .matches(entry)?
                        .into_iter()
                        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
                });
            let native_codex_thread = entry.harness_name() == "codex" && entry.mux.is_none();
            let daemon_dead = native_codex_thread
                && *codex_daemon_dead.get_or_insert_with(|| {
                    crate::codex_inject::CodexDaemonAdapter::from_environment()
                        .provider_pid_start()
                        .is_some_and(|(pid, start)| !crate::daemon::pid_is_ours(pid, start))
                });
            let thread_unloaded = native_codex_thread
                && !daemon_dead
                && loaded
                    .get_or_insert_with(crate::codex_inject::loaded_thread_ids)
                    .as_ref()
                    .is_ok_and(|roster| !roster.contains(sid));
            let dead = matches!(
                entry.status,
                crate::AgentStatus::Failed
                    | crate::AgentStatus::Exited
                    | crate::AgentStatus::PermanentDead
            ) || stopped.contains(sid)
                || entry.pid.is_some_and(|pid| crate::daemon::pid_is_gone(pid))
                || daemon_dead
                || thread_unloaded;
            let transcript_tail = match path.as_deref() {
                Some(path) => tail(path, episode.pending.as_ref().map(|p| p.0.as_str())),
                None if dead => Some(Tail::default()),
                None => None,
            };
            let Some(tail) = transcript_tail else {
                return Ok(());
            };
            let read_receipt = episode
                .pending
                .as_ref()
                .filter(|_| tail.read)
                .map(|(marker, _, stage)| (marker.clone(), *stage));
            let decision = action(
                &mut episode,
                &tail,
                now,
                own && claim_state == crate::claims::ClaimState::Live,
                dead,
            );
            if decision != Action::Wait {
                acted += 1;
            }
            let mut handoff = None;
            match decision {
                Action::Wait => {}
                Action::Nudge(stage) => {
                    let marker = format!("quiet-recovery:{sid}:{}:{stage}", episode.activity);
                    let receipt = crate::receipt::build_reap_receipt(
                        entry,
                        None,
                        crate::receipt::Writer::RegistryWrite,
                    )?;
                    let text = if stage == 1 {
                        if tail.failed {
                            format!("control: previous turn failed on {node}. Continue the assigned work or emit <help>. [{marker}]")
                        } else {
                            format!("control: quiet 15 min on {node}, what blocks you? continue or emit <help>. [{marker}]")
                        }
                    } else {
                        format!("control: quiet 30 min on {node}. Continue or emit <help>. Exact harness resume: {} [{marker}]", receipt.resume)
                    };
                    // A transport ack is only handed. The next transcript read
                    // confirms read before this attempt can advance the ladder.
                    episode.pending = Some((marker, now, stage));
                    let (code, out, error) = command(
                        &[
                            "fno",
                            "agents",
                            "mail",
                            "send",
                            "--from-name",
                            SENDER,
                            "--origin",
                            "scheduler",
                            sid,
                            &text,
                        ],
                        &entry.cwd,
                    );
                    let delivery = if crate::mail_inject::mail_send_accepted(code, &out) {
                        "handed"
                    } else if crate::mail_inject::mail_send_receipt(&out).contains("durable") {
                        "queued"
                    } else {
                        "failed"
                    };
                    emitter
                        .emit(
                            "quiet_worker_nudge",
                            &json!({"session_id": sid, "node": node, "stage": stage,
                    "marker": marker, "delivery": delivery, "exit": code, "receipt": out, "error": error}),
                        )
                        .map_err(|e| e.to_string())?;
                }
                Action::Recover => {
                    let marker = format!("quiet-recovery:{sid}:{}:resume", episode.activity);
                    let text = format!("control: The previous worker stopped with unfinished work. Continue the assigned work in this same worktree from the last completed step, preserving all changes. [{marker}]");
                    let result = restore(entry, sid, &text);
                    episode.recovered = true;
                    let reason = match &result {
                        Ok(()) => format!("resume sent; transcript read pending [{marker}]"),
                        Err(error) => format!(
                            "{error}; {}",
                            salvage(entry, sid, now).unwrap_or_else(|e| format!(
                                "salvage refused: {e}; preserve worktree {}",
                                entry.cwd
                            ))
                        ),
                    };
                    emitter
                        .emit(
                            "quiet_worker_recovery",
                            &json!({"session_id": sid, "node": node, "reason": reason}),
                        )
                        .map_err(|e| e.to_string())?;
                    if result.is_err() {
                        episode.recovery_reason = Some(reason.clone());
                        handoff = Some(reason);
                    } else {
                        episode.recovery_reason = Some(reason.clone());
                        episode.pending = Some((marker, now, 3));
                    }
                }
                Action::Help => {
                    handoff = Some(episode.recovery_reason.clone().unwrap_or_else(|| {
                        "worker stayed quiet after read-confirmed nudges".to_string()
                    }));
                }
            }
            if let Some((marker, stage)) = read_receipt {
                if stage == 3 {
                    emitter
                        .emit(
                            "quiet_worker_recovery",
                            &json!({"session_id": sid, "node": node,
                        "reason": format!("recipient transcript confirms recovery read [{marker}]"),
                        "delivery": "read"}),
                        )
                        .map_err(|e| e.to_string())?;
                } else {
                    emitter
                        .emit(
                            "quiet_worker_nudge",
                            &json!({"session_id": sid, "node": node,
                        "stage": stage, "marker": marker, "delivery": "read"}),
                        )
                        .map_err(|e| e.to_string())?;
                }
            }
            let bytes = serde_json::to_vec(&episode).map_err(|e| e.to_string())?;
            let tmp = state_path.with_extension("tmp");
            std::fs::write(&tmp, bytes)
                .and_then(|_| std::fs::rename(tmp, &state_path))
                .map_err(|e| e.to_string())?;
            if let Some(reason) = handoff {
                let scope = crate::burn_watch::node_owner(
                    Path::new(&entry.cwd),
                    &home.registry_json(),
                    &rows,
                    &mut owners,
                    node,
                )
                .ok_or_else(|| format!("no live team owner resolved for node {node}"))?;
                help(entry, &scope, node, &reason)?;
                episode.escalated = true;
                let bytes = serde_json::to_vec(&episode).map_err(|e| e.to_string())?;
                let tmp = dir.join(format!("{sid}.tmp"));
                std::fs::write(&tmp, bytes)
                    .and_then(|_| std::fs::rename(tmp, &state_path))
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            emitter
                .emit(
                    "quiet_worker_error",
                    &json!({"name": entry.name, "reason": error}),
                )
                .map_err(|e| e.to_string())?;
        }
        if acted >= 4 {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_requires_reads_resets_on_items_and_recovers_unfinished_death() {
        let mut e = Episode {
            activity: 100,
            ..Default::default()
        };
        let mut t = Tail {
            activity: 100,
            ..Default::default()
        };
        assert_eq!(action(&mut e, &t, 1000, true, false), Action::Nudge(1));
        e.pending = Some(("marker".into(), 1000, 1));
        assert_eq!(action(&mut e, &t, 1500, true, false), Action::Wait);
        assert_eq!(action(&mut e, &t, 1600, true, false), Action::Recover);
        t.read = true;
        assert_eq!(action(&mut e, &t, 1900, true, false), Action::Nudge(2));
        e.pending = Some(("second".into(), 1900, 2));
        assert_eq!(action(&mut e, &t, 2800, true, false), Action::Help);
        e.pending = Some(("unread".into(), 2700, 2));
        t.read = false;
        assert_eq!(action(&mut e, &t, 2801, false, true), Action::Recover);
        e.pending = None;
        t.activity = 2800;
        assert_eq!(action(&mut e, &t, 2801, true, false), Action::Wait);
        assert_eq!(e.read_stage, 0);
        assert_eq!(action(&mut e, &t, 2801, false, true), Action::Recover);
        assert_eq!(action(&mut e, &t, 5000, false, false), Action::Wait);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(&path, concat!(
            "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}\n",
            "{\"timestamp\":\"2026-01-01T00:15:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"timestamp\":\"2026-01-01T00:15:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"text\":\"marker\"}}\n"
        )).unwrap();
        let evidence = tail(&path, Some("marker")).unwrap();
        assert_eq!(evidence.activity, 1767225600);
        assert!(
            evidence.read,
            "receipt bytes in the recipient transcript confirm the nudge"
        );
    }

    #[test]
    fn salvage_preserves_untracked_work_and_the_original_branch_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        assert_eq!(command(&["git", "init", "-q"], cwd).0, 0);
        assert_eq!(
            command(&["git", "config", "user.name", "Fixture"], cwd).0,
            0
        );
        assert_eq!(
            command(
                &["git", "config", "user.email", "fixture@example.test"],
                cwd
            )
            .0,
            0
        );
        std::fs::write(dir.path().join("tracked"), "base").unwrap();
        assert_eq!(command(&["git", "add", "tracked"], cwd).0, 0);
        assert_eq!(command(&["git", "commit", "-qm", "base"], cwd).0, 0);
        let head = command(&["git", "rev-parse", "HEAD"], cwd).1;
        std::fs::write(dir.path().join("tracked"), "staged").unwrap();
        assert_eq!(command(&["git", "add", "tracked"], cwd).0, 0);
        std::fs::write(dir.path().join("tracked"), "working").unwrap();
        std::fs::write(dir.path().join(" new file"), "untracked").unwrap();
        let entry = RegistryEntry {
            cwd: cwd.into(),
            ..Default::default()
        };
        let sid = "00000000-0000-4000-8000-000000000001";
        let receipt = salvage(&entry, sid, 1).unwrap();
        let branch = format!("refs/heads/recovery/{sid}-1");
        assert!(receipt.contains(&branch));
        assert_eq!(
            command(&["git", "show", &format!("{branch}:tracked")], cwd).1,
            "working"
        );
        assert_eq!(
            command(&["git", "show", &format!("{branch}: new file")], cwd).1,
            "untracked"
        );
        assert_eq!(command(&["git", "show", ":tracked"], cwd).1, "staged");
        assert_eq!(command(&["git", "rev-parse", "HEAD"], cwd).1, head);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked")).unwrap(),
            "working"
        );
    }
}
