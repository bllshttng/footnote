//! The Messages tab's model: the thread projection gathered from the hidden
//! `fno-agents mail-threads --format json` verb, with org_model's bounded,
//! fail-open shape. A timeout, a non-zero exit or invalid JSON lands in
//! [`MessagesSnapshot::error`], never a paint refusal; the view renders its
//! error line and keeps the last good projection.

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct MessagesSnapshot {
    pub projection: Option<Value>,
    error: Option<String>,
    error_at: Option<u64>,
}

impl MessagesSnapshot {
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
/// like org_model::gather's court-fold call.
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
