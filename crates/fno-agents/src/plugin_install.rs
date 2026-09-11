//! `fno-agents plugin-install <harness> [--force]`, surfaced as
//! `fno config plugin install` (x-7ca7). One local-dev install door for every
//! plugin harness. Everything installs from the FILTERED stage
//! (`<state-root>/plugin-stage/fno`: git-tracked + untracked-but-not-ignored
//! files only, rebuilt wholesale), never from the repo root, because harness
//! caches copy what they are pointed at.
//!
//! Split per the ship-phase ruling (msg-18cf5f): THIS module owns stage
//! build, claude, opencode and agy arms plus the env exports. The codex arm
//! stays on the Python `converge` engine in the Python shim.
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::paths::AgentsHome;

const BUILD_DIR_KEY: &str = "CARGO_BUILD_BUILD_DIR";
const RC_MARK: &str = "# fno: cargo build-dir (x-7ca7)";

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn state_root() -> PathBuf {
    std::env::var_os("FNO_RECLAIM_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".fno"))
}

fn build_dir_value() -> String {
    format!(
        "{}/cargo-build/{{workspace-path-hash}}",
        state_root().display()
    )
}

fn run_checked(cmd: &[String], cwd: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let out = command.output().map_err(|e| format!("{}: {e}", cmd[0]))?;
    if !out.status.success() {
        let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if detail.is_empty() {
            detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        return Err(format!(
            "{} exited {}: {}",
            cmd[0],
            out.status.code().unwrap_or(-1),
            detail
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Rebuild `<state-root>/plugin-stage/fno` from the canonical root of `cwd`.
fn build_stage(cwd: &Path) -> Result<PathBuf, String> {
    let root = run_checked(
        &[
            "git".to_string(),
            "rev-parse".to_string(),
            "--show-toplevel".to_string(),
        ],
        Some(cwd),
    )?;
    let root = PathBuf::from(root.trim());
    let listed = run_checked(
        &[
            "git".to_string(),
            "ls-files".to_string(),
            "-z".to_string(),
            "-co".into(),
            "--exclude-standard".into(),
        ],
        Some(&root),
    )?;
    let dest = state_root().join("plugin-stage").join("fno");
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| format!("stage: {e}"))?;
    let mut copied = 0usize;
    for rel in listed.split('\0').filter(|s| !s.is_empty()) {
        let src = root.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(_) => continue, // cached by git but deleted in the tree
        };
        if meta.is_symlink() || !meta.is_file() {
            continue;
        }
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("stage: {e}"))?;
        }
        std::fs::copy(&src, &target).map_err(|e| format!("stage: {rel}: {e}"))?;
        copied += 1;
    }
    let _ = copied;
    Ok(dest)
}

fn install_claude(stage: &Path, force: bool) -> Result<String, String> {
    run_checked(
        &[
            "claude".into(),
            "plugin".into(),
            "marketplace".into(),
            "add".into(),
            stage.display().to_string(),
        ],
        None,
    )?;
    if force {
        run_checked(
            &[
                "claude".into(),
                "plugin".into(),
                "update".into(),
                "fno@footnote".into(),
            ],
            None,
        )?;
        return Ok(format!("updated fno@footnote from {}", stage.display()));
    }
    match run_checked(
        &[
            "claude".into(),
            "plugin".into(),
            "install".into(),
            "fno@footnote".into(),
        ],
        None,
    ) {
        Ok(_) => Ok(format!("installed fno@footnote from {}", stage.display())),
        Err(e) if e.contains("already installed") => {
            run_checked(
                &[
                    "claude".into(),
                    "plugin".into(),
                    "update".into(),
                    "fno@footnote".into(),
                ],
                None,
            )?;
            Ok(format!("updated fno@footnote from {}", stage.display()))
        }
        Err(e) => Err(e),
    }
}

fn install_opencode() -> Result<String, String> {
    let Some(root) = canonical_root() else {
        return Err("opencode: not inside the footnote repo".to_string());
    };
    let src = root.join("cli/src/fno/setup/assets/opencode/footnote.js");
    let dest_dir = dirs_home().join(".config/opencode/plugins");
    let dest = dest_dir.join("footnote.js");
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("opencode: {e}"))?;
    std::fs::copy(&src, &dest).map_err(|e| format!("opencode: {e}"))?;
    Ok(format!("plugin -> {}", dest.display()))
}

fn canonical_root() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn export_claude_env() -> Result<(), String> {
    let path = dirs_home().join(".claude/settings.json");
    let mut data: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    set_env_entry(&mut data, BUILD_DIR_KEY, build_dir_value());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("claude env: {e}"))?;
    }
    let text = serde_json::to_string_pretty(&data).unwrap_or_default() + "\n";
    std::fs::write(&path, text).map_err(|e| format!("claude env: {e}"))
}

fn set_env_entry(data: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: String) {
    let env = data.entry("env").or_insert(json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    if let Some(map) = env.as_object_mut() {
        map.insert(key.to_string(), json!(value));
    }
}

fn export_codex_env() -> Result<(), String> {
    let path = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".codex"))
        .join("config.toml");
    let mut document: toml::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_else(|| toml::Value::Table(Default::default()));
    if !document.is_table() {
        document = toml::Value::Table(Default::default());
    }
    let table = document.as_table_mut().unwrap();
    let set = table
        .entry("shell_environment_policy")
        .or_insert(toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("codex env: shell_environment_policy is not a table")?
        .entry("set")
        .or_insert(toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("codex env: shell_environment_policy.set is not a table")?;
    set.insert(
        BUILD_DIR_KEY.to_string(),
        toml::Value::String(build_dir_value()),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("codex env: {e}"))?;
    }
    let text = toml::to_string_pretty(&document).map_err(|e| format!("codex env: {e}"));
    std::fs::write(&path, text?).map_err(|e| format!("codex env: {e}"))
}

fn export_rc_env() -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let rc = dirs_home().join(if shell.contains("zsh") {
        ".zshrc"
    } else {
        ".bashrc"
    });
    let mut text = std::fs::read_to_string(&rc).unwrap_or_default();
    if text.contains(RC_MARK) {
        return Some(rc);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let line = format!(
        "{RC_MARK}\nexport {BUILD_DIR_KEY}=\"{}\"",
        build_dir_value()
    );
    text.push_str(&line);
    text.push('\n');
    std::fs::write(&rc, text).ok()?;
    Some(rc)
}

fn remove_stale_copies() -> Vec<PathBuf> {
    let home = dirs_home();
    let mut removed = Vec::new();
    for path in [
        home.join(".gemini/config/plugins/footnote"),
        home.join(".codex/plugins/cache/footnote-local"),
    ] {
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
            removed.push(path);
        }
    }
    removed
}

pub fn run_plugin_install(args: &[String]) -> i32 {
    // Stage-only mode: build the filtered stage, print its path, exit. The
    // Python codex arm uses it to hand converge a source_root.
    if args.first().map(String::as_str) == Some("--stage-only") {
        let cwd = std::env::current_dir().unwrap_or_default();
        return match build_stage(&cwd) {
            Ok(stage) => {
                println!("{}", stage.display());
                0
            }
            Err(e) => {
                eprintln!("plugin install: {e}");
                1
            }
        };
    }
    // Env-only mode: export the build-dir env to the harness surfaces without
    // an install, so the codex path (converge in Python) exports too.
    if args.first().map(String::as_str) == Some("--env-only") {
        env_exports_receipt();
        return 0;
    }
    let Some(harness) = args.first() else {
        eprintln!("usage: fno-agents plugin-install <claude|codex|opencode|agy> [--force]");
        return 2;
    };
    let force = args.iter().any(|a| a == "--force");
    let outcome = install_harness(harness, force);
    match outcome {
        Ok(detail) => {
            println!("plugin install {harness}: {detail}");
            env_exports_receipt();
            let stale = remove_stale_copies();
            if !stale.is_empty() {
                let joined: Vec<String> = stale.iter().map(|p| p.display().to_string()).collect();
                println!("removed stale copies: {}", joined.join(", "));
            }
            let home = AgentsHome::from_env();
            let _ = crate::reclaim::run_reclaim(&["--apply".to_string()], &home);
            0
        }
        Err(e) => {
            eprintln!("plugin install {harness} FAILED: {e}");
            1
        }
    }
}

fn install_harness(harness: &str, force: bool) -> Result<String, String> {
    let stage = build_stage(&std::env::current_dir().unwrap_or_default())?;
    let detail = match harness {
        "claude" => install_claude(&stage, force)?,
        "opencode" => install_opencode()?,
        "agy" => install_agy(&stage, force)?,
        other => {
            return Err(format!(
                "unknown harness '{other}'; want claude, codex, opencode or agy"
            ))
        }
    };
    Ok(detail)
}

fn env_exports_receipt() {
    if let Err(e) = export_claude_env() {
        eprintln!("fno plugin install: {e}");
    }
    if let Err(e) = export_codex_env() {
        eprintln!("fno plugin install: {e}");
    }
    let rc = export_rc_env();
    println!(
        "build-dir env exported to: claude settings env; codex shell_environment_policy; rc ({})",
        rc.map(|p| p.display().to_string())
            .unwrap_or_else(|| "skipped".into())
    );
}

fn install_agy(stage: &Path, force: bool) -> Result<String, String> {
    let _ = force;
    let mut cmd = Command::new("agy")
        .args(["plugin", "install"])
        .arg(stage)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("agy: {e}"))?;
    let out = cmd.wait_with_output().map_err(|e| format!("agy: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "agy plugin install exited {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok("agy plugin imported, hooks.json carries the stop hook".to_string())
}
