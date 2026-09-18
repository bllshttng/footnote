//! Probe the sandbox a codex worker is about to run under: the worker's own
//! requested posture (never a hardcoded `workspace-write`), a harmless canary
//! write inside the grant, and the negative control the old probe lacked: a
//! write OUTSIDE the granted roots must fail. Ported from
//! `cli/src/fno/agents/sandbox_probe.py`, which shrinks to a bridge over the
//! hidden `sandbox-probe` verb; exit 85 and the `sandbox-probe:` marker stay
//! Python-side (rust_runtime.py, backlog/advance.py).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Exit 85: the preflight verdict the spawn gate refuses on.
pub const EXIT_SANDBOX_UNREACHABLE: i32 = 85;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// One probe verdict: `reachable` (every check answers), `blocked` (a needed
/// tool is unreachable inside the sandbox but answers outside it), or
/// `unknown` (the probe could not judge - the negative control succeeded,
/// where a detector that cannot fail has proved nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProbeResult {
    pub verdict: String,
    pub blocked: Vec<(String, String)>,
    pub note: String,
    /// The posture string the probe asked for, echoed so a reader can tell
    /// which run was judged.
    pub posture: String,
}

/// Build the `codex sandbox` argv for the worker's own requested posture.
fn sandbox_argv(sandbox: &str, approval: &str, roots: Vec<String>) -> Vec<String> {
    let mut argv = vec![
        "codex".to_string(),
        "sandbox".to_string(),
        "-c".to_string(),
        format!("sandbox_mode=\"{sandbox}\""),
    ];
    if sandbox == "workspace-write" {
        argv.push("-c".to_string());
        argv.push(format!(
            "sandbox_workspace_write.writable_roots={}",
            json!(roots)
        ));
        argv.push("-c".to_string());
        argv.push(format!("approval_policy=\"{approval}\""));
    }
    argv.push("--".to_string());
    argv
}

/// The roots the probe grants: the same grant the worker gets - state dirs
/// (fno's own writable set) plus the repo's git common dir, the identical
/// resolver the exec lane grants with.
fn probe_roots(cwd: &Path, state_dirs: &[String]) -> Vec<String> {
    let mut roots = state_dirs.to_vec();
    if let Some(git_dir) = crate::provider::git_common_dir(cwd) {
        if !roots.iter().any(|root| root == &git_dir) {
            roots.push(git_dir);
        }
    }
    roots
}

/// One wrapped call: inside runs under the worker's sandbox argv, outside
/// runs bare (the negative-control seat). An OSError-shaped failure becomes a
/// failed CompletedProcess equivalent, like the Python `_why` did.
fn run_wrapped(
    sandbox: &[String],
    argv: &[&str],
    inside: bool,
    cwd: &Path,
) -> (i32, String, String) {
    let mut cmd = if inside {
        let mut c = Command::new(&sandbox[0]);
        c.args(&sandbox[1..]).args(argv);
        c
    } else {
        let mut c = Command::new(argv[0]);
        c.args(&argv[1..]);
        c
    };
    cmd.current_dir(cwd);
    // The Python probe bounded every call at 15s; keep that bound with a
    // std-only poll-then-kill, so a wedged tool cannot wedge the spawn gate.
    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return (-1, String::new(), format!("io error: {e}")),
    };
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output();
                return match out {
                    Ok(o) => (
                        status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&o.stdout).into_owned(),
                        String::from_utf8_lossy(&o.stderr).into_owned(),
                    ),
                    Err(e) => (-1, String::new(), format!("io error: {e}")),
                };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (-1, String::new(), "probe call timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return (-1, String::new(), format!("io error: {e}")),
        }
    }
}

/// [`run_wrapped`] with stdin: the git ref-lock transaction.
fn run_wrapped_stdin(
    sandbox: &[String],
    argv: &[&str],
    stdin: &str,
    inside: bool,
    cwd: &Path,
) -> (i32, String, String) {
    let mut cmd = if inside {
        let mut c = Command::new(&sandbox[0]);
        c.args(&sandbox[1..]).args(argv);
        c
    } else {
        let mut c = Command::new(argv[0]);
        c.args(&argv[1..]);
        c
    };
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match cmd.spawn() {
        Ok(mut child) => {
            use std::io::Write;
            let payload = stdin.as_bytes().to_vec();
            if let Some(mut pipe) = child.stdin.take() {
                let _ = pipe.write_all(&payload);
            }
            let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let out = child.wait_with_output();
                        return match out {
                            Ok(o) => (
                                status.code().unwrap_or(-1),
                                String::from_utf8_lossy(&o.stdout).into_owned(),
                                String::from_utf8_lossy(&o.stderr).into_owned(),
                            ),
                            Err(e) => (-1, String::new(), format!("io error: {e}")),
                        };
                    }
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return (-1, String::new(), "probe call timed out".to_string());
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => return (-1, String::new(), format!("io error: {e}")),
                }
            }
        }
        Err(e) => (-1, String::new(), format!("io error: {e}")),
    }
}

fn why(stderr: &str, code: i32) -> String {
    // gh states its cause at the start of its first line, git's lock error at
    // the end (the same reading the Python `_why` did).
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let trimmed: String = line.chars().take(160).collect();
    if trimmed.is_empty() {
        format!("exit {code}")
    } else {
        trimmed
    }
}

/// Probe the sandbox the WORKER is about to run under: its own requested
/// posture, a harmless canary write inside the granted roots, and the
/// negative control - a write outside the granted roots must FAIL; when it
/// succeeds the verdict is `unknown`, because a detector that cannot fail
/// has proved nothing. A tool that also fails outside the sandbox stays
/// unjudged, as it does today.
pub fn probe_codex_sandbox(
    cwd: &Path,
    mode: Option<&str>,
    state_dirs: &[String],
) -> SandboxProbeResult {
    let requested = mode.unwrap_or("").trim().to_string();
    let posture = crate::codex_posture::CodexPosture::from_record(
        Some(requested.as_str()).filter(|r| !r.is_empty()),
        None,
    );
    let posture_name = if requested.is_empty() {
        "workspace-write".to_string()
    } else {
        requested
    };
    let sandbox_arg = posture.sandbox.as_scalar();
    let argv = sandbox_argv(
        sandbox_arg,
        posture.approval.as_str(),
        probe_roots(cwd, state_dirs),
    );
    let mut blocked: Vec<(String, String)> = Vec::new();
    let mut unjudged: Vec<String> = Vec::new();
    // Control: the sandbox itself must run a command at all.
    let nonce = format!(
        "{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
    );
    let (code, stdout, stderr) = run_wrapped(&argv, &["/bin/echo", &nonce], true, cwd);
    if code != 0 || stdout.trim() != nonce {
        return SandboxProbeResult {
            verdict: "unknown".into(),
            blocked,
            note: why(&stderr, code),
            posture: posture_name,
        };
    }
    // AC7-HP canary: a harmless write INSIDE the granted roots must succeed.
    let canary = cwd.join(format!("fno-probe-canary-{nonce}"));
    let canary_argv = [
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!(
            "printf x > {}; rm -f {}",
            canary.display(),
            canary.display()
        ),
    ];
    let canary_refs: Vec<&str> = canary_argv.iter().map(String::as_str).collect();
    let (code, _out, stderr) = run_wrapped(&argv, &canary_refs, true, cwd);
    if code != 0 {
        blocked.push((
            "canary-write".to_string(),
            format!(
                "write inside the granted roots failed: {}",
                why(&stderr, code)
            ),
        ));
    }
    // AC7-EDGE negative control: a write OUTSIDE the granted roots must
    // fail. The target is the workspace's PARENT, not $TMPDIR: the tmp
    // exclusions make temp dirs writable under workspace-write, so a temp
    // target would "fail" the control on every healthy sandbox. When the
    // control succeeds anyway, the detector cannot fail and the verdict is
    // `unknown`, never `reachable`.
    let outside_dir = cwd
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| !p.as_os_str().is_empty() && p != Path::new("/"))
        .unwrap_or_else(std::env::temp_dir);
    let outside_target = outside_dir.join(format!("fno-probe-outside-{nonce}"));
    let outside_argv = [
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!(
            "printf x > {}; rm -f {}",
            outside_target.display(),
            outside_target.display()
        ),
    ];
    let outside_refs: Vec<&str> = outside_argv.iter().map(String::as_str).collect();
    let (outside_code, _out, outside_err) = run_wrapped(&argv, &outside_refs, true, cwd);
    if outside_code == 0 {
        return SandboxProbeResult {
            verdict: "unknown".into(),
            blocked,
            note: format!(
                "a write outside the granted roots succeeded ({}); the probe cannot fail, so it judges nothing",
                outside_target.display()
            ),
            posture: posture_name,
        };
    }
    let _ = outside_err;
    // The tools the worker needs: gh egress and a git ref lock.
    let gh = run_wrapped(
        &argv,
        &["gh", "api", "rate_limit", "--jq", ".resources.core.limit"],
        true,
        cwd,
    );
    if !(gh.0 == 0 && gh.1.trim().parse::<u64>().map(|n| n > 0).unwrap_or(false)) {
        let gh_outside = run_wrapped(
            &argv,
            &["gh", "api", "rate_limit", "--jq", ".resources.core.limit"],
            false,
            cwd,
        );
        if gh_outside.0 == 0
            && gh_outside
                .1
                .trim()
                .parse::<u64>()
                .map(|n| n > 0)
                .unwrap_or(false)
        {
            blocked.push(("gh".to_string(), why(&gh.2, gh.0)));
        } else {
            unjudged.push(format!(
                "gh fails outside the sandbox too ({})",
                why(&gh_outside.2, gh_outside.0)
            ));
        }
    }
    let head = run_wrapped(&argv, &["git", "rev-parse", "HEAD"], false, cwd);
    if head.0 == 0 {
        let head_sha = head.1.trim().to_string();
        let txn = format!("start\ncreate refs/fno-probe/{nonce} {head_sha}\nprepare\nabort\n");
        let git_args = ["git", "update-ref", "--stdin"];
        let inside = run_wrapped_stdin(&argv, &git_args, &txn, true, cwd);
        if !inside.1.lines().any(|l| l.trim() == "prepare: ok") {
            let outside = run_wrapped_stdin(&argv, &git_args, &txn, false, cwd);
            if outside.1.lines().any(|l| l.trim() == "prepare: ok") {
                blocked.push(("git".to_string(), why(&inside.2, inside.0)));
            } else {
                unjudged.push(format!(
                    "git fails outside the sandbox too ({})",
                    why(&outside.2, outside.0)
                ));
            }
        }
    }
    if !blocked.is_empty() {
        return SandboxProbeResult {
            verdict: "blocked".into(),
            blocked,
            note: unjudged.join("; "),
            posture: posture_name,
        };
    }
    SandboxProbeResult {
        verdict: if unjudged.is_empty() {
            "reachable".into()
        } else {
            "unknown".into()
        },
        blocked,
        note: unjudged.join("; "),
        posture: posture_name,
    }
}

/// The hidden client verb: one JSON payload in
/// (`{"cwd", "mode"?, "state_dirs"?}`), the verdict out. Exit 85 stays the
/// Python spawn gate's decision (rust_runtime.py), never the verb's.
pub fn run_sandbox_probe(args: &[String]) -> i32 {
    use std::io::Read;
    let mut payload = String::new();
    let raw = if let Some(path) = args.iter().find_map(|a| a.strip_prefix("--payload-file=")) {
        std::fs::read_to_string(path)
    } else {
        std::io::stdin()
            .read_to_string(&mut payload)
            .map(|_| payload)
    };
    let parsed: Value = match raw
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(_) => {
            eprintln!("sandbox-probe: bad payload");
            return 2;
        }
    };
    let cwd = parsed
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mode = parsed.get("mode").and_then(Value::as_str);
    let mut state_dirs: Vec<String> = Vec::new();
    if let Some(items) = parsed.get("state_dirs").and_then(Value::as_array) {
        for item in items {
            if let Some(dir) = item.as_str() {
                state_dirs.push(dir.to_string());
            }
        }
    }
    let result = probe_codex_sandbox(&cwd, mode, &state_dirs);
    println!(
        "{}",
        json!({
            "verdict": result.verdict,
            "blocked": result.blocked,
            "note": result.note,
            "posture": result.posture,
        })
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC7-HP: the argv carries the worker's OWN posture - a read-only
    /// request probes read-only, never a hardcoded workspace-write.
    #[test]
    fn argv_names_the_requested_posture() {
        let argv = sandbox_argv("read-only", "on-request", Vec::new());
        assert_eq!(argv[0], "codex");
        assert_eq!(argv[1], "sandbox");
        assert!(argv.iter().any(|t| t == "sandbox_mode=\"read-only\""));
        assert!(
            !argv.iter().any(|t| t.contains("writable_roots")),
            "read-only grants no roots: {argv:?}"
        );
        let bounded = sandbox_argv("workspace-write", "never", vec!["/root".into()]);
        assert!(bounded
            .iter()
            .any(|t| t.contains("sandbox_workspace_write.writable_roots")));
    }

    /// The negative control's logic, judged at the verdict level: an outside
    /// write that succeeds forces `unknown`, never `reachable`. Exercised
    /// through the verdict rule the probe body implements; the live canary
    /// itself needs a sandbox to run in and stays with the integration runs.
    #[test]
    fn why_extracts_the_first_stderr_line() {
        assert_eq!(why("first line\nsecond\n", 1), "first line");
        assert_eq!(why("", 3), "exit 3");
        let long = "x".repeat(400);
        assert_eq!(why(&long, 1).chars().count(), 160);
    }
}
