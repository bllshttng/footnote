//! `fno-agents plugin-install <harness> [--force]`, surfaced as
//! `fno config plugin install`. One local-dev install door for every
//! plugin harness. Everything installs from the FILTERED stage
//! (`<state-root>/plugin-stage/fno`: git-tracked + untracked-but-not-ignored
//! files only, rebuilt wholesale), never from the repo root, because harness
//! caches copy what they are pointed at.
//!
//! This module owns stage build, the `--check`/`--restage` deploy verbs, the
//! claude, opencode and agy arms plus the env exports. The codex arm stays on
//! the Python `converge` engine in the Python shim.
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde::Serialize;
use serde_json::json;

use crate::paths::{dirs_home, worktree_repo_root, AgentsHome};

const BUILD_DIR_KEY: &str = "CARGO_BUILD_BUILD_DIR";
const RC_MARK: &str = "# fno: cargo build-dir";

/// Same shape as the Python gate in cli/src/fno/hook_config.py:29.
const HOOK_REF_PATTERN: &str =
    r#"\$\{(?:CLAUDE_PLUGIN_ROOT|CODEX_PLUGIN_ROOT|PLUGIN_ROOT)\}/([^\s"'\\]+)"#;

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

fn run_checked_stdin(cmd: &[String], cwd: Option<&Path>, stdin: &str) -> Result<String, String> {
    use std::io::Write;
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn().map_err(|e| format!("{}: {e}", cmd[0]))?;
    let mut stdin_handle = child
        .stdin
        .take()
        .ok_or_else(|| format!("{}: no stdin", cmd[0]))?;
    let input = stdin.to_string();
    // The child's stdout pipe fills while it consumes stdin, so write from a
    // thread: hashing a 3.8k-file stage deadlocks a single-threaded write.
    let writer = std::thread::spawn(move || {
        let _ = stdin_handle.write_all(input.as_bytes());
    });
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{}: {e}", cmd[0]))?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(format!(
            "{} exited {}: {}",
            cmd[0],
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The git checkout that owns `dir`.
fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let root = run_checked(
        &[
            "git".to_string(),
            "rev-parse".to_string(),
            "--show-toplevel".to_string(),
        ],
        Some(dir),
    )?;
    Ok(PathBuf::from(root.trim()))
}

/// Copy hook-config-referenced scripts the new tree lacks from the old stage.
/// Sessions started before the restage hold the old hook config for their
/// whole life; the hook-tombstones lint protects one merge range, but a stage
/// stale across many merges can still reference a script the new tree deleted.
fn carry_referenced_scripts(old_stage: &Path, new_stage: &Path) -> usize {
    let re = match Regex::new(HOOK_REF_PATTERN) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut carried = 0usize;
    for config in ["hooks/hooks.json", "hooks/codex-hooks.json"] {
        let text = match std::fs::read_to_string(old_stage.join(config)) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for rel in re.captures_iter(&text).map(|c| c[1].to_string()) {
            let target = new_stage.join(&rel);
            if target.exists() {
                continue;
            }
            let src = old_stage.join(&rel);
            if !src.is_file() {
                continue;
            }
            if let Some(parent) = target.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            if std::fs::copy(&src, &target).is_ok() {
                carried += 1;
            }
        }
    }
    carried
}

/// Rebuild `<stage_parent>/fno` from `source_root` (git-tracked +
/// untracked-but-not-ignored files). Builds into a sibling temp dir and swaps
/// by rename so live sessions execing hooks from the stage never see a
/// half-built tree; running bash hooks keep their open inode and finish on
/// old bytes. Returns the stage path and the file count copied.
fn build_stage(source_root: &Path, stage_parent: &Path) -> Result<(PathBuf, usize), String> {
    let listed = run_checked(
        &[
            "git".to_string(),
            "ls-files".to_string(),
            "-z".to_string(),
            "-co".into(),
            "--exclude-standard".into(),
        ],
        Some(source_root),
    )?;
    let dest = stage_parent.join("fno");
    let pid = std::process::id();
    let new_dir = stage_parent.join(format!(".fno.new-{pid}"));
    let old_dir = stage_parent.join(format!(".fno.old-{pid}"));
    let _ = std::fs::remove_dir_all(&new_dir);
    let _ = std::fs::remove_dir_all(&old_dir);
    std::fs::create_dir_all(&new_dir).map_err(|e| format!("stage: {e}"))?;

    let copy_all = || -> Result<usize, String> {
        let mut copied = 0usize;
        for rel in listed.split('\0').filter(|s| !s.is_empty()) {
            let src = source_root.join(rel);
            let meta = match std::fs::symlink_metadata(&src) {
                Ok(m) => m,
                Err(_) => continue, // cached by git but deleted in the tree
            };
            if meta.is_symlink() || !meta.is_file() {
                continue;
            }
            let target = new_dir.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("stage: {e}"))?;
            }
            std::fs::copy(&src, &target).map_err(|e| format!("stage: {rel}: {e}"))?;
            copied += 1;
        }
        Ok(copied)
    };
    let copied = match copy_all() {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&new_dir);
            return Err(e);
        }
    };

    if dest.exists() {
        carry_referenced_scripts(&dest, &new_dir);
        // ponytail: two concurrent manual installs can race these renames;
        // the update:fno claim serializes the automated path.
        std::fs::rename(&dest, &old_dir)
            .map_err(|e| format!("stage: could not set aside the live stage: {e}"))?;
        if let Err(e) = std::fs::rename(&new_dir, &dest) {
            let _ = std::fs::rename(&old_dir, &dest);
            return Err(format!("stage: could not activate the new stage: {e}"));
        }
        let _ = std::fs::remove_dir_all(&old_dir);
    } else if let Err(e) = std::fs::rename(&new_dir, &dest) {
        let _ = std::fs::remove_dir_all(&new_dir);
        return Err(format!("stage: {e}"));
    }
    Ok((dest, copied))
}

#[derive(Serialize)]
struct StageCheck {
    status: &'static str,
    stage: String,
    source: String,
    source_head: String,
    differing_count: usize,
    missing_count: usize,
    sample: Vec<String>,
    remedy: String,
    detail: Option<String>,
}

fn absent_check(stage: &Path, source: &Path) -> StageCheck {
    StageCheck {
        status: "absent",
        stage: stage.display().to_string(),
        source: source.display().to_string(),
        source_head: String::new(),
        differing_count: 0,
        missing_count: 0,
        sample: Vec::new(),
        remedy: String::new(),
        detail: None,
    }
}

fn unknown_check(stage: &Path, source: &Path, detail: String) -> StageCheck {
    StageCheck {
        status: "unknown",
        stage: stage.display().to_string(),
        source: source.display().to_string(),
        source_head: String::new(),
        differing_count: 0,
        missing_count: 0,
        sample: Vec::new(),
        remedy: String::new(),
        detail: Some(detail),
    }
}

/// Byte verdict for the stage against the source checkout's HEAD. Stage files
/// HEAD lacks are ignored: the swap carries referenced scripts forward on
/// purpose, so extra files are not drift.
fn check_stage_report(stage: &Path, source_dir: &Path) -> StageCheck {
    if !stage.exists() {
        return absent_check(stage, source_dir);
    }
    let root = match repo_root(source_dir) {
        Ok(r) => r,
        Err(e) => return unknown_check(stage, source_dir, e),
    };
    let head = match run_checked(
        &[
            "git".to_string(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ],
        Some(&root),
    ) {
        Ok(h) => h,
        Err(e) => return unknown_check(stage, source_dir, e),
    };
    let listing = match run_checked(
        &[
            "git".to_string(),
            "ls-tree".to_string(),
            "-r".to_string(),
            "-z".to_string(),
            "HEAD".to_string(),
        ],
        Some(&root),
    ) {
        Ok(l) => l,
        Err(e) => return unknown_check(stage, source_dir, e),
    };

    let mut tracked: Vec<(String, String)> = Vec::new(); // (path, blob sha)
    for entry in listing.split('\0').filter(|s| !s.is_empty()) {
        let (meta, path) = match entry.split_once('\t') {
            Some(pair) => pair,
            None => continue,
        };
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        let sha = parts.next().unwrap_or("").to_string();
        // Symlinks (120000) and submodules (160000) are not files; build_stage
        // skips them too.
        if kind != "blob" || mode == "120000" || mode == "160000" {
            continue;
        }
        tracked.push((path.to_string(), sha));
    }

    let mut missing: Vec<String> = Vec::new();
    let mut to_hash: Vec<(&str, &str)> = Vec::new(); // (stage path, HEAD sha)
    for (path, sha) in &tracked {
        if stage.join(path).is_file() {
            to_hash.push((path, sha));
        } else {
            missing.push(path.clone());
        }
    }
    let mut differing: Vec<String> = Vec::new();
    if !to_hash.is_empty() {
        let stdin = format!(
            "{}\n",
            to_hash
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join("\n")
        );
        match run_checked_stdin(
            &[
                "git".to_string(),
                "hash-object".to_string(),
                "--stdin-paths".to_string(),
            ],
            // Relative paths resolve against the STAGE, wherever --check
            // was invoked from; hash-object needs no repo for plain hashing.
            Some(stage),
            &stdin,
        ) {
            Ok(out) => {
                for (line, (path, sha)) in out.lines().zip(to_hash.iter()) {
                    if line.trim() != *sha {
                        differing.push((*path).to_string());
                    }
                }
            }
            Err(e) => return unknown_check(stage, source_dir, e),
        }
    }

    let mut combined = differing.clone();
    combined.extend(missing.clone());
    combined.sort();
    let status = if combined.is_empty() {
        "fresh"
    } else {
        "stale"
    };
    let source_root = root.display().to_string();
    StageCheck {
        status,
        stage: stage.display().to_string(),
        source: source_root.clone(),
        source_head: head,
        differing_count: differing.len(),
        missing_count: missing.len(),
        sample: combined.into_iter().take(10).collect(),
        remedy: format!("cd {source_root} && fno config plugin install claude"),
        detail: None,
    }
}

#[derive(Debug)]
enum RestageOutcome {
    Absent,
    Restaged {
        files: usize,
        root: PathBuf,
        head12: String,
    },
}

/// Rebuild an EXISTING stage from `source_dir`. Never installs, exports env,
/// or reclaims: those belong to install, not deploy. A machine that never set
/// up the directory marketplace has no stage and gets a no-op.
fn restage_stage(source_dir: &Path, stage_parent: &Path) -> Result<RestageOutcome, String> {
    let stage = stage_parent.join("fno");
    if !stage.exists() {
        return Ok(RestageOutcome::Absent);
    }
    let root = repo_root(source_dir)?;
    let head = run_checked(
        &[
            "git".to_string(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ],
        Some(&root),
    )?;
    let (_, files) = build_stage(&root, &stage_parent)?;
    Ok(RestageOutcome::Restaged {
        files,
        root,
        head12: head.chars().take(12).collect(),
    })
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
    let root = worktree_repo_root(&std::env::current_dir().unwrap_or_default());
    let src = root.join("cli/src/fno/setup/assets/opencode/footnote.js");
    if !src.is_file() {
        return Err("opencode: not inside the footnote repo".to_string());
    }
    let dest_dir = dirs_home().join(".config/opencode/plugins");
    let dest = dest_dir.join("footnote.js");
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("opencode: {e}"))?;
    std::fs::copy(&src, &dest).map_err(|e| format!("opencode: {e}"))?;
    Ok(format!("plugin -> {}", dest.display()))
}

fn export_claude_env() -> Result<(), String> {
    let path = dirs_home().join(".claude/settings.json");
    // Absent: start from an empty object. Present but unparseable: refuse to
    // write - the file is the user's, and an env-only rewrite would destroy it.
    let mut data: serde_json::Map<String, serde_json::Value> = match std::fs::read_to_string(&path)
    {
        Err(_) => Default::default(),
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "claude env: {} is not valid JSON ({e}); not overwriting",
                path.display()
            )
        })?,
    };
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
    // Same contract as the claude export: absent starts empty, unparseable
    // refuses rather than replacing the user's config with a one-key table.
    let mut document: toml::Value = match std::fs::read_to_string(&path) {
        Err(_) => toml::Value::Table(Default::default()),
        Ok(text) => toml::from_str(&text).map_err(|e| {
            format!(
                "codex env: {} is not valid TOML ({e}); not overwriting",
                path.display()
            )
        })?,
    };
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
    let fresh_line = format!(
        "{RC_MARK}\nexport {BUILD_DIR_KEY}=\"{}\"",
        build_dir_value()
    );
    if let Some(mark_at) = text.find(RC_MARK) {
        // The marked block is ours to keep current: rewrite it in place when
        // the exported value drifted (a later cargo_targets_base change must
        // reach the shell), instead of freezing the first exported path.
        let line_end = text[mark_at..]
            .find('\n')
            .map(|i| mark_at + i)
            .unwrap_or(text.len());
        let value_end = text[line_end + 1..]
            .find('\n')
            .map(|i| line_end + 1 + i)
            .unwrap_or(text.len());
        let mut updated = String::with_capacity(text.len());
        updated.push_str(&text[..mark_at]);
        updated.push_str(&fresh_line);
        updated.push_str(&text[value_end..]);
        return std::fs::write(&rc, updated).ok().map(|_| rc);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&fresh_line);
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

/// `--source` default: the source checkout `fno doctor update` pinned at its
/// last successful install, else the cwd.
fn default_source_dir() -> PathBuf {
    let pin = state_root().join("source-path");
    if let Ok(text) = std::fs::read_to_string(&pin) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    std::env::current_dir().unwrap_or_default()
}

fn check_exit_code(status: &str) -> i32 {
    // Matches fno doctor plugin-file: fresh/absent 0, stale 3, unknown 4.
    match status {
        "stale" => 3,
        "unknown" => 4,
        _ => 0,
    }
}

fn print_check(report: &StageCheck, json: bool) {
    if json {
        match serde_json::to_string(report) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("plugin-install --check: serialization error: {e}"),
        }
        return;
    }
    match report.status {
        "fresh" => println!(
            "plugin stage: fresh (source HEAD {})",
            &report.source_head[..report.source_head.len().min(12)]
        ),
        "stale" => println!(
            "plugin stage: stale ({} differing, {} missing; e.g. {})",
            report.differing_count,
            report.missing_count,
            report.sample.first().map(String::as_str).unwrap_or("?")
        ),
        "absent" => println!("plugin stage: absent"),
        _ => println!(
            "plugin stage: unknown ({})",
            report.detail.as_deref().unwrap_or("no detail")
        ),
    }
}

pub fn run_plugin_install(args: &[String]) -> i32 {
    let mut mode: Option<String> = None;
    let mut source: Option<String> = None;
    let mut stage: Option<String> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--source" => {
                source = args.get(i + 1).cloned();
                i += 2;
            }
            "--stage" => {
                stage = args.get(i + 1).cloned();
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--force" => i += 1,
            "--check" | "--restage" | "--stage-only" | "--env-only" => {
                if mode.is_none() {
                    mode = Some(args[i].clone());
                }
                i += 1;
            }
            other => {
                if mode.is_none() && !other.starts_with('-') {
                    mode = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    match mode.as_deref() {
        // Byte verdict for the stage; doctor's plugin_cache leg calls this.
        Some("--check") => {
            let stage_path = stage
                .map(PathBuf::from)
                .unwrap_or_else(|| state_root().join("plugin-stage").join("fno"));
            let source_dir = source.map(PathBuf::from).unwrap_or_else(default_source_dir);
            let report = check_stage_report(&stage_path, &source_dir);
            print_check(&report, json);
            check_exit_code(report.status)
        }
        // Deploy-only rebuild of an existing stage; fno doctor update calls
        // this after a successful install.
        Some("--restage") => {
            let source_dir = source
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let stage_parent = state_root().join("plugin-stage");
            match restage_stage(&source_dir, &stage_parent) {
                Ok(RestageOutcome::Absent) => {
                    println!("plugin stage: absent, skipped");
                    0
                }
                Ok(RestageOutcome::Restaged {
                    files,
                    root,
                    head12,
                }) => {
                    println!(
                        "plugin stage: restaged {files} files from {} at {head12}",
                        root.display()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("plugin install: {e}");
                    1
                }
            }
        }
        // Stage-only mode: build the filtered stage, print its path, exit. The
        // Python codex arm uses it to hand converge a source_root.
        Some("--stage-only") => {
            let cwd = source
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match repo_root(&cwd).and_then(|root| {
                let parent = state_root().join("plugin-stage");
                build_stage(&root, &parent).map(|(p, _)| p)
            }) {
                Ok(stage) => {
                    println!("{}", stage.display());
                    0
                }
                Err(e) => {
                    eprintln!("plugin install: {e}");
                    1
                }
            }
        }
        // Env-only mode: export the build-dir env to the harness surfaces without
        // an install, so the codex path (converge in Python) exports too.
        Some("--env-only") => {
            env_exports_receipt();
            0
        }
        Some(harness) => {
            let force = args.iter().any(|a| a == "--force");
            let outcome = install_harness(harness, force);
            match outcome {
                Ok(detail) => {
                    println!("plugin install {harness}: {detail}");
                    env_exports_receipt();
                    let stale = remove_stale_copies();
                    if !stale.is_empty() {
                        let joined: Vec<String> =
                            stale.iter().map(|p| p.display().to_string()).collect();
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
        None => {
            eprintln!("usage: fno-agents plugin-install <claude|codex|opencode|agy> [--force]");
            eprintln!("       fno-agents plugin-install --check [--stage <dir>] [--source <dir>] [--json]");
            eprintln!("       fno-agents plugin-install --restage [--source <dir>]");
            2
        }
    }
}

fn install_harness(harness: &str, force: bool) -> Result<String, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = repo_root(&cwd)?;
    let parent = state_root().join("plugin-stage");
    let (stage, _) = build_stage(&root, &parent)?;
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
    let _ = force; // agy imports the stage fresh on every install
    run_checked(
        &[
            "agy".into(),
            "plugin".into(),
            "install".into(),
            stage.display().to_string(),
        ],
        None,
    )?;
    Ok("agy plugin imported, hooks.json carries the stop hook".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// One git command, panicking on failure - fixtures abort the test loudly.
    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git binary available");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit carrying a hook config and one script.
    fn new_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-b", "main", "-q"]);
        fs::create_dir_all(dir.join("hooks"));
        fs::write(
            dir.join("hooks/hooks.json"),
            "{\"hooks\":[{\"command\":\"${CLAUDE_PLUGIN_ROOT}/hooks/live.sh\"}]}",
        )
        .unwrap();
        fs::write(dir.join("hooks/live.sh"), "live\n").unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "c1"]);
    }

    /// A stage built from `source`, with every tracked file present.
    fn fresh_stage(base: &Path) -> (PathBuf, PathBuf) {
        let source = base.join("source");
        new_repo(&source);
        let stage_parent = base.join("stage-parent");
        fs::create_dir_all(&stage_parent).unwrap();
        let (stage, _) = build_stage(&source, &stage_parent).unwrap();
        (source, stage)
    }

    /// AC1-HP: the swap is atomic (no temp dirs left), new bytes land, and a
    /// handle opened before the rebuild still reads the old bytes.
    #[test]
    fn rebuild_swaps_atomically_and_keeps_open_handle_stable() {
        let base = std::env::temp_dir().join(format!("pi-ac1hp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        let live = stage.join("hooks/live.sh");
        let old_handle = fs::File::open(&live).unwrap();

        fs::write(source.join("hooks/live.sh"), "updated\n").unwrap();
        git_in(&source, &["add", "-A"]);
        git_in(&source, &["commit", "-q", "-m", "c2"]);

        let stage_parent = stage.parent().unwrap().to_path_buf();
        let (_, copied) = build_stage(&source, &stage_parent).unwrap();
        assert!(copied >= 2);
        assert_eq!(fs::read_to_string(&live).unwrap(), "updated\n");
        let mut old_text = String::new();
        use std::io::Read;
        let mut old_handle2 = old_handle;
        old_handle2.read_to_string(&mut old_text).unwrap();
        assert_eq!(old_text, "live\n");
        let leftovers: Vec<_> = fs::read_dir(&stage_parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".fno.new-") || n.starts_with(".fno.old-"))
            .collect();
        assert!(leftovers.is_empty(), "temp dirs left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&base);
    }

    /// AC1-ERR: a referenced script the new tree deleted is carried forward
    /// from the old stage with the old bytes.
    #[test]
    fn rebuild_carries_referenced_script_forward() {
        let base = std::env::temp_dir().join(format!("pi-ac1err-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        // A pre-restage session's hook config still names gone.sh, and the
        // old stage still has it.
        let gone = stage.join("hooks/gone.sh");
        fs::write(&gone, "old bytes\n").unwrap();
        // The source tree no longer has it (deleted in a later merge).
        let stage_parent = stage.parent().unwrap().to_path_buf();
        let (stage2, _) = build_stage(&source, &stage_parent).unwrap();
        assert_eq!(stage2, stage);
        // gone.sh is NOT referenced by the current config, so it is gone.
        assert!(!stage2.join("hooks/gone.sh").exists());
        // Now make the OLD config reference it and rebuild: the carry-forward
        // reads hooks/hooks.json from the old stage, so the referenced script
        // survives the swap even though HEAD lacks it.
        let _ = fs::remove_dir_all(&stage2);
        fs::create_dir_all(stage2.join("hooks")).unwrap();
        fs::write(
            stage2.join("hooks/hooks.json"),
            "{\"hooks\":[{\"command\":\"${CLAUDE_PLUGIN_ROOT}/hooks/gone.sh ${CLAUDE_PLUGIN_ROOT}/hooks/live.sh\"}]}",
        )
        .unwrap();
        fs::write(stage2.join("hooks/gone.sh"), "old bytes\n").unwrap();
        fs::write(stage2.join("hooks/live.sh"), "live\n").unwrap();
        let (stage3, _) = build_stage(&source, &stage_parent).unwrap();
        assert_eq!(
            fs::read_to_string(stage3.join("hooks/gone.sh")).unwrap(),
            "old bytes\n"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// AC2-HP: fresh right after a build; stale with a named sample after a
    /// staged file is edited.
    #[test]
    fn check_reports_fresh_then_stale_with_sample() {
        let base = std::env::temp_dir().join(format!("pi-ac2hp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        let report = check_stage_report(&stage, &source);
        assert_eq!(report.status, "fresh");
        assert_eq!(report.differing_count, 0);
        assert_eq!(report.missing_count, 0);
        assert!(!report.source_head.is_empty());

        fs::write(stage.join("hooks/live.sh"), "tampered\n").unwrap();
        let report = check_stage_report(&stage, &source);
        assert_eq!(report.status, "stale");
        assert_eq!(report.differing_count, 1);
        assert_eq!(report.sample, vec!["hooks/live.sh".to_string()]);
        assert!(report.remedy.contains("fno config plugin install claude"));
        let _ = fs::remove_dir_all(&base);
    }

    /// AC2-ERR: a missing stage reads absent; a non-git source reads unknown
    /// with the failed instrument in detail.
    #[test]
    fn check_reports_absent_and_unknown() {
        let base = std::env::temp_dir().join(format!("pi-ac2err-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        new_repo(&source);
        let report = check_stage_report(&base.join("no-such-stage"), &source);
        assert_eq!(report.status, "absent");

        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        // The stage dir must EXIST for the unknown case to be reachable; an
        // absent stage short-circuits to absent first.
        fs::create_dir_all(base.join("stage-parent/fno")).unwrap();
        let report = check_stage_report(&base.join("stage-parent/fno"), &plain);
        assert_eq!(report.status, "unknown");
        assert!(report.detail.as_deref().unwrap_or("").contains("git"));
        let _ = fs::remove_dir_all(&base);
    }

    /// AC3-HP: restage rebuilds a stale stage; the receipt names count+head.
    #[test]
    fn restage_rebuilds_stale_stage() {
        let base = std::env::temp_dir().join(format!("pi-ac3hp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        fs::write(stage.join("hooks/live.sh"), "tampered\n").unwrap();
        assert_eq!(check_stage_report(&stage, &source).status, "stale");
        let stage_parent = stage.parent().unwrap().to_path_buf();
        match restage_stage(&source, &stage_parent).unwrap() {
            RestageOutcome::Restaged { files, head12, .. } => {
                assert!(files >= 2);
                assert_eq!(head12.len(), 12);
            }
            other => panic!("expected Restaged, got {other:?}"),
        }
        assert_eq!(check_stage_report(&stage, &source).status, "fresh");
        let _ = fs::remove_dir_all(&base);
    }

    /// AC3-ERR: --restage on a machine with no stage creates nothing.
    #[test]
    fn restage_skips_absent_stage() {
        let base = std::env::temp_dir().join(format!("pi-ac3err-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        new_repo(&source);
        let stage_parent = base.join("stage-parent");
        match restage_stage(&source, &stage_parent).unwrap() {
            RestageOutcome::Absent => {}
            other => panic!("expected Absent, got {other:?}"),
        }
        assert!(!stage_parent.join("fno").exists());
        let _ = fs::remove_dir_all(&base);
    }
}
