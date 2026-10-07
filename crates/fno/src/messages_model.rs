//! The Messages tab's model: the thread projection gathered from the hidden
//! `fno-agents mail-threads --format json` verb, with org_model's bounded,
//! fail-open shape. A timeout, a non-zero exit or invalid JSON lands in
//! [`MessagesSnapshot::error`], never a paint refusal; the view renders its
//! error line and keeps the last good projection.

use serde_json::Value;

/// The last good projection for the process's lifetime: a reopened Messages
/// tab paints it at once instead of an empty screen (item 1). One row, in a
/// mutex; the first gather replaces it.
static CACHE: std::sync::LazyLock<std::sync::Mutex<Option<Value>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// The cached projection, if a read ever landed in this process.
pub fn cached() -> Option<Value> {
    CACHE.lock().ok().and_then(|guard| guard.as_ref().cloned())
}

/// Cache a landed projection for the next open (item 1). Called by the
/// view's apply_gather, never by the snapshot's own apply - tests apply
/// fixtures freely without polluting the process-global cache.
pub(crate) fn remember(projection: &Value) {
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(projection.clone());
    }
}

/// The mail store's change signal (item 1): the chats dir's entry count and
/// its messages.jsonl files' total length and newest mtime, read every 5s by
/// the open board. No change, no re-read.
///
/// The dir resolves through the same ladder `fno-agents chats::chats_dir`
/// applies (a `paths.chats` config override, else `<state>/chats`); the
/// mirror lives here because the client crate cannot call the agent crate.
pub fn store_fingerprint() -> Option<(usize, u64, u64)> {
    let dir = chats_dir()?;
    let rd = std::fs::read_dir(dir).ok()?;
    let mut count = 0usize;
    let mut total_len = 0u64;
    let mut newest = 0u64;
    for entry in rd.flatten() {
        count += 1;
        let Ok(meta) = entry.path().join("messages.jsonl").metadata() else {
            continue;
        };
        total_len += meta.len();
        newest = newest.max(
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
    }
    Some((count, total_len, newest))
}

/// `chats_dir` as the agent crate resolves it: the `paths.chats` override
/// from `<cwd>/.fno/config.toml` then `~/.fno/config.toml`, else
/// `<state_dir>/chats`.
fn chats_dir() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".fno/config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::PathBuf::from(home).join(".fno/config.toml"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = text.parse::<toml::Table>() else {
            continue;
        };
        if let Some(raw) = value
            .get("paths")
            .and_then(|p| p.get("chats"))
            .and_then(toml::Value::as_str)
        {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let expanded = trimmed
                    .strip_prefix("~/")
                    .map(|rest| {
                        std::env::var_os("HOME")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_default()
                            .join(rest)
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from(trimmed));
                return Some(expanded);
            }
        }
    }
    Some(crate::model_catalog::state_dir().join("chats"))
}

#[derive(Debug, Clone, Default)]
pub struct MessagesSnapshot {
    pub projection: Option<Value>,
    error: Option<String>,
    error_at: Option<u64>,
}

impl MessagesSnapshot {
    /// A snapshot opening onto the cached last read, if any (item 1).
    pub fn with_cache() -> Self {
        Self {
            projection: cached(),
            error: None,
            error_at: None,
        }
    }
    pub fn apply(&mut self, value: Value) {
        self.projection = Some(value);
        self.error = None;
        self.error_at = None
    }
    pub fn fail(&mut self, reason: String, now: u64) {
        self.error = Some(reason);
        self.error_at = Some(now)
    }
    pub fn error_line(&self, now: u64) -> String {
        match (&self.projection, &self.error) {
            (None, Some(reason)) => format!(
                "{reason} (failed {}s ago)",
                now.saturating_sub(self.error_at.unwrap_or(now))
            ),
            (None, None) => "not read".into(),
            (Some(_), Some(reason)) => format!("stale: {reason}"),
            (Some(_), None) => String::new(),
        }
    }
}

/// Gather the projection: `fno-agents mail-threads --format json` with
/// org_model's 30s bound and fail-open shape. The command path rides PATH
/// like org_model::gather's team-fold call.
pub async fn gather() -> Result<Value, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("fno-agents")
            .args(["mail-threads", "--format", "json"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match output {
        Err(_) => Err("mail-threads timed out after 30s".into()),
        Ok(Err(error)) => Err(format!("mail-threads could not run: {error}")),
        Ok(Ok(output)) if !output.status.success() => Err(format!(
            "mail-threads exited {}: {}",
            output
                .status
                .code()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "by signal".into()),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(Ok(output)) => serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("mail-threads invalid JSON: {e}")),
    }
}
