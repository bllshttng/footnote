//! Spawn receipts arm one durable first check and one retry, across daemon restarts.

use crate::claims::{self, AcquireOpts, ClaimRecord};
use crate::paths::AgentsHome;
use crate::state::{self, RegistryEntry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
struct Check {
    name: String,
    parent: String,
    created_at: String,
    fno_id: Option<String>,
    born_at: i64,
    deadline: i64,
    interval: i64,
    head: Option<String>,
    tool_calls: u64,
    transcript_offset: u64,
    notices: u8,
    finished: bool,
}

fn interval(cwd: &str) -> i64 {
    crate::agents_config::config_lookup(Path::new(cwd), &["agents", "first_check_minutes"])
        .and_then(|v| v.as_integer())
        .filter(|v| (1..=60).contains(v))
        .unwrap_or(10)
        * 60_000
}

fn head(cwd: &str) -> Option<String> {
    let root = Path::new(cwd)
        .ancestors()
        .find(|p| p.join(".git").exists())?;
    let git = root.join(".git");
    let git = if git.is_dir() {
        git
    } else {
        root.join(
            std::fs::read_to_string(&git)
                .ok()?
                .trim()
                .strip_prefix("gitdir: ")?,
        )
    };
    let raw = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let raw = raw.trim();
    let oid = if let Some(reference) = raw.strip_prefix("ref: ") {
        let common = std::fs::read_to_string(git.join("commondir"))
            .ok()
            .map_or_else(|| git.clone(), |p| git.join(p.trim()));
        std::fs::read_to_string(git.join(reference))
            .or_else(|_| std::fs::read_to_string(common.join(reference)))
            .ok()
            .or_else(|| {
                std::fs::read_to_string(common.join("packed-refs"))
                    .ok()?
                    .lines()
                    .find_map(|line| {
                        let (oid, name) = line.split_once(' ')?;
                        (name == reference).then(|| oid.to_string())
                    })
            })?
    } else {
        raw.to_string()
    };
    let oid = oid.trim();
    (oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit())).then(|| oid.to_string())
}

pub(crate) fn record_birth(
    journal: &Path,
    event_id: &str,
    data: &Value,
    born_at: i64,
) -> Result<(), String> {
    let Some(root) = journal.parent() else {
        return Ok(());
    };
    let direct = AgentsHome::at(root);
    let home = if direct.registry_json().exists() {
        direct
    } else {
        AgentsHome::at(root.join("agents"))
    };
    if !home.registry_json().exists() {
        return Ok(());
    }
    let Some(name) = data.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(parent) = data
        .get("spawned_by_session")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    else {
        return Ok(());
    };
    // Only a persisted birth row can move a claim. Event replay cannot mint a worker.
    let registry = state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let Some(row) = registry
        .entries
        .iter()
        .find(|r| r.name == name && r.spawned_by_session.as_deref() == Some(parent))
    else {
        return Ok(());
    };
    if let Some(sid) = data.get("harness_session_id").and_then(Value::as_str) {
        if row.harness_session_id.as_deref() != Some(sid) {
            return Ok(());
        }
    }
    let dir = home.root().join("first-checks");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    use sha2::{Digest, Sha256};
    let key = format!("{:x}", Sha256::digest(event_id.as_bytes()));
    let path = dir.join(format!("{key}.json"));
    if !path.exists() {
        let interval = interval(&row.cwd);
        let check = Check {
            name: name.into(),
            parent: parent.into(),
            created_at: row.created_at.clone(),
            fno_id: row.fno_id.clone(),
            born_at,
            deadline: born_at.saturating_add(interval),
            interval,
            head: head(&row.cwd),
            tool_calls: row.tool_calls.unwrap_or(0),
            transcript_offset: transcript(row)
                .and_then(|p| std::fs::metadata(p).ok())
                .map_or(0, |m| m.len()),
            notices: 0,
            finished: false,
        };
        state::write_json_atomic(&path, &check).map_err(|e| e.to_string())?;
    }
    transfer_birth_claim(row, born_at)?;
    Ok(())
}

fn transfer_birth_claim(row: &RegistryEntry, born_at: i64) -> Result<(), String> {
    let (Some(node), Some(parent)) = (&row.node, &row.spawned_by_session) else {
        return Ok(());
    };
    let key = format!("node:{node}");
    let path = claims::claim_path(&key, None)?;
    if !path.exists() {
        return Ok(());
    }
    let sid = row.harness_session_id.clone();
    let holder = sid.as_ref().map_or_else(
        || format!("spawn-handover:{}", row.name),
        |sid| format!("target-session:{sid}"),
    );
    let opts = AcquireOpts {
        metadata: Some(serde_json::Map::from_iter([(
            "worktree".into(),
            row.cwd.clone().into(),
        )])),
        pid: row.pid,
        pid_unavailable: sid.is_none()
            || row.pid.is_none()
            || !claims::pid_dies_with_session(row.harness.as_deref()),
        ttl_ms: Some(2 * 60 * 60_000),
        identity: sid.map(|sid| (sid, row.harness_name().to_string())),
        reason: Some("spawn claim transfer".into()),
        ..Default::default()
    };
    transfer(&path, parent, &holder, &opts, Some(born_at)).map(|_| ())
}

fn transfer(
    path: &Path,
    parent: &str,
    holder: &str,
    opts: &AcquireOpts,
    born_at: Option<i64>,
) -> Result<Option<ClaimRecord>, String> {
    claims::with_recovery_lock(path, || {
        let mut claim = match claims::read_claim_file(path) {
            Ok(claim) => claim,
            Err(claims::ReadError::GoneAway) => return Ok(None),
            Err(claims::ReadError::Corrupted(e)) => return Err(e),
        };
        if claim.session_id.as_deref() != Some(parent) || claim.holder == holder {
            return Ok(None);
        }
        let now = claims::now_ms();
        claim
            .metadata
            .insert("dispatched_by_session".into(), parent.into());
        if let Some(metadata) = &opts.metadata {
            claim.metadata.extend(metadata.clone());
        }
        claim.holder = holder.into();
        claim.acquired_at = born_at.unwrap_or(now);
        claim.expires_at = Some(now.saturating_add(opts.ttl_ms.unwrap_or(2 * 60 * 60_000)));
        claim.pid = if opts.pid_unavailable {
            None
        } else {
            Some(opts.pid.unwrap_or_else(std::process::id) as i32)
        };
        claim.pid_unavailable = opts.pid_unavailable;
        claim.pid_provenance = Some("ambient".into());
        claim.schema_version = if claim.pid_unavailable {
            claims::PID_UNAVAILABLE_SCHEMA_VERSION
        } else {
            claims::SCHEMA_VERSION
        };
        claim.session_id = opts.identity.as_ref().map(|(sid, _)| sid.clone());
        claim.harness = opts.identity.as_ref().map(|(_, harness)| harness.clone());
        claim.reason = opts.reason.clone();
        claims::atomic_replace(path, &claims::serialize_claim(&claim)?)?;
        Ok(Some(claim))
    })
}

pub(crate) fn take_parent_claim(
    key: &str,
    holder: &str,
    opts: &AcquireOpts,
    existing: &ClaimRecord,
) -> Result<Option<ClaimRecord>, String> {
    let Some(node) = key.strip_prefix("node:") else {
        return Ok(None);
    };
    let (sid, harness) = opts
        .identity
        .clone()
        .map(|(s, h)| (Some(s), Some(h)))
        .unwrap_or_else(claims::resolve_identity);
    let (Some(sid), Some(harness), Some(parent)) = (sid, harness, existing.session_id.as_ref())
    else {
        return Ok(None);
    };
    if holder
        .strip_prefix("spawn-handover:")
        .is_some_and(|name| !name.is_empty())
        && lead_can_delegate(existing, Some(&sid), Some(&harness))
    {
        let mut parent_opts = opts.clone();
        parent_opts.identity = Some((sid, harness));
        return transfer(
            &claims::claim_path(key, opts.root.as_deref())?,
            parent,
            holder,
            &parent_opts,
            None,
        );
    }
    if holder != format!("target-session:{sid}") {
        return Ok(None);
    }
    let Some(home) = AgentsHome::from_env_opt() else {
        return Ok(None);
    };
    let registry = state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let Some(row) = registry.find_by_session(&harness, &sid) else {
        return Ok(None);
    };
    if row.node.as_deref() != Some(node)
        || row.spawned_by_session.as_ref() != Some(parent)
        || (!row.status.is_drive_eligible() && row.status != crate::AgentStatus::Spawning)
    {
        return Ok(None);
    }
    let mut child_opts = opts.clone();
    child_opts.identity = Some((sid, harness));
    transfer(
        &claims::claim_path(key, opts.root.as_deref())?,
        parent,
        holder,
        &child_opts,
        None,
    )
}

pub(crate) fn lead_can_delegate(
    record: &ClaimRecord,
    session: Option<&str>,
    harness: Option<&str>,
) -> bool {
    let (Some(session), Some(harness)) = (session, harness) else {
        return false;
    };
    if !record.key.starts_with("node:")
        || record.session_id.as_deref() != Some(session)
        || record.holder.starts_with("spawn-handover:")
    {
        return false;
    }
    let Some(home) = AgentsHome::from_env_opt() else {
        return false;
    };
    let Ok(registry) = state::load_registry(&home.registry_json()) else {
        return false;
    };
    registry
        .find_by_session(harness, session)
        .is_some_and(|row| row.crown_level.is_some() && row.status.is_drive_eligible())
}

pub(crate) fn run_pass(
    home: &AgentsHome,
    now: i64,
    runner: crate::burn_watch::Runner<'_>,
) -> Result<(), String> {
    let mut runner = runner;
    let dir = home.root().join("first-checks");
    if !dir.exists() {
        return Ok(());
    }
    let registry = state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let mut first_error = None;
    for item in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let path = item.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let result = (|| -> Result<(), String> {
            let mut check: Check =
                serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if check.finished {
                if now.saturating_sub(check.born_at) > 86_400_000 {
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                }
                return Ok(());
            }
            if now < check.deadline {
                return Ok(());
            }
            let row = registry.entries.iter().find(|r| {
                (r.name == check.name || (check.fno_id.is_some() && r.fno_id == check.fno_id))
                    && r.created_at == check.created_at
                    && r.spawned_by_session.as_deref() == Some(check.parent.as_str())
            });
            let mut evidence = String::new();
            let progress = if let Some(row) = row {
                check.name = row.name.clone();
                let owned = row.node.as_ref().is_some_and(|node| {
                    let (status, claim) = claims::status(&format!("node:{node}"), None);
                    status == claims::ClaimState::Live
                        && claim.is_some_and(|c| {
                            c.session_id == row.harness_session_id
                                && c.acquired_at > check.born_at
                                && c.reason.as_deref() != Some("spawn claim transfer")
                        })
                });
                let tools = row.tool_calls.unwrap_or(0) > check.tool_calls;
                let mut fold = crate::tool_activity::ToolFold::default();
                fold.offset = check.transcript_offset;
                let mut activity = false;
                if let Some(path) = transcript(row) {
                    fold.absorb(&path, row.harness_name());
                    activity = transcript_activity(&path, check.transcript_offset);
                }
                let commit = check
                    .head
                    .as_ref()
                    .is_some_and(|old| head(&row.cwd).is_some_and(|new| new != *old));
                evidence = format!("state={:?}, worker-claim={owned}, new-tools={}, transcript-activity={activity}, commit-change={commit}", row.status, row.tool_calls.unwrap_or(0).saturating_sub(check.tool_calls).max(fold.calls));
                owned || tools || fold.calls > 0 || activity || commit
            } else {
                true
            };
            if progress {
                check.finished = true;
            } else {
                let text = format!("worker {}: no progress {}m after spawn. Evidence: {evidence}. First-check notice {} of 2. Inspect fno agents peek {}.", check.name, (now - check.born_at) / 60_000, check.notices + 1, check.name);
                let argv = vec![
                    "fno".into(),
                    "agents".into(),
                    "mail".into(),
                    "send".into(),
                    "--from-name".into(),
                    "fno-first-check".into(),
                    "--origin".into(),
                    "scheduler".into(),
                    check.parent.clone(),
                    text,
                ];
                let (code, out, err) = runner(&argv, "");
                if !crate::mail_inject::mail_send_accepted(code, &out)
                    && !(code == 0
                        && crate::mail_inject::mail_send_receipt(&out).contains("queued (durable)"))
                {
                    return Err(format!("first-check delivery failed: {err}"));
                }
                check.notices += 1;
                check.finished = check.notices >= 2;
                check.deadline = now.saturating_add(check.interval);
            }
            state::write_json_atomic(&path, &check).map_err(|e| e.to_string())?;
            Ok(())
        })();
        if let Err(error) = result {
            first_error.get_or_insert_with(|| format!("{}: {error}", path.display()));
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn transcript(row: &RegistryEntry) -> Option<std::path::PathBuf> {
    row.transcript_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            row.harness_session_id.as_ref().and_then(|sid| {
                crate::context_run::SessionTranscripts::default().find(sid, row.harness_name())
            })
        })
        .or_else(|| row.log_path.as_ref().map(std::path::PathBuf::from))
}

fn transcript_activity(path: &Path, offset: u64) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut tail = String::new();
    if file
        .take(16 * 1024 * 1024)
        .read_to_string(&mut tail)
        .is_err()
    {
        return false;
    }
    tail.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| {
            row["type"] == "assistant"
                || row
                    .pointer("/message/role")
                    .is_some_and(|v| v == "assistant")
                || row
                    .pointer("/payload/role")
                    .is_some_and(|v| v == "assistant")
                || row
                    .pointer("/payload/type")
                    .is_some_and(|v| v == "agent_message")
        })
}

pub(crate) struct Arm {
    last: Instant,
    in_flight: Arc<AtomicBool>,
}
impl Default for Arm {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            last: now.checked_sub(Duration::from_secs(60)).unwrap_or(now),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Arm {
    pub(crate) fn tick(&mut self, home: AgentsHome) {
        if self.last.elapsed() < Duration::from_secs(60)
            || self.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        self.last = Instant::now();
        let flag = self.in_flight.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = crate::daemon::SweepGate(flag);
            if let Err(e) = run_pass(&home, claims::now_ms(), &mut crate::burn_watch::run_command) {
                eprintln!("first-check: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventEmitter;
    use crate::state::Lineage;
    use serde_json::json;

    #[test]
    fn birth_hands_over_only_the_parent_claim_and_checks_twice_across_restarts() {
        let _lock = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
        let _restore = Restore(
            ["FNO_AGENTS_HOME", "FNO_CLAIMS_ROOT", "FNO_CONFIG"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        );
        let temp = tempfile::tempdir().unwrap();
        let home = AgentsHome::at(temp.path().join("agents"));
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", home.root());
        std::env::set_var("FNO_CLAIMS_ROOT", temp.path());
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "[agents]\nfirst_check_minutes = 5\n").unwrap();
        std::env::set_var("FNO_CONFIG", &config);
        let parent = "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9";
        let child = "30a22982-fbc8-4e64-b848-4b82c68a17b3";
        let mut row = RegistryEntry::new(
            Some(child.into()),
            Lineage::captured((
                Some(parent.into()),
                Some("claude".into()),
                Some(cwd.to_string_lossy().into_owned()),
            )),
        );
        row.name = "worker".into();
        row.harness = Some("codex".into());
        row.origin = Some("spawn".into());
        row.status = crate::AgentStatus::Busy;
        row.node = Some("first-check-node".into());
        row.cwd = cwd.to_string_lossy().into_owned();
        row.created_at = crate::daemon::now_rfc3339_like();
        let key = "node:first-check-node";
        let opts = AcquireOpts {
            root: None,
            pid: Some(std::process::id()),
            ttl_ms: Some(7_200_000),
            identity: Some((parent.into(), "claude".into())),
            events_dir: Some(temp.path().join("events")),
            ..Default::default()
        };
        assert!(matches!(
            claims::acquire(key, &format!("target-session:{parent}"), opts.clone()),
            claims::AcquireOutcome::Acquired(_)
        ));
        state::update_registry(&home.registry_json(), |r| r.entries.push(row.clone())).unwrap();
        let mut lead = RegistryEntry::new(Some(parent.into()), Lineage::unproven("operator lead"));
        lead.name = "lead".into();
        lead.harness = Some("claude".into());
        lead.status = crate::AgentStatus::Busy;
        lead.crown_level = Some(1);
        lead.crown_scope = Some("first-check-node".into());
        state::update_registry(&home.registry_json(), |r| r.entries.push(lead)).unwrap();
        assert!(
            matches!(
                claims::acquire(key, "spawn-handover:worker", opts.clone()),
                claims::AcquireOutcome::Acquired(_)
            ),
            "the spawning lead can delegate its own claim before launch"
        );
        assert!(
            matches!(
                claims::acquire(key, "spawn-handover:another-worker", opts.clone()),
                claims::AcquireOutcome::HeldByOther { .. }
            ),
            "a pending launch cannot be delegated twice"
        );
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        emitter.emit("agent_spawned", &json!({"name": row.name, "spawned_by_session": parent, "harness_session_id": child, "cwd": row.cwd})).unwrap();
        let (_, claim) = claims::status(key, None);
        let claim = claim.unwrap();
        assert_eq!(claim.holder, format!("target-session:{child}"));
        assert_eq!(claim.session_id.as_deref(), Some(child));
        assert_eq!(claim.harness.as_deref(), Some("codex"));
        assert!(claim.pid_unavailable);
        assert_eq!(claim.metadata["dispatched_by_session"], parent);
        let path = std::fs::read_dir(home.root().join("first-checks"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let read_check =
            || -> Check { serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap() };
        let armed = read_check();
        assert_eq!(armed.interval, 300_000);
        assert_eq!(armed.deadline, armed.born_at + armed.interval);
        assert_eq!(armed.notices, 0);
        let mut notices = Vec::new();
        let mut deliver = |argv: &[String], _: &str| {
            assert_eq!(argv[8], parent);
            notices.push(argv[9].clone());
            (0, "delivered (hosted)".into(), String::new())
        };
        run_pass(&home, armed.deadline - 1, &mut deliver).unwrap();
        assert_eq!(read_check().notices, 0);
        let mut failed = |_: &[String], _: &str| (1, String::new(), "transport unavailable".into());
        assert!(run_pass(&home, armed.deadline, &mut failed).is_err());
        assert_eq!(read_check().notices, 0, "a failed delivery must retry");
        run_pass(&home, armed.deadline, &mut deliver).unwrap();
        assert_eq!(read_check().notices, 1);
        let rearmed = read_check().deadline;
        state::update_registry(&home.registry_json(), |r| {
            r.entries[0].name = "renamed-worker".into()
        })
        .unwrap();
        run_pass(&home, rearmed - 1, &mut deliver).unwrap();
        let corrupt = home.root().join("first-checks/corrupt.json");
        std::fs::write(&corrupt, "{broken").unwrap();
        assert!(run_pass(&home, rearmed, &mut deliver).is_err());
        std::fs::remove_file(corrupt).unwrap();
        run_pass(&home, rearmed + 1, &mut deliver).unwrap();
        assert!(read_check().finished);
        assert_eq!(notices.len(), 2);
        assert!(notices[0].contains("no progress"));
        assert!(notices[1].contains("renamed-worker"));

        claims::release(key, &claim.holder, None, None).unwrap();
        let foreign = AcquireOpts {
            identity: Some(("another-parent".into(), "claude".into())),
            ..opts.clone()
        };
        assert!(matches!(
            claims::acquire(key, "unrelated-holder", foreign),
            claims::AcquireOutcome::Acquired(_)
        ));
        transfer_birth_claim(&row, claims::now_ms()).unwrap();
        assert_eq!(
            claims::status(key, None).1.unwrap().holder,
            "unrelated-holder"
        );
        claims::release(key, "unrelated-holder", None, None).unwrap();
        assert!(matches!(
            claims::acquire(key, &format!("target-session:{parent}"), opts.clone()),
            claims::AcquireOutcome::Acquired(_)
        ));
        let child_opts = AcquireOpts {
            identity: Some((child.into(), "codex".into())),
            pid: None,
            pid_unavailable: true,
            ..opts
        };
        assert!(
            matches!(
                claims::acquire(key, &format!("target-session:{child}"), child_opts),
                claims::AcquireOutcome::Acquired(_)
            ),
            "a worker takes its own parent's claim without a handover flag"
        );

        claims::release(key, &format!("target-session:{child}"), None, None).unwrap();
        let mut active = read_check();
        active.finished = false;
        active.notices = 0;
        active.deadline = rearmed;
        state::write_json_atomic(&path, &active).unwrap();
        state::update_registry(&home.registry_json(), |r| r.entries[0].tool_calls = Some(1))
            .unwrap();
        run_pass(&home, rearmed, &mut failed).unwrap();
        assert!(read_check().finished, "observed tools suppress the notice");
    }
}
