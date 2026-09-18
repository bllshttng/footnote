//! The session-preserving Codex daemon upgrade transaction.
//!
//! A stale daemon (live version older than the installed CLI) upgrades ONLY
//! here: under the provider-daemon lock, after every loaded thread proves
//! readable and safe, through the vendor restart verb, with the old
//! incarnation's end, the new version, the initialize handshake, the config
//! bytes, and every snapshot id verified after. On any post-restart failure
//! the receipt reads `failed` with the ids it cannot read back. Never a
//! success-shaped line. Never a blind retry. This module is the ONLY caller
//! of the vendor restart (exactly one writer).

use crate::codex_daemon_readiness::{codex_daemon_readiness, CodexDaemonReadiness, VersionVerdict};
use crate::codex_inject::{
    connect_app_server, loaded_list_request_json, parse_loaded_list_response,
    parse_thread_read_cwd, parse_thread_read_status, read_until_id, thread_read_request_json,
    CodexDaemonAdapter,
};
use crate::harness_daemon::HarnessDaemonAdapter;
use futures_util::SinkExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio_tungstenite::tungstenite::Message;

/// One snapshot thread: the facts the transaction preserves and re-reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotThread {
    pub id: String,
    pub cwd: String,
    /// The runtime `status.type`, read at snapshot time.
    pub status: Option<String>,
    /// The fno registry row this thread rolls up under, when one exists.
    pub fno_row: Option<String>,
}

/// Why the transaction held without mutating anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldKind {
    /// The daemon is not stale, so there is nothing to upgrade.
    NotStale,
    /// The provider-daemon lock is held elsewhere.
    LockBusy,
}

/// The transaction's outcome. `upgraded` requires EVERY verification to
/// pass; anything less is `failed` with the missing facts named.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UpgradeOutcome {
    /// Not stale: the running daemon IS the installed version.
    ReusedCurrent {
        installed: Option<String>,
        live: Option<String>,
    },
    /// Stale-but-not-upgradable or lock busy: reported and held, nothing
    /// mutated.
    Held {
        kind: HoldKind,
        reason: String,
        installed: Option<String>,
        live: Option<String>,
        pid: Option<u32>,
    },
    /// Refused before mutation because the snapshot is not provably safe.
    Refused {
        reason: String,
        threads: Vec<SnapshotThread>,
    },
    /// The vendor restart ran but a post-restart verification failed.
    Failed {
        reason: String,
        threads: Vec<SnapshotThread>,
        missing_ids: Vec<String>,
    },
    /// Everything verified.
    Upgraded {
        before: serde_json::Value,
        after: serde_json::Value,
        threads: Vec<SnapshotThread>,
        config_unchanged: bool,
        /// The top-level `model_context_window` before and after, `None`
        /// when the config declares none. A key present before and absent
        /// after never reaches this arm: it is a failed transaction.
        model_context_window_before: Option<String>,
        model_context_window_after: Option<String>,
    },
}

/// The snapshot refusal: `Some(reason)` when ANY thread blocks mutation.
/// An active turn, a systemError thread, or an unreadable status each hold
/// the upgrade BEFORE the vendor restart runs. Pure and unit-tested.
pub fn snapshot_refusal(threads: &[SnapshotThread]) -> Option<String> {
    for thread in threads {
        match thread.status.as_deref() {
            Some("active") => {
                return Some(format!(
                    "thread {} has an active turn; refusing before mutation",
                    thread.id
                ))
            }
            Some("systemError") => {
                return Some(format!(
                    "thread {} reads systemError; refusing before mutation",
                    thread.id
                ))
            }
            None => {
                return Some(format!(
                    "thread {} reads no readable status; refusing before mutation",
                    thread.id
                ))
            }
            _ => {}
        }
    }
    None
}

/// The post-restart id fold: which snapshot ids are NOT readable afterward
/// with the same id and cwd. Pure and unit-tested.
pub fn missing_ids(snapshot: &[SnapshotThread], post: &[SnapshotThread]) -> Vec<String> {
    snapshot
        .iter()
        .filter(|before| {
            !post
                .iter()
                .any(|candidate| candidate.id == before.id && candidate.cwd == before.cwd)
        })
        .map(|before| before.id.clone())
        .collect()
}

/// The readiness snapshot as a JSON value for the receipt.
fn readiness_json(r: &crate::codex_daemon_readiness::CodexDaemonReadiness) -> serde_json::Value {
    serde_json::json!({
        "pid": r.pid,
        "start_token": r.start_token,
        "installed": r.installed_version,
        "live": r.live_version,
        "verdict": r.verdict,
    })
}

/// Walk `thread/loaded/list` page by page, then `thread/read` each id.
/// A thread that answers with an unreadable cwd or status reads `None`s -
/// the refusal classifier decides. Bounded paging, same cap as the loaded
/// roster's own walker.
pub(crate) async fn snapshot_threads(socket: &Path) -> Result<Vec<SnapshotThread>, String> {
    let (mut sink, mut stream) = connect_app_server(socket)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let mut ids = Vec::new();
    let mut cursor: Option<String> = None;
    let mut request_id = 2_u64;
    for _ in 0..64 {
        sink.send(Message::Text(
            loaded_list_request_json(request_id, cursor.as_deref()).into(),
        ))
        .await
        .map_err(|e| format!("send: {e}"))?;
        let raw = read_until_id(&mut stream, &serde_json::json!(request_id))
            .await
            .map_err(|e| format!("read: {e}"))?;
        let (page, next_cursor) =
            parse_loaded_list_response(&raw).map_err(|e| format!("loaded list: {e:?}"))?;
        ids.extend(page);
        request_id += 1;
        match next_cursor {
            None => break,
            Some(next) => cursor = Some(next),
        }
    }
    let mut threads = Vec::new();
    for id in ids {
        sink.send(Message::Text(
            thread_read_request_json(request_id, &id).into(),
        ))
        .await
        .map_err(|e| format!("send: {e}"))?;
        let raw = read_until_id(&mut stream, &serde_json::json!(request_id))
            .await
            .map_err(|e| format!("read: {e}"))?;
        threads.push(SnapshotThread {
            id,
            cwd: parse_thread_read_cwd(&raw).unwrap_or_default(),
            status: parse_thread_read_status(&raw),
            fno_row: None,
        });
        request_id += 1;
    }
    Ok(threads)
}

/// Locate the codex CLI through the shared resolver. `Err` = absent.
fn codex_bin() -> Result<PathBuf, String> {
    crate::codex_daemon_readiness::codex_cli_path()
        .ok_or_else(|| "codex CLI not found on PATH (set FNO_CODEX_BIN to override)".to_string())
}

/// The transaction entry. Lock, re-read, snapshot, restart, verify.
pub async fn codex_daemon_upgrade_transaction() -> UpgradeOutcome {
    let adapter = CodexDaemonAdapter::from_environment();
    let _lock = match crate::harness_daemon::try_acquire_lock(&adapter.lock_path()) {
        Some(lock) => lock,
        None => {
            return UpgradeOutcome::Held {
                kind: HoldKind::LockBusy,
                reason: "provider-daemon lock busy; another caller is mid-transaction".to_string(),
                installed: None,
                live: None,
                pid: None,
            }
        }
    };
    let before = codex_daemon_readiness();
    if matches!(before.verdict, VersionVerdict::Current) {
        return UpgradeOutcome::ReusedCurrent {
            installed: before.installed_version.clone(),
            live: before.live_version.clone(),
        };
    }
    if !matches!(before.verdict, VersionVerdict::Stale) {
        return UpgradeOutcome::Held {
            kind: HoldKind::NotStale,
            reason: "readiness is not readable enough to upgrade on".to_string(),
            installed: before.installed_version.clone(),
            live: before.live_version.clone(),
            pid: before.pid,
        };
    }
    let _ = &_lock;
    snapshot_and_swap(&adapter, before).await
}

/// Snapshot, classify, restart through the vendor verb, verify.
async fn snapshot_and_swap(
    adapter: &CodexDaemonAdapter,
    before: CodexDaemonReadiness,
) -> UpgradeOutcome {
    let mut snapshot = match snapshot_threads(&adapter.control_socket()).await {
        Ok(threads) => threads,
        Err(reason) => {
            return UpgradeOutcome::Refused {
                reason,
                threads: Vec::new(),
            }
        }
    };
    // Join each thread id with its fno registry row (crown/row association)
    // before anything mutates, so the receipt carries the association.
    let registry_rows = crate::restart_run::read_thread_rows(&crate::paths::AgentsHome::from_env());
    for thread in &mut snapshot {
        thread.fno_row = registry_rows
            .iter()
            .find(|row| row.session_id.as_deref() == Some(thread.id.as_str()))
            .map(|row| row.name.clone());
    }
    if let Some(reason) = snapshot_refusal(&snapshot) {
        return UpgradeOutcome::Refused {
            reason,
            threads: snapshot,
        };
    }
    let config_before = read_config_bytes();
    let window_before = match config_before.as_deref().map(read_context_window) {
        None => None,
        Some(Err(error)) => {
            return UpgradeOutcome::Refused {
                reason: format!("config.toml does not parse before the restart: {error}"),
                threads: snapshot,
            }
        }
        Some(Ok(window)) => window,
    };
    if let Err(reason) = codex_bin().and_then(|bin| vendor_restart(&bin)) {
        return UpgradeOutcome::Failed {
            reason,
            threads: snapshot,
            missing_ids: Vec::new(),
        };
    }
    verify_after_restart(adapter, before, snapshot, config_before, window_before).await
}

/// Post-restart verification. Every check reads live state; nothing is
/// assumed. Any failure reads `failed` with what is missing named.
async fn verify_after_restart(
    adapter: &CodexDaemonAdapter,
    before: CodexDaemonReadiness,
    snapshot: Vec<SnapshotThread>,
    config_before: Option<Vec<u8>>,
    window_before: Option<String>,
) -> UpgradeOutcome {
    let after = codex_daemon_readiness();
    let config_after = read_config_bytes();
    let config_unchanged = config_before == config_after;
    let window_after = match config_after.as_deref().map(read_context_window) {
        None => None,
        Some(Err(error)) => {
            return UpgradeOutcome::Failed {
                reason: format!("config.toml does not parse after the restart: {error}"),
                threads: snapshot,
                missing_ids: Vec::new(),
            }
        }
        Some(Ok(window)) => window,
    };
    if window_before != window_after {
        return UpgradeOutcome::Failed {
            reason: format!(
                "model_context_window changed across the transaction (writer=unknown, before {:?}, after {:?})",
                window_before, window_after
            ),
            threads: snapshot,
            missing_ids: Vec::new(),
        };
    }
    let same_incarnation = after.pid == before.pid && after.start_token == before.start_token;
    if !after.healthy || same_incarnation {
        return UpgradeOutcome::Failed {
            reason: if same_incarnation {
                "the daemon state still names the pre-restart incarnation".to_string()
            } else {
                "the daemon does not answer initialize after the restart".to_string()
            },
            threads: snapshot,
            missing_ids: Vec::new(),
        };
    }
    let post = match snapshot_threads(&adapter.control_socket()).await {
        Ok(post) => post,
        Err(reason) => {
            return UpgradeOutcome::Failed {
                reason,
                threads: snapshot,
                missing_ids: Vec::new(),
            }
        }
    };
    let missing = missing_ids(&snapshot, &post);
    if !matches!(after.verdict, VersionVerdict::Current) {
        return UpgradeOutcome::Failed {
            reason: "the restarted daemon does not read as the installed version".to_string(),
            threads: snapshot,
            missing_ids: missing,
        };
    }
    if !missing.is_empty() {
        return UpgradeOutcome::Failed {
            reason: "threads missing after the restart".to_string(),
            threads: snapshot,
            missing_ids: missing,
        };
    }
    UpgradeOutcome::Upgraded {
        before: readiness_json(&before),
        after: readiness_json(&after),
        threads: snapshot,
        config_unchanged,
        model_context_window_before: window_before,
        model_context_window_after: window_after,
    }
}

#[cfg(test)]
mod tests {
    use super::{missing_ids, snapshot_refusal, SnapshotThread};

    fn thread(id: &str, status: Option<&str>) -> SnapshotThread {
        SnapshotThread {
            id: id.to_string(),
            cwd: "/tmp/x".to_string(),
            status: status.map(str::to_string),
            fno_row: None,
        }
    }

    #[test]
    fn refusal_holds_on_active_systemerror_and_unreadable() {
        let idle = vec![thread("a", Some("idle")), thread("b", Some("notLoaded"))];
        assert_eq!(snapshot_refusal(&idle), None, "idle and notLoaded are safe");

        let active = vec![thread("a", Some("idle")), thread("b", Some("active"))];
        assert!(
            snapshot_refusal(&active)
                .expect("active refuses")
                .contains("b"),
            "the refusal names the holding thread"
        );

        let error = vec![thread("c", Some("systemError"))];
        assert!(snapshot_refusal(&error)
            .expect("systemError refuses")
            .contains("c"));

        let unreadable = vec![thread("d", None)];
        assert!(
            snapshot_refusal(&unreadable).is_some(),
            "unreadable status refuses"
        );
    }

    #[test]
    fn missing_ids_names_only_threads_that_lost_id_or_cwd() {
        let snapshot = vec![thread("keep", Some("idle")), thread("lose", Some("idle"))];
        // Same id, same cwd: present. Same id at a DIFFERENT cwd: the fold
        // reads it missing, because the readable world changed under it.
        let post = vec![thread("keep", Some("idle"))];
        assert_eq!(missing_ids(&snapshot, &post), vec!["lose".to_string()]);
        let moved = vec![{
            let mut t = thread("keep", Some("idle"));
            t.cwd = "/moved".to_string();
            t
        }];
        assert_eq!(
            missing_ids(&snapshot, &moved),
            vec!["keep".to_string(), "lose".to_string()],
            "a changed cwd is a missing thread"
        );
        let _ = &snapshot;
    }

    /// AC23-EDGE: a second transaction behind the lock reads lock-busy and
    /// never touches the daemon. The lock file is the provider's own; the
    /// test only holds and releases it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_second_concurrent_transaction_reads_lock_busy() {
        use crate::harness_daemon::HarnessDaemonAdapter;
        let adapter = crate::codex_inject::CodexDaemonAdapter::from_environment();
        let held = crate::harness_daemon::acquire_lock(&adapter.lock_path())
            .expect("test acquires the provider lock first");
        let outcome = crate::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
        drop(held);
        match outcome {
            crate::codex_daemon_upgrade::UpgradeOutcome::Held {
                kind: crate::codex_daemon_upgrade::HoldKind::LockBusy,
                ..
            } => {}
            other => panic!("expected lock-busy hold, got {other:?}"),
        }
    }
}

/// Run the vendor restart verb with the resolved CODEX_HOME. NEVER a hand
/// signal to the pid: `codex app-server daemon restart` is the vendor's own
/// swap and the only mutation this transaction performs. Blocking call; the
/// vendor verb is expected to be prompt.
fn vendor_restart(codex: &Path) -> Result<(), String> {
    let output = std::process::Command::new(codex)
        .args(["app-server", "daemon", "restart"])
        .env("CODEX_HOME", codex_home())
        .output()
        .map_err(|e| format!("run codex: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "codex app-server daemon restart failed (rc={})",
            output.status.code().unwrap_or(-1)
        ))
    }
}

/// The resolved CODEX_HOME (the adapter's lock path sits two levels under
/// it: <home>/app-server-daemon/fno-harness-daemon.lock).
fn codex_home() -> PathBuf {
    CodexDaemonAdapter::from_environment()
        .lock_path()
        .parent()
        .and_then(|parent| parent.parent())
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// The CODEX_HOME config.toml bytes, so the receipt can prove the config
/// survived the restart byte-identically. `None` = no config file.
fn read_config_bytes() -> Option<Vec<u8>> {
    std::fs::read(codex_home().join("config.toml")).ok()
}

/// The top-level `model_context_window` value from config bytes, rendered
/// as a string. `Ok(None)` = the key is absent (never invented). `Err` =
/// the config does not parse as TOML.
fn read_context_window(config: &[u8]) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(config).map_err(|e| e.to_string())?;
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(value
        .get("model_context_window")
        .map(|window| match window {
            toml::Value::Integer(n) => n.to_string(),
            other => other.to_string(),
        }))
}
