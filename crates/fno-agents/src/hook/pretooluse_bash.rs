//! One Bash PreToolUse entry for the guards that share the same payload.
//!
//! Keep the former registration order. Rust guards run in-process; the three
//! Python guards keep their existing owners and run as children with the same
//! stdin and environment. A denial from any guard is returned after all six
//! have recorded their decision. The guardrail preset owns whether a guard
//! runs at all: a preset-disabled guard answers nothing and emits no row.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn run(_args: &[String]) -> i32 {
    let raw = super::read_stdin();
    let payload: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_root = std::env::var_os("FNO_REPO_ROOT")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .or_else(crate::provider::plugin_root);
    let mut refusals = Vec::new();

    // One config resolution for the whole chain: guard_enabled re-walks the
    // config candidates per call, and this hook runs on every Bash turn.
    let preset = crate::agents_config::guard_preset(&cwd);

    if crate::agents_config::preset_runs(preset, "bg-process") {
        if let Some(root) = plugin_root.as_deref() {
            if let Some(reason) = run_python_guard(&root, "bg-process-guard.py", &raw) {
                refusals.push(reason);
            }
        } else {
            eprintln!("pretooluse-bash: plugin root unavailable; skipping bg-process-guard");
        }
    }

    if crate::agents_config::preset_runs(preset, "bin-install") {
        let bin_refusal = super::bin_install_guard::judge(&payload);
        super::emit_guard_decision(&cwd, "bin-install-guard", "Bash", bin_refusal.is_some());
        refusals.extend(bin_refusal);
    }

    if crate::agents_config::preset_runs(preset, "git-protection") {
        if let Some(root) = plugin_root.as_deref() {
            if let Some(reason) = run_python_guard(&root, "git-protection.py", &raw) {
                refusals.push(reason);
            }
        } else {
            eprintln!("pretooluse-bash: plugin root unavailable; skipping git-protection guard");
        }
    }

    if crate::agents_config::preset_runs(preset, "pipe") {
        let pipe_refusal = super::pipe_guard::judge(&payload);
        super::emit_guard_decision(&cwd, "pipe-guard", "Bash", pipe_refusal.is_some());
        refusals.extend(pipe_refusal);
    }

    if crate::agents_config::preset_runs(preset, "recursive-grep") {
        if let Some(root) = plugin_root.as_deref() {
            if let Some(reason) = run_python_guard(&root, "recursive-grep-guard.py", &raw) {
                refusals.push(reason);
            }
        } else {
            eprintln!("pretooluse-bash: plugin root unavailable; skipping recursive-grep-guard");
        }
    }

    let test = if crate::agents_config::preset_runs(preset, "test-run") {
        super::test_run_guard::evaluate(&payload)
    } else {
        super::test_run_guard::Evaluation {
            should_log: false,
            refusal: None,
            stage: "preset-disabled",
        }
    };
    if test.should_log {
        super::emit_guard_decision(
            &process_cwd,
            "test-run-guard",
            "Bash",
            test.refusal.is_some(),
        );
    }
    refusals.extend(test.refusal);

    if crate::agents_config::preset_runs(preset, "effect") {
        let effect_refusal = crate::effect_gate::judge(&payload, &cwd);
        super::emit_guard_decision(&cwd, "effect-guard", "Bash", effect_refusal.is_some());
        refusals.extend(effect_refusal);
    }

    if refusals.is_empty() {
        super::emit_allow()
    } else {
        super::emit_block(&refusals.join("\n\n"))
    }
}

fn run_python_guard(root: &Path, script: &str, input: &str) -> Option<String> {
    let path = root.join("hooks").join(script);
    let mut child = match Command::new("python3")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "pretooluse-bash: could not start {}: {error}; allowing",
                path.display()
            );
            return None;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(input.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "pretooluse-bash: could not send payload to {}: {error}; allowing",
                path.display()
            );
            return None;
        }
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!(
                "pretooluse-bash: could not read {}: {error}; allowing",
                path.display()
            );
            return None;
        }
    };
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        eprintln!(
            "pretooluse-bash: {} exited {}; allowing",
            path.display(),
            output.status
        );
        return None;
    }
    python_refusal(&output.stdout)
}

fn python_refusal(stdout: &[u8]) -> Option<String> {
    let response: Value = serde_json::from_slice(stdout).ok()?;
    let specific = response.get("hookSpecificOutput").unwrap_or(&response);
    let decision = specific
        .get("permissionDecision")
        .or_else(|| response.get("decision"))
        .and_then(Value::as_str);
    if !matches!(decision, Some("deny" | "block")) {
        return None;
    }
    specific
        .get("permissionDecisionReason")
        .or_else(|| response.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| Some("a Bash PreToolUse guard denied this command".to_string()))
}
