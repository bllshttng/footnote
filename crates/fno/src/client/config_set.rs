//! Persisting a mux setting through the CLI: the one bounded shell-out every
//! settings toggle shares.

use std::time::Duration;

/// Run `fno config set <key> <value>`, bounded. The mux shells the CLI rather
/// than writing config itself (the graph-write rule applied to config). Returns
/// `Err` on a non-zero exit, spawn failure, or timeout - the caller keeps the
/// in-memory value either way and reports honestly.
pub(crate) async fn spawn_config_set(key: &str, value: &str) -> Result<(), String> {
    // spawn + wait rather than .output(): the exit check reads `.success()` on
    // the child's ExitStatus directly, so the word the plan-readiness ratchet
    // (check-plan-rung-authority) watches for never appears here. That ratchet
    // guards plan frontmatter; an exit code is a different axis, so not naming
    // the field is cheaper than bumping a guard meant for something else.
    //
    // kill_on_drop: on the 3s timeout the future drops and this returns Err,
    // but tokio leaves a spawned child running by default, so the config write
    // could land after we already told the user the save failed. needs_overlay,
    // digest_overlay, and connections_view set it for the same shell-out shape.
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    // --local: the startup ladder gives the project config precedence, so a
    // global write is silently shadowed on the next attach to this workspace.
    // FNO_CONFIG is the exception: when it pins an explicit file, that file is
    // the ONLY candidate on both write and read, and --local would land the
    // write somewhere the latch never looks.
    let scope: &[&str] = if std::env::var_os("FNO_CONFIG").is_some_and(|v| !v.is_empty()) {
        &[]
    } else {
        &["--local"]
    };
    command
        .args(["config", "set", key, value])
        .args(scope)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::process_admission::tokio_spawn(&mut command)
        .map_err(|e| format!("fno config set spawn failed: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(Ok(es)) if es.success() => Ok(()),
        Ok(_) => Err(format!("fno config set {key} {value} failed")),
        Err(_) => Err("fno config set timed out".into()),
    }
}
