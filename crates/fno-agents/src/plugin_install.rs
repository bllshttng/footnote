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
use serde_json::Value;

use crate::paths::{dirs_home, AgentsHome};

const BUILD_DIR_KEY: &str = "CARGO_BUILD_BUILD_DIR";
const RC_MARK: &str = "# fno: cargo build-dir";

/// Same shape as the Python gate in cli/src/fno/hook_config.py:29.
const HOOK_REF_PATTERN: &str =
    r#"\$\{(?:CLAUDE_PLUGIN_ROOT|CODEX_PLUGIN_ROOT|PLUGIN_ROOT)\}/([^\s"'\\]+)"#;

pub(crate) fn state_root() -> PathBuf {
    std::env::var_os("FNO_RECLAIM_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".fno"))
}

fn build_dir_value() -> String {
    // Through the one base resolver, so an operator's
    // `paths.cargo_targets_base` reaches the exported rc env too (it was
    // ignored here); the FNO_RECLAIM_STATE_ROOT test seam still lands via the
    // state fallback inside fno_build_base.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    format!(
        "{}/{{workspace-path-hash}}",
        crate::cargo_build_dirs::fno_build_base(&cwd).display()
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
    for config in [
        "hooks/hooks.json",
        "hooks/codex-hooks.json",
        "hooks/context-hooks.json",
    ] {
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

/// The marketplace manifest path, relative to the stage root.
const MARKETPLACE_REL: &str = ".claude-plugin/marketplace.json";

/// The stage serves its own copy of the public manifest with the fno entry
/// pointed at the stage root (`source: "./"`), so `claude plugin install
/// fno@footnote` resolves in place and never clones a GitHub ref: the public
/// pins (`stable`, `nightly`) are release-side refs a dev machine cannot rely
/// on. The repo file stays verbatim. Anything that would leave the fno entry
/// unserved is Err: a verbatim copy would silently resurrect the clone trap.
fn staged_marketplace_bytes(source_bytes: &str) -> Result<String, String> {
    let mut manifest: Value =
        serde_json::from_str(source_bytes).map_err(|e| format!("{MARKETPLACE_REL}: {e}"))?;
    let Some(entries) = manifest.get_mut("plugins").and_then(Value::as_array_mut) else {
        return Err(format!(
            "{MARKETPLACE_REL}: no plugins array; cannot serve the fno entry locally"
        ));
    };
    let mut rewritten = false;
    for entry in entries.iter_mut() {
        if entry.get("name").and_then(Value::as_str) == Some("fno") {
            if !entry.is_object() {
                return Err(format!(
                    "{MARKETPLACE_REL}: the fno entry is not an object; cannot serve it locally"
                ));
            }
            entry["source"] = json!("./");
            rewritten = true;
        }
    }
    if !rewritten {
        return Err(format!(
            "{MARKETPLACE_REL}: no fno entry; cannot serve the plugin locally"
        ));
    }
    serde_json::to_string_pretty(&manifest).map_err(|e| format!("{MARKETPLACE_REL}: {e}"))
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

    let manifest = new_dir.join(MARKETPLACE_REL);
    if manifest.is_file() {
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("stage: {MARKETPLACE_REL}: {e}"))?;
        let rewritten = staged_marketplace_bytes(&text).map_err(|e| format!("stage: {e}"))?;
        std::fs::write(&manifest, rewritten)
            .map_err(|e| format!("stage: {MARKETPLACE_REL}: {e}"))?;
    }

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
    let mut has_manifest = false;
    for (path, sha) in &tracked {
        if *path == MARKETPLACE_REL {
            // The stage serves a rewritten copy of this file
            // (staged_marketplace_bytes), so no HEAD sha can match it.
            has_manifest = true;
            continue;
        }
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

    if has_manifest {
        let stage_manifest = stage.join(MARKETPLACE_REL);
        if !stage_manifest.is_file() {
            missing.push(MARKETPLACE_REL.to_string());
        } else {
            let verdict = std::fs::read_to_string(root.join(MARKETPLACE_REL))
                .map_err(|e| format!("{MARKETPLACE_REL}: {e}"))
                .and_then(|t| staged_marketplace_bytes(&t))
                .and_then(|expected| {
                    std::fs::read_to_string(&stage_manifest)
                        .map(|actual| actual == expected)
                        .map_err(|e| format!("{MARKETPLACE_REL}: {e}"))
                });
            match verdict {
                Err(e) => return unknown_check(stage, source_dir, e),
                Ok(false) => differing.push(MARKETPLACE_REL.to_string()),
                Ok(true) => {}
            }
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

/// One fno plugin root on disk: a tree that looks like (or is recorded as) an
/// installed copy of this plugin.
#[derive(Debug, Clone, Serialize)]
struct PluginRoot {
    path: PathBuf,
    origin: &'static str,
    live: bool,
}

/// Canonical-equality path compare; falls back to raw equality when either
/// side cannot be canonicalized (a not-yet-created path).
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

fn push_root(
    roots: &mut Vec<PluginRoot>,
    seen: &mut Vec<PathBuf>,
    path: PathBuf,
    origin: &'static str,
    live: bool,
) {
    if !path.exists() {
        return;
    }
    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.contains(&key) {
        return;
    }
    seen.push(key);
    roots.push(PluginRoot { path, origin, live });
}

/// The footnote marketplace's `source.source` kind ("directory", "file",
/// "github", ...). None when the registry is unreadable or names no footnote
/// marketplace.
fn marketplace_source_kind(home: &Path) -> Option<String> {
    let text =
        std::fs::read_to_string(home.join(".claude/plugins/known_marketplaces.json")).ok()?;
    let v = serde_json::from_str::<Value>(&text).ok()?;
    v.get("footnote")?
        .get("source")?
        .get("source")?
        .as_str()
        .map(String::from)
}

/// Every fno plugin root this machine could load or mistake for the loaded
/// one, plus one detail line per registry file that could not be read. A
/// path that does not exist on disk is never reported, and an unreadable
/// registry never contributes a root reported as fresh.
///
/// Live means "the harness loads this tree in place": Claude records a local
/// (directory or file) marketplace's own path as its install location and
/// execs from there, so the marketplace root is live exactly when the
/// marketplace shape is local. `installPath` in installed_plugins.json is a
/// recorded string contradicted by that behaviour, never live; nor are the
/// orphan copies.
fn plugin_roots_for(home: &Path) -> (Vec<PluginRoot>, Vec<String>) {
    let mut roots: Vec<PluginRoot> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut detail: Vec<String> = Vec::new();

    let marketplaces = home.join(".claude/plugins/known_marketplaces.json");
    match std::fs::read_to_string(&marketplaces)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
    {
        Err(e) => detail.push(format!("{} unreadable: {e}", marketplaces.display())),
        Ok(v) => match (
            v.get("footnote"),
            v.get("footnote").and_then(|f| f.get("source")),
        ) {
            (Some(rec), Some(source)) => {
                let local = matches!(
                    source.get("source").and_then(Value::as_str),
                    Some("directory" | "file")
                );
                let path = rec
                    .get("installLocation")
                    .and_then(Value::as_str)
                    .or_else(|| source.get("path").and_then(Value::as_str));
                if let Some(p) = path {
                    push_root(
                        &mut roots,
                        &mut seen,
                        PathBuf::from(p),
                        "marketplace",
                        local,
                    );
                }
            }
            _ => detail.push(format!(
                "{} names no footnote marketplace",
                marketplaces.display()
            )),
        },
    }

    let registry = home.join(".claude/plugins/installed_plugins.json");
    match std::fs::read_to_string(&registry)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
    {
        Err(e) => detail.push(format!("{} unreadable: {e}", registry.display())),
        Ok(v) => {
            let entries = v
                .get("plugins")
                .and_then(|p| p.get("fno@footnote"))
                .and_then(Value::as_array);
            match entries {
                None => detail.push(format!(
                    "{} names no fno@footnote entry",
                    registry.display()
                )),
                Some(list) => {
                    for entry in list {
                        if let Some(p) = entry.get("installPath").and_then(Value::as_str) {
                            push_root(&mut roots, &mut seen, PathBuf::from(p), "registry", false);
                        }
                    }
                }
            }
        }
    }

    for path in [
        home.join(".gemini/config/plugins/footnote"),
        home.join(".codex/plugins/cache/footnote-local"),
        home.join(".claude/plugins/cache/footnote"),
    ] {
        push_root(&mut roots, &mut seen, path, "orphan", false);
    }

    (roots, detail)
}

fn plugin_roots() -> (Vec<PluginRoot>, Vec<String>) {
    plugin_roots_for(&dirs_home())
}

/// A single root's verdict carrying the role it played in the enumeration,
/// plus the rendered one-line note and blocker text the doctor printer and
/// blocker list print verbatim (the Python side is transport only).
#[derive(Serialize)]
struct RootVerdict {
    #[serde(flatten)]
    check: StageCheck,
    path: String,
    origin: &'static str,
    live: bool,
    kind: &'static str,
    note: String,
    blocker: Option<String>,
}

/// The doctor-shaped fold: flat keys pointed at the live root, status the
/// worst across roots, and the per-root detail under `roots`.
#[derive(Serialize)]
struct RootsReport {
    status: String,
    sha: Option<String>,
    installed_at: Option<String>,
    kind: Option<&'static str>,
    stage: Option<String>,
    remedy: Option<String>,
    detail: Option<String>,
    roots: Vec<RootVerdict>,
    enumeration_detail: Vec<String>,
}

fn root_note(live: bool, drift: usize, remedy: &str) -> String {
    let mut note = format!(" ({drift} file(s) differ from source HEAD)");
    if drift == 0 {
        note.clear();
    }
    if !live && drift > 0 {
        let remedy = if remedy.is_empty() {
            "fno config plugin install claude"
        } else {
            remedy
        };
        note.push_str(&format!(". Fix: {remedy} removes the stale second copy"));
    }
    note
}

/// Byte verdict for EVERY enumerated root against source HEAD. The exit code
/// is the worst status across roots, so one stale root exits 3 even when the
/// live root is fresh.
fn check_roots_report(
    scan: &[PluginRoot],
    detail: Vec<String>,
    source_dir: &Path,
) -> (RootsReport, i32) {
    // Worst status reads stale above unknown: stale carries a remedy and the
    // doctor exit gate keys on it, while unknown only names its gap.
    fn rank(status: &str) -> u8 {
        match status {
            "stale" => 2,
            "unknown" => 1,
            _ => 0,
        }
    }
    let mut roots = Vec::new();
    let mut worst: Option<(u8, &'static str)> = None;
    for root in scan {
        let check = check_stage_report(&root.path, source_dir);
        let drift = check.differing_count + check.missing_count;
        let blocker = if check.status == "stale" {
            let role = if root.live { "live" } else { "second copy" };
            Some(format!(
                "plugin root {} ({}) differs from source HEAD in {} file(s) (e.g. {}). Fix: {}",
                root.path.display(),
                role,
                drift,
                check.sample.first().map(String::as_str).unwrap_or("?"),
                check.remedy
            ))
        } else {
            None
        };
        let note = root_note(root.live, drift, &check.remedy);
        let last_status = check.status;
        roots.push(RootVerdict {
            path: root.path.display().to_string(),
            origin: root.origin,
            live: root.live,
            kind: "stage",
            note,
            blocker,
            check,
        });
        let rank_cur = rank(last_status);
        if worst.map_or(true, |w| rank_cur > w.0) {
            worst = Some((rank_cur, last_status));
        }
    }
    let live = roots.iter().find(|r| r.live);
    let joined = if detail.is_empty() {
        None
    } else {
        Some(detail.join("; "))
    };
    let report = RootsReport {
        status: worst.map(|w| w.1).unwrap_or("unknown").to_string(),
        sha: live.map(|r| r.check.source_head.clone()),
        installed_at: None,
        kind: if live.is_some() || roots.is_empty() {
            Some("stage")
        } else {
            None
        },
        stage: live.map(|r| r.path.clone()),
        remedy: live.map(|r| r.check.remedy.clone()),
        detail: joined,
        enumeration_detail: detail,
        roots,
    };
    let exit = worst.map(|w| check_exit_code(w.1)).unwrap_or(4);
    (report, exit)
}

fn print_roots_report(report: &RootsReport) {
    for line in &report.enumeration_detail {
        println!("plugin root: {line}");
    }
    for root in &report.roots {
        let role = if root.live { "live" } else { "second copy" };
        println!("plugin root ({}, {role}): {}", root.origin, root.path);
        print_check(&root.check, false);
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
        // update keeps same-version files; force means delete and reinstall,
        // and also covers not-yet-installed, so uninstall failure is fine.
        let _ = run_checked(
            &[
                "claude".into(),
                "plugin".into(),
                "uninstall".into(),
                "fno@footnote".into(),
            ],
            None,
        );
        run_checked(
            &[
                "claude".into(),
                "plugin".into(),
                "install".into(),
                "fno@footnote".into(),
            ],
            None,
        )?;
        return Ok(format!("reinstalled fno@footnote from {}", stage.display()));
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
    let receipt = crate::opencode_install::install(&std::env::current_dir().unwrap_or_default())?;
    Ok(format!(
        "{} (footnote {} -> {})",
        receipt.status, receipt.version, receipt.config_dir
    ))
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

/// Remove the second copies a successful install leaves behind. The gemini
/// and codex-dev-channel copies are unconditional. The Claude cache copy is
/// guarded: removed only when the marketplace loads in place (directory or
/// file shape) and a live root was proven and that live root is not the
/// cache itself. `home` and `roots` come from the caller's scan, so the
/// removal and any report can never disagree about which root is live.
fn remove_stale_copies(home: &Path, roots: &[PluginRoot]) -> (Vec<PathBuf>, Vec<String>) {
    let mut removed = Vec::new();
    let mut refused = Vec::new();
    for path in [
        home.join(".gemini/config/plugins/footnote"),
        home.join(".codex/plugins/cache/footnote-local"),
    ] {
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
            removed.push(path);
        }
    }
    let cache = home.join(".claude/plugins/cache/footnote");
    if !cache.exists() {
        return (removed, refused);
    }
    let live_roots: Vec<&PluginRoot> = roots.iter().filter(|r| r.live).collect();
    let cache_is_live = live_roots.iter().any(|r| paths_equal(&r.path, &cache));
    match marketplace_source_kind(home) {
        None => refused.push(format!(
            "claude cache {} kept: footnote marketplace shape unreadable",
            cache.display()
        )),
        Some(kind) if kind != "directory" && kind != "file" => refused.push(format!(
            "claude cache {} kept: marketplace source is '{kind}', not a local directory",
            cache.display()
        )),
        _ if cache_is_live => refused.push(format!(
            "claude cache {} kept: it is the live root",
            cache.display()
        )),
        _ if live_roots.is_empty() => refused.push(format!(
            "claude cache {} kept: no live root proven on this machine",
            cache.display()
        )),
        _ => {
            let _ = std::fs::remove_dir_all(&cache);
            removed.push(cache);
        }
    }
    (removed, refused)
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

struct PluginInstallArgs {
    mode: Option<String>,
    source: Option<String>,
    stage: Option<String>,
    json: bool,
    force: bool,
    uninstall: bool,
    status: bool,
    quick: bool,
    hooks: bool,
    hooks_status: bool,
    adapter: Option<String>,
    crown: Option<String>,
    hooks_file: Option<String>,
    extension_src: Option<String>,
}

fn parse_plugin_install_args(args: &[String]) -> PluginInstallArgs {
    let mut parsed = PluginInstallArgs {
        mode: None,
        source: None,
        stage: None,
        json: false,
        force: false,
        uninstall: false,
        status: false,
        quick: false,
        hooks: false,
        hooks_status: false,
        adapter: None,
        crown: None,
        hooks_file: None,
        extension_src: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--source" => {
                parsed.source = args.get(i + 1).cloned();
                i += 2;
            }
            "--stage" => {
                parsed.stage = args.get(i + 1).cloned();
                i += 2;
            }
            "--adapter" => {
                parsed.adapter = args.get(i + 1).cloned();
                i += 2;
            }
            "--crown" => {
                parsed.crown = args.get(i + 1).cloned();
                i += 2;
            }
            "--hooks-file" => {
                parsed.hooks_file = args.get(i + 1).cloned();
                i += 2;
            }
            "--extension-src" => {
                parsed.extension_src = args.get(i + 1).cloned();
                i += 2;
            }
            "--json" | "-J" => {
                parsed.json = true;
                i += 1;
            }
            "--force" => {
                parsed.force = true;
                i += 1;
            }
            "--uninstall" => {
                parsed.uninstall = true;
                i += 1;
            }
            "--status" => {
                parsed.status = true;
                i += 1;
            }
            "--installed" => {
                parsed.quick = true;
                i += 1;
            }
            "--hooks" => {
                parsed.hooks = true;
                i += 1;
            }
            "--hooks-status" => {
                parsed.hooks_status = true;
                i += 1;
            }
            "--check" | "--restage" | "--stage-only" | "--env-only" => {
                if parsed.mode.is_none() {
                    parsed.mode = Some(args[i].clone());
                }
                i += 1;
            }
            other => {
                if parsed.mode.is_none() && !other.starts_with('-') {
                    parsed.mode = Some(other.to_string());
                }
                i += 1;
            }
        }
    }
    parsed
}

pub fn run_plugin_install(args: &[String]) -> i32 {
    let PluginInstallArgs {
        mode,
        source,
        stage,
        json,
        force,
        uninstall,
        status,
        quick,
        hooks,
        hooks_status,
        adapter,
        crown,
        hooks_file,
        extension_src,
    } = parse_plugin_install_args(args);
    if hooks || hooks_status {
        return run_agy_hooks(
            mode.as_deref(),
            hooks,
            hooks_status,
            adapter.as_deref(),
            crown.as_deref(),
            hooks_file.as_deref(),
            json,
        );
    }
    match mode.as_deref() {
        // Byte verdict for the stage; doctor's plugin_cache leg calls this.
        // With no --stage, EVERY enumerated root is checked and the exit code
        // is the worst status across roots.
        Some("--check") => {
            let source_dir = source.map(PathBuf::from).unwrap_or_else(default_source_dir);
            if let Some(dir) = stage {
                let stage_path = PathBuf::from(dir);
                let report = check_stage_report(&stage_path, &source_dir);
                print_check(&report, json);
                check_exit_code(report.status)
            } else {
                let (roots, detail) = plugin_roots();
                let (report, worst) = check_roots_report(&roots, detail, &source_dir);
                if json {
                    match serde_json::to_string(&report) {
                        Ok(s) => println!("{s}"),
                        Err(e) => {
                            eprintln!("plugin-install --check: serialization error: {e}")
                        }
                    }
                } else {
                    print_roots_report(&report);
                }
                worst
            }
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
            if harness == "opencode" {
                return run_opencode_arm(harness, json, uninstall, status, quick);
            }
            if harness == "grok" && status {
                let receipt = grok_status_receipt();
                println!("{receipt}");
                return if receipt.starts_with("unknown") { 1 } else { 0 };
            }
            if harness == "pi" {
                return run_pi_arm(status, json, extension_src.as_deref());
            }
            if uninstall || status || quick {
                eprintln!(
                    "plugin install: --uninstall/--status/--installed apply to the opencode or grok arms"
                );
                return 2;
            }
            let outcome = install_harness(harness, force);
            match outcome {
                Ok(detail) => {
                    println!("plugin install {harness}: {detail}");
                    env_exports_receipt();
                    let (roots, _) = plugin_roots();
                    let (stale, refused) = remove_stale_copies(&dirs_home(), &roots);
                    for line in refused {
                        println!("{line}");
                    }
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
            eprintln!(
                "usage: fno-agents plugin-install <claude|codex|opencode|agy|grok> [--force]"
            );
            eprintln!("       fno-agents plugin-install --check [--stage <dir>] [--source <dir>] [--json|-J]");
            eprintln!("         (no --stage checks every plugin root; exit is the worst status across roots)");
            eprintln!("       fno-agents plugin-install --restage [--source <dir>]");
            2
        }
    }
}

/// The opencode arm: one install/uninstall/status door onto
/// `opencode_install`, plus the shared env exports and stale-copy sweep the
/// other harness arms run. `--json` keeps stdout to the receipt alone (the
/// Python door parses it), so the prose side lines move to stderr there.
fn run_opencode_arm(_harness: &str, json: bool, uninstall: bool, status: bool, quick: bool) -> i32 {
    let say = |line: String| {
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    if uninstall {
        return match crate::opencode_install::uninstall() {
            Ok(receipt) => {
                if json {
                    println!("{}", serde_json::to_string(&receipt).unwrap_or_default());
                } else {
                    println!(
                        "plugin uninstall opencode: {} ({} file(s) removed from {})",
                        receipt.status, receipt.removed, receipt.config_dir
                    );
                    for rel in &receipt.kept {
                        println!(
                            "kept user file: {rel} (its bytes changed since the install; \
                             remove it by hand if it is yours to remove)"
                        );
                    }
                }
                if receipt.kept.is_empty() {
                    0
                } else {
                    3
                }
            }
            Err(e) => {
                eprintln!("plugin uninstall opencode FAILED: {e}");
                1
            }
        };
    }
    if status {
        let value = crate::opencode_install::status_json();
        if json {
            println!("{value}");
        } else {
            print_status_prose(&value);
        }
        return 0;
    }
    if quick {
        let value = crate::opencode_install::installed_status();
        if json {
            println!("{value}");
        } else {
            println!("opencode install: {}", value["status"]);
        }
        return 0;
    }
    match crate::opencode_install::install(&std::env::current_dir().unwrap_or_default()) {
        Ok(receipt) => {
            if json {
                println!("{}", serde_json::to_string(&receipt).unwrap_or_default());
            } else {
                println!(
                    "plugin install opencode: {} (footnote {} -> {})",
                    receipt.status, receipt.version, receipt.config_dir
                );
                for rel in &receipt.kept {
                    println!("kept user file: {rel} (footnote did not overwrite it)");
                }
            }
            for (line, is_error) in env_exports_lines() {
                if is_error {
                    eprintln!("{line}");
                } else {
                    say(line);
                }
            }
            let (roots, _) = plugin_roots();
            let (stale, refused) = remove_stale_copies(&dirs_home(), &roots);
            for line in refused {
                say(line);
            }
            if !stale.is_empty() {
                let joined: Vec<String> = stale.iter().map(|p| p.display().to_string()).collect();
                say(format!("removed stale copies: {}", joined.join(", ")));
            }
            let home = AgentsHome::from_env();
            let _ = crate::reclaim::run_reclaim(&["--apply".to_string()], &home);
            if receipt.status == "partial" {
                3
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("plugin install opencode FAILED: {e}");
            1
        }
    }
}

fn print_status_prose(value: &serde_json::Value) {
    let status = value["status"].as_str().unwrap_or("unknown");
    match status {
        "installed" => println!(
            "opencode: installed (footnote {}): {} command(s), {} agent(s), {} skill(s)",
            value["version"].as_str().unwrap_or("?"),
            value["installed"]["commands"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            value["installed"]["agents"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            value["installed"]["skills"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
        ),
        "absent" => println!(
            "opencode: not installed (bridge file {}); \
             install with `fno config plugin install opencode`",
            if value["bridge_present"].as_bool() == Some(true) {
                "present, legacy"
            } else {
                "absent"
            }
        ),
        _ => {
            println!(
                "opencode: PARTIAL: {} name(s) installed but not loaded: {}",
                value["missing"].as_array().map(Vec::len).unwrap_or(0),
                value["missing"]
                    .as_array()
                    .map(|names| names
                        .iter()
                        .filter_map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default()
            );
            let stale = value["stale"].as_array().map(Vec::len).unwrap_or(0);
            if stale > 0 {
                println!(
                    "opencode: {} loaded name(s) footnote's manifest does not know: {}",
                    stale,
                    value["stale"]
                        .as_array()
                        .map(|names| names
                            .iter()
                            .filter_map(|n| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default()
                );
            }
        }
    }
}

/// The pi arm: install the loop extension into pi's agent dir (honoring
/// `PI_CODING_AGENT_DIR`), or read the install status. Neither path builds
/// the plugin stage or needs a repo checkout, so a relocated pi install
/// works from the binary alone.
///
/// - `--extension-src <path>` (install): copy the source to
///   `<agent dir>/extensions/footnote.ts` through a temp file in the same
///   directory and a rename, so a half-written extension is never loadable.
/// - `--status`: answer `{"installed", "dest", "agent_dir_source", "skills"}`;
///   `installed` means the dest exists and, when a source was named, is
///   byte-equal to it. `skills` reads the same plugin-root pointer the verb
///   renderer reads.
fn run_pi_arm(status: bool, json: bool, extension_src: Option<&str>) -> i32 {
    let dest = crate::pi::pi_agent_dir()
        .join("extensions")
        .join("footnote.ts");
    let agent_dir_source = match std::env::var("PI_CODING_AGENT_DIR") {
        Ok(v) if !v.trim().is_empty() => "env",
        _ => "default",
    };
    let skills = match crate::provider::plugin_root() {
        Some(root) if root.join("skills").is_dir() => format!("{}/skills", root.display()),
        Some(root) => format!("unresolved: {} names no skills dir", root.display()),
        None => "unresolved: no plugin-root pointer".to_string(),
    };
    if status {
        let mut installed = dest.is_file();
        if installed {
            if let Some(src) = extension_src {
                installed = files_byte_equal(Path::new(src), &dest).unwrap_or(false);
            }
        }
        print_pi_receipt(installed, &dest, agent_dir_source, &skills, json);
        return 0;
    }
    let Some(src) = extension_src else {
        eprintln!("plugin install pi: --extension-src <path> is required (or pass --status)");
        return 2;
    };
    let src_path = Path::new(src);
    if !src_path.is_file() {
        eprintln!(
            "plugin install pi: extension source {} is not a file",
            src_path.display()
        );
        return 1;
    }
    let dir = dest.parent().unwrap_or_else(|| Path::new("/"));
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("plugin install pi: cannot create {}: {e}", dir.display());
        return 1;
    }
    // Same-directory temp + rename: a reader (pi loading its extensions)
    // never sees a partial copy.
    let tmp = dir.join(format!(".footnote.ts.{}.tmp", std::process::id()));
    if let Err(e) = std::fs::copy(src_path, &tmp) {
        eprintln!("plugin install pi: cannot stage the copy: {e}");
        let _ = std::fs::remove_file(&tmp);
        return 1;
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        eprintln!("plugin install pi: cannot finalize {}: {e}", dest.display());
        let _ = std::fs::remove_file(&tmp);
        return 1;
    }
    print_pi_receipt(true, &dest, agent_dir_source, &skills, json);
    0
}

fn print_pi_receipt(
    installed: bool,
    dest: &Path,
    agent_dir_source: &str,
    skills: &str,
    json: bool,
) {
    if json {
        // cli/label/status/note name the setup wizard's receipt fields, so
        // the Python door relays the answer instead of translating it.
        let answer = serde_json::json!({
            "installed": installed,
            "cli": "pi",
            "label": "pi",
            "status": if installed { "installed" } else { "failed" },
            "note": format!(
                "extension -> {} (agent dir from {}, skills: {})",
                dest.display(),
                agent_dir_source,
                skills
            ),
            "dest": dest.display().to_string(),
            "agent_dir_source": agent_dir_source,
            "skills": skills,
        });
        println!("{answer}");
    } else {
        println!(
            "pi extension {}: {} (agent dir from {}, skills: {})",
            if installed { "installed" } else { "absent" },
            dest.display(),
            agent_dir_source,
            skills
        );
    }
}

fn files_byte_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    if a == b {
        return Ok(true);
    }
    let mut left = std::fs::File::open(a)?;
    let mut right = std::fs::File::open(b)?;
    let mut la = [0u8; 8192];
    let mut rb = [0u8; 8192];
    loop {
        let n = left.read(&mut la)?;
        let m = right.read(&mut rb)?;
        if n != m || la[..n] != rb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
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
        "grok" => install_grok(&stage, force)?,
        other => {
            return Err(format!(
                "unknown harness '{other}'; want claude, codex, opencode, agy or grok"
            ))
        }
    };
    Ok(detail)
}

/// The env exports as (line, is_error) pairs, so an arm that must keep
/// stdout to a JSON receipt can still run them with everything on stderr.
fn env_exports_lines() -> Vec<(String, bool)> {
    let mut lines: Vec<(String, bool)> = Vec::new();
    if let Err(e) = export_claude_env() {
        lines.push((format!("fno plugin install: {e}"), true));
    }
    if let Err(e) = export_codex_env() {
        lines.push((format!("fno plugin install: {e}"), true));
    }
    let rc = export_rc_env();
    lines.push((
        format!(
            "build-dir env exported to: claude settings env; codex shell_environment_policy; rc ({})",
            rc.map(|p| p.display().to_string())
                .unwrap_or_else(|| "skipped".into())
        ),
        false,
    ));
    lines
}

fn env_exports_receipt() {
    for (line, is_error) in env_exports_lines() {
        if is_error {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
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
    // The old receipt CLAIMED "hooks.json carries the stop hook" without
    // reading it. Report the real state of the global file against the
    // stage's own shipped adapters instead.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(home) = home else {
        return Ok("agy plugin imported; hooks.json status unknown (no HOME)".to_string());
    };
    let hooks = home.join(".gemini").join("config").join("hooks.json");
    let adapter = stage.join("hooks").join("agy-target-stop-hook.sh");
    let crown = stage.join("hooks").join("agy-crown-inject.sh");
    let s = crate::agy_hooks::status(
        &hooks,
        adapter.is_file().then_some(adapter.as_path()),
        crown.is_file().then_some(crown.as_path()),
    );
    Ok(format!("agy plugin imported; {}", s.summary()))
}

/// The `plugin-install agy --hooks / --hooks-status` arm: an agy hooks.json
/// read or preserving-install that builds no plugin stage and needs no repo
/// checkout. Exit 0 on success; exit 1 on an install refusal (message on
/// stderr); exit 2 on a usage fault (missing adapter, no HOME, flags on a
/// non-agy harness).
fn run_agy_hooks(
    harness: Option<&str>,
    _hooks: bool,
    status_flag: bool,
    adapter: Option<&str>,
    crown: Option<&str>,
    hooks_file: Option<&str>,
    json: bool,
) -> i32 {
    if harness != Some("agy") {
        eprintln!("plugin install: --hooks/--hooks-status apply to the agy arm; name it: plugin-install agy --hooks");
        return 2;
    }
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("plugin install agy --hooks: no HOME; cannot resolve the hooks file");
        return 2;
    };
    let hooks_path = match hooks_file {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(home)
            .join(".gemini")
            .join("config")
            .join("hooks.json"),
    };
    if status_flag {
        let adapter = adapter.map(PathBuf::from);
        let crown = crown.map(PathBuf::from);
        let s = crate::agy_hooks::status(&hooks_path, adapter.as_deref(), crown.as_deref());
        if json {
            match serde_json::to_string(&s) {
                Ok(text) => println!("{text}"),
                Err(e) => {
                    eprintln!("plugin install agy --hooks-status: serialization error: {e}");
                    return 1;
                }
            }
        } else {
            println!("agy hooks {}: {}", hooks_path.display(), s.summary());
        }
        return 0;
    }
    // --hooks (install)
    let Some(adapter) = adapter else {
        eprintln!("plugin install agy --hooks: --adapter <path> is required");
        return 2;
    };
    let crown = crown.map(PathBuf::from);
    match crate::agy_hooks::install(&hooks_path, Path::new(adapter), crown.as_deref()) {
        Ok(receipt) => {
            if json {
                match serde_json::to_string(&receipt) {
                    Ok(text) => println!("{text}"),
                    Err(e) => {
                        eprintln!("plugin install agy --hooks: serialization error: {e}");
                        return 1;
                    }
                }
            } else {
                println!("{}", receipt.note);
            }
            0
        }
        Err(reason) => {
            eprintln!("plugin install agy --hooks refused: {reason}");
            1
        }
    }
}

/// Read whether footnote's hooks reach grok on THIS machine: run
/// `grok inspect --json` (no auth needed) and look for an enabled fno plugin
/// with hooks. Reports `reachable` (naming the plugin path), `absent`, or
/// `unknown` (grok missing, inspect failed, or unparseable output).
fn grok_status_receipt() -> String {
    match grok_reachability() {
        GrokReachability::Reachable { path } => format!("reachable: {path}"),
        GrokReachability::Absent => "absent: no enabled fno plugin with hooks".to_string(),
        GrokReachability::Unknown { reason } => format!("unknown: {reason}"),
    }
}

#[derive(Debug)]
enum GrokReachability {
    Reachable { path: String },
    Absent,
    Unknown { reason: String },
}

/// The 30-second-bounded `grok inspect --json` read, classified.
fn grok_reachability() -> GrokReachability {
    match which_grok() {
        None => GrokReachability::Unknown {
            reason: "grok not found on PATH".to_string(),
        },
        Some(_) => match run_grok_inspect() {
            Some(text) => parse_grok_inspect(&text),
            None => GrokReachability::Unknown {
                reason: "grok inspect --json failed or timed out".to_string(),
            },
        },
    }
}

fn which_grok() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("grok"))
        .find(|candidate| candidate.is_file())
}

fn run_grok_inspect() -> Option<String> {
    let mut cmd = std::process::Command::new("grok");
    cmd.arg("inspect")
        .arg("--json")
        .env_remove("GROK_SESSION_ID");
    let out = crate::bounded_cmd::output_with_timeout_result(cmd, 30).ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Classify recorded `grok inspect --json` text. `Absent` when no enabled fno
/// plugin carries hooks; `Unknown` on unparseable JSON.
fn parse_grok_inspect(text: &str) -> GrokReachability {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return GrokReachability::Unknown {
            reason: "inspect output did not parse as JSON".to_string(),
        };
    };
    let Some(plugins) = v.get("plugins").and_then(Value::as_array) else {
        return GrokReachability::Unknown {
            reason: "inspect output has no plugins array".to_string(),
        };
    };
    for plugin in plugins {
        let name_ok = plugin.get("name").and_then(Value::as_str) == Some("fno");
        let enabled = plugin.get("enabled").and_then(Value::as_bool) == Some(true);
        let hooks = plugin
            .get("provides")
            .and_then(|p| p.get("hooks"))
            .and_then(Value::as_bool)
            == Some(true);
        if name_ok && enabled && hooks {
            let path = plugin
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return GrokReachability::Reachable { path };
        }
    }
    GrokReachability::Absent
}

/// grok's own installer, then a second status read: `installed` prints only
/// when the second read is reachable. grok dedupes plugins by name, so a
/// Claude-compat copy and a grok-installed copy never both load.
fn install_grok(stage: &Path, _force: bool) -> Result<String, String> {
    match grok_reachability() {
        GrokReachability::Reachable { path } => Ok(format!("already installed, reachable: {path}")),
        GrokReachability::Unknown { reason } => Err(format!("unknown: {reason}")),
        GrokReachability::Absent => {
            run_checked(
                &[
                    "grok".into(),
                    "plugin".into(),
                    "install".into(),
                    stage.display().to_string(),
                    "--trust".into(),
                ],
                None,
            )?;
            match grok_reachability() {
                GrokReachability::Reachable { path } => Ok(format!("installed, reachable: {path}")),
                _ => Err("installed but post-install status read is not reachable".into()),
            }
        }
    }
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

    /// A repo with one commit carrying a hook config, one script, and the
    /// public marketplace manifest with its GitHub release pins.
    fn new_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        git_in(dir, &["init", "-b", "main", "-q"]);
        fs::create_dir_all(dir.join("hooks")).unwrap();
        fs::write(
            dir.join("hooks/hooks.json"),
            "{\"hooks\":[{\"command\":\"${CLAUDE_PLUGIN_ROOT}/hooks/live.sh\"}]}",
        )
        .unwrap();
        fs::write(dir.join("hooks/live.sh"), "live\n").unwrap();
        fs::write(
            dir.join(MARKETPLACE_REL),
            r#"{"name":"footnote","plugins":[{"name":"fno","source":{"source":"github","repo":"bllshttng/footnote","ref":"stable"}},{"name":"fno-nightly","source":{"source":"github","repo":"bllshttng/footnote","ref":"nightly"}}]}"#,
        )
        .unwrap();
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

    /// The staged manifest serves the fno entry from the stage itself while
    /// the source checkout keeps the public GitHub pins, and the rewrite is
    /// not reported as drift.
    #[test]
    fn stage_serves_fno_entry_locally_without_drift() {
        let base = std::env::temp_dir().join(format!("pi-mkt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        let staged: Value =
            serde_json::from_str(&fs::read_to_string(stage.join(MARKETPLACE_REL)).unwrap())
                .unwrap();
        let entries = staged["plugins"].as_array().unwrap();
        let fno = entries.iter().find(|p| p["name"] == "fno").unwrap();
        assert_eq!(fno["source"], json!("./"));
        let nightly = entries.iter().find(|p| p["name"] == "fno-nightly").unwrap();
        assert_eq!(nightly["source"]["ref"], json!("nightly"));
        let public: Value =
            serde_json::from_str(&fs::read_to_string(source.join(MARKETPLACE_REL)).unwrap())
                .unwrap();
        assert_eq!(public["plugins"][0]["source"]["ref"], json!("stable"));
        assert_eq!(check_stage_report(&stage, &source).status, "fresh");
        let _ = fs::remove_dir_all(&base);
    }

    /// A hand-tampered staged manifest still reads stale with a named sample:
    /// the served copy is checked, not exempt.
    #[test]
    fn tampered_staged_manifest_reads_stale() {
        let base = std::env::temp_dir().join(format!("pi-mkt2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        fs::write(stage.join(MARKETPLACE_REL), "{\"tampered\":true}").unwrap();
        let report = check_stage_report(&stage, &source);
        assert_eq!(report.status, "stale");
        assert_eq!(report.sample, vec![MARKETPLACE_REL.to_string()]);
        let _ = fs::remove_dir_all(&base);
    }

    /// A manifest that cannot serve the fno entry (entry renamed away) fails
    /// the build loud instead of silently shipping the github pin back.
    #[test]
    fn build_refuses_manifest_without_fno_entry() {
        let base = std::env::temp_dir().join(format!("pi-mkt3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        new_repo(&source);
        fs::write(
            source.join(MARKETPLACE_REL),
            r#"{"name":"footnote","plugins":[{"name":"fno-nightly","source":{"source":"github","repo":"bllshttng/footnote","ref":"nightly"}}]}"#,
        )
        .unwrap();
        git_in(&source, &["add", "-A"]);
        git_in(&source, &["commit", "-q", "-m", "c2"]);
        let stage_parent = base.join("stage-parent");
        fs::create_dir_all(&stage_parent).unwrap();
        let err = build_stage(&source, &stage_parent).unwrap_err();
        assert!(err.contains("no fno entry"), "err: {err}");
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

    /// The uninstall mode word must parse into the opencode arm's flag: a
    /// swallowed --uninstall would run an INSTALL where the user asked for
    /// the opposite, so the wiring is pinned at the parser, env-free.
    #[test]
    fn opencode_mode_words_parse() {
        let args: Vec<String> = ["opencode", "--uninstall", "--json"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let parsed = parse_plugin_install_args(&args);
        assert_eq!(parsed.mode.as_deref(), Some("opencode"));
        assert!(parsed.uninstall);
        assert!(parsed.json);

        let args: Vec<String> = ["opencode", "--status", "--installed"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let parsed = parse_plugin_install_args(&args);
        assert_eq!(parsed.mode.as_deref(), Some("opencode"));
        assert!(parsed.status);
        assert!(parsed.quick);
        assert!(!parsed.uninstall);

        let args: Vec<String> = ["claude"].iter().map(|s| (*s).to_string()).collect();
        let parsed = parse_plugin_install_args(&args);
        assert_eq!(parsed.mode.as_deref(), Some("claude"));
        assert!(!parsed.uninstall && !parsed.status && !parsed.quick);
    }

    /// Regression: the --force arm once skipped `i += 1`, so the parse loop
    /// spun on the flag forever. The test returning at all is the proof.
    #[test]
    fn parse_force_flag_returns_and_sets_flags() {
        let args: Vec<String> = ["--force", "--status"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let parsed = parse_plugin_install_args(&args);
        assert!(parsed.force);
        assert!(parsed.status);
    }

    /// A throwaway HOME for the root-enumeration fixtures. The marketplace
    /// carries both a path-bearing source and an installLocation; the
    /// installLocation must win (Claude records the in-place load path there).
    fn fixture_home(base: &Path) -> PathBuf {
        let home = base.join("home");
        fs::create_dir_all(home.join(".claude/plugins")).unwrap();
        home
    }

    fn write_marketplace(home: &Path, shape: &str, install_location: &Path, source_path: &Path) {
        let v = json!({"footnote": {"source": {"source": shape, "path": source_path.display().to_string()},
            "installLocation": install_location.display().to_string()}});
        fs::write(
            home.join(".claude/plugins/known_marketplaces.json"),
            serde_json::to_string(&v).unwrap(),
        )
        .unwrap();
    }

    fn write_registry(home: &Path, install_path: &Path) {
        let v = json!({"version": 2, "plugins": {"fno@footnote": [
            {"scope": "user", "installPath": install_path.display().to_string()}]}});
        fs::write(
            home.join(".claude/plugins/installed_plugins.json"),
            serde_json::to_string(&v).unwrap(),
        )
        .unwrap();
    }

    /// Recorded `grok inspect --json` text (grok 1.0.34, machine 2026-09-18):
    /// the fno plugin entry, an unrelated plugin, and a hooks-bearing plugin.
    fn grok_inspect_sample() -> String {
        let fno = serde_json::json!({
            "name": "fno", "scope": "user", "enabled": true,
            "path": "/claude/plugins/cache/footnote/fno/0.3.2",
            "provides": {"skills": 25, "agents": 1, "hooks": true, "mcpServers": 0}
        });
        let other = serde_json::json!({
            "name": "feature-dev", "scope": "user", "enabled": true,
            "path": "/plugins/feature-dev",
            "provides": {"skills": 0, "agents": 1, "hooks": false, "mcpServers": 0}
        });
        serde_json::to_string(&serde_json::json!({ "plugins": [other, fno] })).unwrap()
    }

    #[test]
    fn grok_parse_reachable_absent_disabled_and_malformed() {
        // Reachable: the recorded sample names fno enabled with hooks.
        match parse_grok_inspect(&grok_inspect_sample()) {
            GrokReachability::Reachable { path } => {
                assert!(path.ends_with("fno/0.3.2"), "{path}");
            }
            other => panic!("want Reachable, got {other:?}"),
        }
        // Absent: fno missing entirely.
        let no_fno =
            r#"{"plugins":[{"name":"feature-dev","enabled":true,"provides":{"hooks":false}}]}"#;
        assert!(matches!(
            parse_grok_inspect(no_fno),
            GrokReachability::Absent
        ));
        // Disabled, hooks false: both read as absent.
        let disabled = r#"{"plugins":[{"name":"fno","enabled":false,"provides":{"hooks":true}}]}"#;
        assert!(matches!(
            parse_grok_inspect(disabled),
            GrokReachability::Absent
        ));
        let no_hooks = r#"{"plugins":[{"name":"fno","enabled":true,"provides":{"hooks":false}}]}"#;
        assert!(matches!(
            parse_grok_inspect(no_hooks),
            GrokReachability::Absent
        ));
        // Malformed: unknown, never reads as installed.
        assert!(matches!(
            parse_grok_inspect("not json {"),
            GrokReachability::Unknown { .. }
        ));
        let no_array = r#"{"plugins":{}}"#;
        assert!(matches!(
            parse_grok_inspect(no_array),
            GrokReachability::Unknown { .. }
        ));
    }

    /// AC1-HP: with no --stage, the check runs once per enumerated root. A
    /// byte-identical marketplace stage reads live and fresh; a differing
    /// registry installPath reads not live, stale with a named sample, and
    /// the worst status drives the exit code.
    #[test]
    fn check_without_stage_reports_every_root_and_worst_exit() {
        let base = std::env::temp_dir().join(format!("pi-roots-ac1-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        let home = fixture_home(&base);
        write_marketplace(&home, "directory", &stage, &base.join("elsewhere"));
        // A registry installPath whose bytes differ from source HEAD in
        // exactly one file.
        let cache = base.join("registry-tree");
        fs::create_dir_all(cache.join("hooks")).unwrap();
        fs::write(
            cache.join("hooks/hooks.json"),
            "{\"hooks\":[{\"command\":\"${CLAUDE_PLUGIN_ROOT}/hooks/live.sh\"}]}",
        )
        .unwrap();
        fs::write(cache.join("hooks/live.sh"), "tampered\n").unwrap();
        write_registry(&home, &cache);

        let (roots, detail) = plugin_roots_for(&home);
        assert_eq!(roots.len(), 2, "roots: {roots:?} detail: {detail:?}");
        let (report, exit) = check_roots_report(&roots, detail, &source);
        assert_eq!(exit, 3);
        assert_eq!(report.roots.len(), 2);
        let live = &report.roots[0];
        assert!(live.live, "the marketplace root must read live");
        assert_eq!(live.check.status, "fresh");
        assert_eq!(live.path, stage.display().to_string());
        let second = &report.roots[1];
        assert!(!second.live);
        assert_eq!(second.check.status, "stale");
        // The registry copy also lacks the manifest HEAD tracks.
        assert_eq!(
            second.check.sample,
            vec![
                ".claude-plugin/marketplace.json".to_string(),
                "hooks/live.sh".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// The installLocation (where Claude actually loads a local marketplace)
    /// wins over source.path, and origins dedupe by canonical path with the
    /// earlier origin kept.
    #[test]
    fn roots_prefer_install_location_and_dedupe_by_canonical_path() {
        let base = std::env::temp_dir().join(format!("pi-roots-dedupe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        new_repo(&source);
        let stage_parent = base.join("stage-parent");
        fs::create_dir_all(&stage_parent).unwrap();
        let (stage, _) = build_stage(&source, &stage_parent).unwrap();
        let home = fixture_home(&base);
        // installLocation names the stage; source.path names a DIFFERENT
        // existing tree, so a wrong preference shows up in the enumeration.
        write_marketplace(&home, "directory", &stage, &source);
        // The registry names the cache path, which is also a hardcoded
        // orphan path: dedupe must keep the earlier origin (registry).
        let cache = home.join(".claude/plugins/cache/world");
        fs::create_dir_all(&cache).unwrap();
        write_registry(&home, &cache);

        let (roots, _) = plugin_roots_for(&home);
        assert_eq!(roots.len(), 2, "roots: {roots:?}");
        assert_eq!(roots[0].path, stage);
        assert!(roots[0].live);
        assert_eq!(
            roots[1].origin, "registry",
            "earlier origin wins: {roots:?}"
        );
        assert_eq!(roots[1].path, cache);
        let _ = fs::remove_dir_all(&base);
    }

    /// AC2-HP: a missing registry contributes no roots and one detail line,
    /// and no root the enumerator could not read is ever reported fresh.
    #[test]
    fn roots_missing_or_malformed_registry_never_reads_fresh() {
        let base = std::env::temp_dir().join(format!("pi-roots-ac2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, stage) = fresh_stage(&base);
        let home = fixture_home(&base);
        write_marketplace(&home, "directory", &stage, &stage);

        let (roots, detail) = plugin_roots_for(&home);
        assert_eq!(roots.len(), 1, "roots: {roots:?}");
        assert!(roots[0].live);
        assert!(
            detail.iter().any(|d| d.contains("installed_plugins.json")),
            "detail: {detail:?}"
        );
        let (report, exit) = check_roots_report(&roots, detail, &source);
        assert_eq!(exit, 0);
        assert!(report.roots.iter().all(|r| r.check.status == "fresh"));

        // Malformed JSON: same never-assert rule, one detail line, no roots
        // contributed by the registry.
        fs::write(
            home.join(".claude/plugins/installed_plugins.json"),
            "not json {",
        )
        .unwrap();
        let (roots, detail) = plugin_roots_for(&home);
        assert!(roots.iter().all(|r| r.origin != "registry"));
        assert!(detail.iter().any(|d| d.contains("installed_plugins.json")));
        let _ = fs::remove_dir_all(&base);
    }

    /// A NON-local marketplace (github and friends) yields no live root, and
    /// the registry installPath copy is still enumerated and byte-checked
    /// whatever the marketplace shape - the enumeration is shape-independent.
    #[test]
    fn roots_without_a_local_marketplace_still_report_registry_copies() {
        let base = std::env::temp_dir().join(format!("pi-roots-github-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (source, _stage) = fresh_stage(&base);
        let home = fixture_home(&base);
        write_marketplace(
            &home,
            "github",
            &base.join("not-on-disk"),
            &base.join("not-on-disk"),
        );
        // A registry copy that differs from source HEAD in one file.
        let cache = home.join(".claude/plugins/cache/footnote");
        fs::create_dir_all(cache.join("hooks")).unwrap();
        fs::write(
            cache.join("hooks/hooks.json"),
            "{\"hooks\":[{\"command\":\"${CLAUDE_PLUGIN_ROOT}/hooks/live.sh\"}]}",
        )
        .unwrap();
        fs::write(cache.join("hooks/live.sh"), "tampered\n").unwrap();
        write_registry(&home, &cache);

        let (roots, _) = plugin_roots_for(&home);
        assert_eq!(roots.len(), 1, "only the registry copy exists: {roots:?}");
        assert!(!roots[0].live, "a github marketplace yields no live root");
        let (report, exit) = check_roots_report(&roots, Vec::new(), &source);
        assert_eq!(exit, 3);
        assert_eq!(report.roots[0].check.status, "stale");
        assert_eq!(
            report.roots[0].check.sample,
            vec![
                ".claude-plugin/marketplace.json".to_string(),
                "hooks/live.sh".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// AC2.1-HP + AC2.1-ERR: the Claude cache copy is removed only under the
    /// guard. A local marketplace with a proven live root removes it and
    /// receipts the path; a non-local marketplace and a cache that IS the
    /// live root both keep it, naming the condition that refused.
    #[test]
    fn claude_cache_removal_is_guarded() {
        let base = std::env::temp_dir().join(format!("pi-rm-ac3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let home = fixture_home(&base);
        let stage = base.join("stage");
        fs::create_dir_all(&stage).unwrap();
        let cache = home.join(".claude/plugins/cache/footnote");
        fs::create_dir_all(&cache).unwrap();
        let live_stage = vec![PluginRoot {
            path: stage.clone(),
            live: true,
            origin: "marketplace",
        }];

        // HP: directory marketplace + live stage + existing cache -> removed.
        write_marketplace(&home, "directory", &stage, &stage);
        let (removed, refused) = remove_stale_copies(&home, &live_stage);
        assert_eq!(removed, vec![cache.clone()], "removed: {removed:?}");
        assert!(refused.is_empty());
        assert!(!cache.exists());
        fs::create_dir_all(&cache).unwrap();

        // ERR: a non-local marketplace refuses, naming the shape.
        write_marketplace(&home, "github", &stage, &stage);
        let (removed, refused) = remove_stale_copies(&home, &live_stage);
        assert!(removed.is_empty());
        assert!(
            refused.iter().any(|l| l.contains("'github'")),
            "{refused:?}"
        );
        assert!(cache.exists());
        fs::create_dir_all(&cache).unwrap();

        // ERR: the cache itself is the live root -> kept, naming why.
        write_marketplace(&home, "directory", &cache, &cache);
        let live_cache = vec![PluginRoot {
            path: cache.clone(),
            live: true,
            origin: "marketplace",
        }];
        let (removed, refused) = remove_stale_copies(&home, &live_cache);
        assert!(removed.is_empty());
        assert!(
            refused.iter().any(|l| l.contains("live root")),
            "{refused:?}"
        );
        assert!(cache.exists());
        let _ = fs::remove_dir_all(&base);
    }
}
