//! `fno uninstall`: find everything an fno install put on this machine, list
//! it, and remove each item through its own uninstall verb.
//!
//! Native on purpose: the verb removes the Python wheel half way through, so
//! it must never depend on that wheel. `~/.fno` stays unless `--purge` is
//! passed and the user types the confirmation word on a terminal.

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use clap::Args;
use serde_json::Value;

use crate::process_admission::{std_command, std_spawn_for_human};

/// `fno uninstall`'s flags.
#[derive(Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Opts {
    /// Remove without asking (required when stdin is not a terminal)
    #[arg(long)]
    pub yes: bool,
    /// List what would be removed and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Also delete ~/.fno, after you type the confirmation word
    #[arg(long)]
    pub purge: bool,
}

/// The dev build-dir export `fno-agents plugin-install` writes.
const BUILD_DIR_KEY: &str = "CARGO_BUILD_BUILD_DIR";
const RC_MARK: &str = "# fno: cargo build-dir";
/// The first line of the block `fno config setup cli-hooks` appends to codex.
const CODEX_BLOCK_MARK: &str = "# Added by `fno config setup cli-hooks`";
const PURGE_WORD: &str = "purge";
/// The uv tool name the wheel installs under (a directory in `uv tool dir`).
const UV_TOOL: &str = "fno";
/// The crontab block `scripts/install-autocorrect-cron.sh` writes on Linux.
const CRON_BEGIN: &str = "# BEGIN autocorrect-managed";
const CRON_END: &str = "# END autocorrect-managed";
/// The plugin name agy knows: the repo plugin.json "name", the same name
/// `scripts/install/agy-plugin.sh` installs and `agy plugin uninstall` takes.
const AGY_PLUGIN: &str = "footnote";

enum Action {
    Run(Vec<String>),
    Launchd { label: String, plist: PathBuf },
    StripJson(PathBuf),
    StripCodexToml(PathBuf),
    StripRc(PathBuf),
    StripCrontab,
    RemovePath(PathBuf),
    StopDaemon(Vec<u32>),
    KillMux,
}

struct Item {
    what: String,
    action: Action,
}

impl Item {
    fn how(&self) -> String {
        match &self.action {
            Action::Run(argv) => argv.join(" "),
            Action::Launchd { label, plist } => {
                format!(
                    "launchctl bootout gui/<uid>/{label}; rm {}",
                    plist.display()
                )
            }
            Action::StripJson(p) | Action::StripCodexToml(p) | Action::StripRc(p) => {
                format!("edit {}", p.display())
            }
            Action::StripCrontab => "edit crontab (drop the managed block)".into(),
            Action::RemovePath(p) => format!("rm -r {}", p.display()),
            Action::StopDaemon(pids) => format!("SIGTERM pid {pids:?}"),
            Action::KillMux => "fno mux kill-server --all".into(),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn env_dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(default))
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Run a command to completion with its output captured. `Err` carries the
/// command and its stderr so the receipt names what failed.
fn run(argv: &[String]) -> Result<String, String> {
    let mut cmd = std_command(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = std_spawn_for_human(&mut cmd)
        .and_then(|child| child.wait_with_output())
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    Err(format!(
        "`{}` exited {}: {detail}",
        argv.join(" "),
        out.status.code().unwrap_or(-1)
    ))
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// Run a command to completion with `input` on its stdin, output captured.
fn run_stdin(argv: &[String], input: &str) -> Result<String, String> {
    let mut cmd = std_command(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let failed = |e: std::io::Error| format!("{}: {e}", argv[0]);
    let mut child = std_spawn_for_human(&mut cmd).map_err(failed)?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("{}: no stdin handle", argv[0]))?
        .write_all(input.as_bytes())
        .map_err(failed)?;
    let out = child.wait_with_output().map_err(failed)?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    Err(format!(
        "`{}` exited {}: {detail}",
        argv.join(" "),
        out.status.code().unwrap_or(-1)
    ))
}

/// The user's crontab body, or `None` when cron has none for this user. Any
/// other failure is `Err`: an unreadable crontab is never clobbered.
fn crontab_list() -> Result<Option<String>, String> {
    match run(&argv(&["crontab", "-l"])) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.contains("no crontab") => Ok(None),
        Err(e) => Err(e),
    }
}

/// The agy plugin is in when a staged copy exists (the config path agy 1.1.16
/// measured, or the `AGY_PLUGIN_HOME` override `scripts/install/agy-plugin.sh`
/// also writes) or the import manifest names it. Shared by discover and
/// residue so the two can never disagree.
fn agy_plugin_in() -> bool {
    let home = home();
    [
        home.join(".gemini/config/plugins").join(AGY_PLUGIN),
        env_dir("AGY_PLUGIN_HOME", ".gemini/antigravity-cli/plugins").join(AGY_PLUGIN),
    ]
    .iter()
    .any(|path| path.is_dir())
        || read(&home.join(".gemini/config/import_manifest.json"))
            .contains(&format!("\"{AGY_PLUGIN}\""))
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// This user's processes whose full command line matches `pattern` (a pgrep
/// regex), as `(pid, name)`, never this process. `-f` because Linux cuts the
/// process name to 15 characters, which `fno-agents-daemon` exceeds.
fn processes(pattern: &str) -> Vec<(u32, String)> {
    let me = std::process::id();
    run(&argv(&[
        "pgrep",
        "-l",
        "-f",
        "-U",
        &uid().to_string(),
        pattern,
    ]))
    .unwrap_or_default()
    .lines()
    .filter_map(|line| {
        let (pid, name) = line.trim().split_once(' ')?;
        Some((pid.parse().ok()?, name.to_string()))
    })
    .filter(|(pid, _)| *pid != me)
    .collect()
}

/// The launchd label of a plist fno installed, or `None` for anyone else's.
/// The autocorrect agents carry a generic `com.user` label, so only their
/// footnote script names claim them.
pub(crate) fn launch_agent_label(file_name: &str, plist: &str) -> Option<String> {
    let label = file_name.strip_suffix(".plist")?;
    let ours = label.starts_with("sh.fno.")
        || (label.starts_with("com.user.autocorrect")
            && (plist.contains("autocorrect-review.sh")
                || plist.contains("autocorrect-watcher.sh")));
    ours.then(|| label.to_string())
}

fn launch_agents() -> Vec<(String, PathBuf)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let dir = home().join("Library/LaunchAgents");
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            launch_agent_label(&name, &read(&path)).map(|label| (label, path))
        })
        .collect();
    found.sort();
    found
}

fn is_footnote_hook(hook: &Value) -> bool {
    if hook.get("name").and_then(Value::as_str) == Some("fno-session-start") {
        return true;
    }
    let cmd = hook.get("command").and_then(Value::as_str).unwrap_or("");
    cmd.contains("hooks/worktree-remove.sh")
        || (cmd.contains("hooks/session-start.sh")
            && (cmd.contains("FNO_PLATFORM=") || cmd.contains("/footnote/")))
}

/// Remove footnote's hook entries and its build-dir env export from one
/// harness settings document. A group or event array is dropped only when
/// this pass emptied it; everything else stays. Returns the count removed.
pub(crate) fn strip_json(doc: &mut Value) -> usize {
    let mut removed = 0;
    if let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) {
        hooks.retain(|_, groups| {
            let Some(groups) = groups.as_array_mut() else {
                return true;
            };
            let before = removed;
            groups.retain_mut(|group| {
                let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                    return true;
                };
                let n = entries.len();
                entries.retain(|h| !is_footnote_hook(h));
                removed += n - entries.len();
                !(n > entries.len() && entries.is_empty())
            });
            !(removed > before && groups.is_empty())
        });
    }
    if let Some(env) = doc.get_mut("env").and_then(Value::as_object_mut) {
        let ours = env
            .get(BUILD_DIR_KEY)
            .and_then(Value::as_str)
            .is_some_and(|v| v.contains(".fno"));
        if ours {
            env.remove(BUILD_DIR_KEY);
            removed += 1;
        }
    }
    removed
}

fn join_lines(lines: &[&str]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Line-based, so a codex config keeps its comments and layout. Drops the
/// marked SessionStart block, the `fno@footnote` hook trust tables and the
/// build-dir export. `None` when nothing matched.
pub(crate) fn strip_codex_toml(text: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut in_trust_table = false;
    for line in text.lines() {
        let t = line.trim();
        // The block ends at its `command` line. Any line the template never
        // writes ends it early, so an edited block cannot eat the rest of
        // the file.
        if in_block {
            if t.starts_with("command") {
                in_block = false;
                continue;
            }
            let template_line = t.is_empty()
                || t.starts_with('#')
                || t == "[[hooks.SessionStart]]"
                || t == "[[hooks.SessionStart.hooks]]"
                || t == "type = \"command\"";
            if template_line {
                continue;
            }
            in_block = false;
        }
        if t.starts_with(CODEX_BLOCK_MARK) {
            in_block = true;
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            in_trust_table = t.starts_with("[hooks.state.\"fno@footnote:");
        }
        if in_trust_table || (t.starts_with(BUILD_DIR_KEY) && t.contains(".fno")) {
            continue;
        }
        out.push(line);
    }
    (out.len() != text.lines().count()).then(|| join_lines(&out))
}

/// Drop the marked build-dir export pair from a shell rc file.
pub(crate) fn strip_rc(text: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut after_mark = false;
    for line in text.lines() {
        if line == RC_MARK {
            after_mark = true;
            continue;
        }
        if std::mem::take(&mut after_mark) && line.starts_with("export CARGO_BUILD_BUILD_DIR=") {
            continue;
        }
        out.push(line);
    }
    (out.len() != text.lines().count()).then(|| join_lines(&out))
}

/// Drop the managed autocorrect block from a crontab body: the lines between
/// the sentinels and the sentinels themselves. A lost END sentinel strips to
/// the last line, matching the installer's awk. `None` when no block present.
fn strip_crontab(text: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for line in text.lines() {
        if line == CRON_BEGIN {
            skip = true;
        } else if line == CRON_END {
            skip = false;
        } else if !skip {
            out.push(line);
        }
    }
    (out.len() != text.lines().count()).then(|| join_lines(&out))
}

fn json_has_ours(path: &Path) -> bool {
    serde_json::from_str::<Value>(&read(path))
        .map(|mut doc| strip_json(&mut doc) > 0)
        .unwrap_or(false)
}

/// Everything removable, in removal order: schedulers first so nothing
/// respawns, then the harness plugins and config, then the binaries, with
/// this binary last.
fn discover() -> Vec<Item> {
    let mut items = Vec::new();
    let mut add = |what: String, action: Action| items.push(Item { what, action });
    let home = home();

    for (label, plist) in launch_agents() {
        add(
            format!("launchd agent {label}"),
            Action::Launchd { label, plist },
        );
    }

    // A scheduler, so it goes with launchd before anything it could respawn.
    // An unreadable crontab stays untouched: the action re-reads and refuses.
    // The probe is the strip itself, so an item is discovered only when the
    // action can actually remove it.
    if let Ok(Some(text)) = crontab_list() {
        if strip_crontab(&text).is_some() {
            add("autocorrect crontab block".into(), Action::StripCrontab);
        }
    }

    let claude = env_dir("CLAUDE_CONFIG_DIR", ".claude");
    if on_path("claude").is_some() {
        if read(&claude.join("plugins/installed_plugins.json")).contains("\"fno@footnote\"") {
            add(
                "Claude Code plugin fno@footnote".into(),
                Action::Run(argv(&["claude", "plugin", "uninstall", "fno@footnote"])),
            );
        }
        if read(&claude.join("plugins/known_marketplaces.json")).contains("\"footnote\"") {
            add(
                "Claude Code marketplace footnote".into(),
                Action::Run(argv(&[
                    "claude",
                    "plugin",
                    "marketplace",
                    "remove",
                    "footnote",
                ])),
            );
        }
    }

    let codex = env_dir("CODEX_HOME", ".codex");
    let codex_config = codex.join("config.toml");
    if on_path("codex").is_some() {
        let text = read(&codex_config);
        if text.contains("[plugins.\"fno@footnote\"]") {
            add(
                "Codex plugin fno@footnote".into(),
                Action::Run(argv(&["codex", "plugin", "remove", "fno@footnote"])),
            );
        }
        if text.contains("[marketplaces.footnote]") {
            add(
                "Codex marketplace footnote".into(),
                Action::Run(argv(&[
                    "codex",
                    "plugin",
                    "marketplace",
                    "remove",
                    "footnote",
                ])),
            );
        }
    }

    if on_path("agy").is_some() && agy_plugin_in() {
        add(
            format!("agy plugin {AGY_PLUGIN}"),
            Action::Run(argv(&["agy", "plugin", "uninstall", AGY_PLUGIN])),
        );
    }

    let opencode_manifest = std::fs::read_dir(home.join(".fno"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("opencode-install-")
        });
    if opencode_manifest && on_path("fno-agents").is_some() {
        add(
            "OpenCode plugin files".into(),
            Action::Run(argv(&[
                "fno-agents",
                "plugin-install",
                "opencode",
                "--uninstall",
            ])),
        );
    }

    for (what, path) in [
        ("Claude settings hooks", claude.join("settings.json")),
        ("Gemini settings hooks", home.join(".gemini/settings.json")),
        ("Codex legacy hooks.json", codex.join("hooks.json")),
    ] {
        if json_has_ours(&path) {
            add(
                format!("{what} ({})", path.display()),
                Action::StripJson(path),
            );
        }
    }
    if strip_codex_toml(&read(&codex_config)).is_some() {
        add(
            format!("Codex config hooks ({})", codex_config.display()),
            Action::StripCodexToml(codex_config),
        );
    }
    for rc in [".zshrc", ".bashrc"] {
        let path = home.join(rc);
        if strip_rc(&read(&path)).is_some() {
            add(format!("build-dir export in ~/{rc}"), Action::StripRc(path));
        }
    }
    let pi = env_dir("PI_CODING_AGENT_DIR", ".pi/agent").join("extensions/footnote.ts");
    if pi.is_file() {
        add("pi extension footnote.ts".into(), Action::RemovePath(pi));
    }

    if on_path("uv").is_some() {
        // FORCE_COLOR in the user's env would wrap the path in ANSI escapes.
        let tools = run(&argv(&["uv", "--color", "never", "tool", "dir"])).unwrap_or_default();
        if !tools.trim().is_empty() && Path::new(tools.trim()).join(UV_TOOL).is_dir() {
            add(
                "Python CLI (uv tool fno)".into(),
                Action::Run(argv(&["uv", "tool", "uninstall", UV_TOOL])),
            );
        }
    }
    let brewed = on_path("brew").is_some()
        && run(&argv(&["brew", "list", "--versions", "fno"])).is_ok_and(|v| !v.trim().is_empty());
    if brewed {
        add(
            "Homebrew formula fno".into(),
            Action::Run(argv(&["brew", "uninstall", "fno"])),
        );
    }
    let sentinel = crate::bootstrap::sentinel_dir();
    if sentinel.is_dir() {
        add("bootstrap cache".into(), Action::RemovePath(sentinel));
    }

    let daemons: Vec<u32> = processes("(^|/)fno-agents-daemon( |$)")
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    if !daemons.is_empty() {
        add("fno-agents daemon".into(), Action::StopDaemon(daemons));
    }
    let mux_live = std::fs::read_dir(crate::proto::mux_dir())
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "sock"));
    if mux_live {
        add("fno mux sessions".into(), Action::KillMux);
    }

    let crates = read(&env_dir("CARGO_HOME", ".cargo").join(".crates.toml"));
    if on_path("cargo").is_some() {
        for krate in ["fno-agents", "fno"] {
            if crates.contains(&format!("\"{krate} ")) {
                add(
                    format!("Rust binaries (cargo crate {krate})"),
                    Action::Run(argv(&["cargo", "uninstall", krate])),
                );
            }
        }
    }
    items
}

/// Write through a temp sibling and rename, so an interrupted run never
/// leaves a truncated settings file. The rename lands on the symlink's
/// target, so a dotfiles-managed file stays a symlink.
fn rewrite(path: &Path, text: String) -> Result<String, String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let target = std::fs::canonicalize(path).map_err(err)?;
    let mut tmp = target.clone().into_os_string();
    tmp.push(".fno-uninstall.tmp");
    // A codex config can hold secrets at 0600: the temp file is born with
    // the original's mode, never readable wider for a moment.
    let mode = std::fs::metadata(&target)
        .map_err(err)?
        .permissions()
        .mode();
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode & 0o7777)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(err)?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        err(e)
    })?;
    Ok(format!("edited {}", path.display()))
}

fn stop_daemon(pids: &[u32]) -> Result<String, String> {
    for &pid in pids {
        // SAFETY: kill with a real pid read from pgrep; a gone pid is ESRCH.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    for _ in 0..50 {
        // SAFETY: signal 0 only probes existence.
        if pids
            .iter()
            .all(|&p| unsafe { libc::kill(p as libc::pid_t, 0) } != 0)
        {
            return Ok("stopped".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("still running after SIGTERM and 5s: pid {pids:?}"))
}

fn execute(action: &Action) -> Result<String, String> {
    match action {
        Action::Run(words) => run(words).map(|_| "removed".into()),
        Action::Launchd { label, plist } => {
            // A job that is not loaded fails bootout; removing the plist is
            // what keeps it from loading at the next login either way.
            let _ = run(&argv(&[
                "launchctl",
                "bootout",
                &format!("gui/{}/{label}", uid()),
            ]));
            std::fs::remove_file(plist)
                .map(|_| "removed".into())
                .map_err(|e| format!("{}: {e}", plist.display()))
        }
        Action::StripJson(path) => {
            let mut doc: Value = serde_json::from_str(&read(path)).map_err(|e| {
                format!("{} is not valid JSON ({e}); left unchanged", path.display())
            })?;
            strip_json(&mut doc);
            let text = serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n";
            rewrite(path, text)
        }
        Action::StripCodexToml(path) => match strip_codex_toml(&read(path)) {
            Some(text) => rewrite(path, text),
            None => Ok("already clean".into()),
        },
        Action::StripRc(path) => match strip_rc(&read(path)) {
            Some(text) => rewrite(path, text),
            None => Ok("already clean".into()),
        },
        Action::StripCrontab => {
            let Some(text) = crontab_list()? else {
                return Ok("already clean".into());
            };
            let Some(stripped) = strip_crontab(&text) else {
                return Ok("already clean".into());
            };
            // A crontab holding only our block is removed outright, like the
            // installer's own --uninstall; anything left is written back.
            if stripped.trim().is_empty() {
                let _ = run(&argv(&["crontab", "-r"]));
            } else {
                run_stdin(&argv(&["crontab", "-"]), &stripped)?;
            }
            Ok("removed".into())
        }
        Action::RemovePath(path) => {
            let gone = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            gone.map(|_| "removed".into())
                .map_err(|e| format!("{}: {e}", path.display()))
        }
        Action::StopDaemon(pids) => stop_daemon(pids),
        Action::KillMux => {
            match crate::mux_cli::kill_selector(crate::mux_cli::kill_policy::Selector::All, false) {
                0 => Ok("stopped".into()),
                code => Err(format!("fno mux kill-server --all exited {code}")),
            }
        }
    }
}

/// What is still named fno after the run: executables on PATH, launchd
/// plists, running processes, and a codex hook without its marker block.
fn residue() -> Vec<String> {
    let mut left = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = entry.path();
                if entry.file_name().to_string_lossy().starts_with("fno") && seen.insert(p.clone())
                {
                    left.push(format!("on PATH: {}", p.display()));
                }
            }
        }
    }
    for (label, plist) in launch_agents() {
        left.push(format!("launchd agent {label}: {}", plist.display()));
    }
    for (pid, name) in processes("(^|/)fno[^/ ]*( |$)") {
        left.push(format!("running: {name} (pid {pid})"));
    }
    let codex_config = env_dir("CODEX_HOME", ".codex").join("config.toml");
    if read(&codex_config).contains("FNO_PLATFORM=codex") {
        left.push(format!(
            "codex hook without its marker block in {}: remove the [[hooks.SessionStart]] entry running session-start.sh by hand",
            codex_config.display()
        ));
    }
    if agy_plugin_in() {
        left.push(format!(
            "agy plugin {AGY_PLUGIN}: agy plugin uninstall {AGY_PLUGIN}"
        ));
    }
    if let Ok(Some(text)) = crontab_list() {
        if strip_crontab(&text).is_some() {
            left.push(format!("crontab: {} block still present", CRON_BEGIN));
        }
    }
    left
}

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim().to_string()
}

pub fn run_uninstall(opts: Opts) -> i32 {
    // Every path below hangs off HOME. A relative one would point --purge at
    // the .fno dir of whatever project the user stands in.
    if !home().is_absolute() {
        eprintln!("fno uninstall: HOME is unset or not absolute; refusing to guess the state dir.");
        return 2;
    }
    let tty = std::io::stdin().is_terminal();
    let state = home().join(".fno");
    let items = discover();

    if items.is_empty() {
        println!("fno uninstall: no installed fno items found.");
    } else {
        println!("fno uninstall: found {} item(s):", items.len());
        for item in &items {
            println!("  - {}  [{}]", item.what, item.how());
        }
    }
    if opts.purge {
        let worktrees = std::fs::read_dir(state.join("worktrees"))
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|repo| {
                std::fs::read_dir(repo.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .count();
        println!(
            "  - local state {} (--purge): {worktrees} worktree(s) under it; uncommitted work there is lost",
            state.display()
        );
    } else if state.is_dir() {
        println!("kept: {} (pass --purge to delete it)", state.display());
    }
    if opts.dry_run {
        return 0;
    }
    if std::env::var_os("FNO_PANE").is_some() {
        eprintln!(
            "fno uninstall: refused inside an fno mux pane - it stops the mux, which would end this \
             pane mid-run. Run it from a plain terminal."
        );
        return 2;
    }

    if !items.is_empty() && !opts.yes {
        if !tty {
            eprintln!("fno uninstall: stdin is not a terminal; re-run with --yes to remove these.");
            return 2;
        }
        if !matches!(ask("Remove these? [y/N] ").as_str(), "y" | "Y" | "yes") {
            println!("fno uninstall: nothing removed.");
            return 0;
        }
    }
    let purge = opts.purge && state.is_dir();
    if purge {
        if !tty {
            eprintln!("fno uninstall: --purge needs a terminal to type the confirmation word; nothing removed.");
            return 2;
        }
        if ask(&format!(
            "Type {PURGE_WORD} to delete {}: ",
            state.display()
        )) != PURGE_WORD
        {
            println!("fno uninstall: purge not confirmed; nothing removed.");
            return 0;
        }
    }

    let mut failed = 0;
    for item in &items {
        match execute(&item.action) {
            Ok(word) => println!("{}: {word}", item.what),
            Err(e) => {
                failed += 1;
                println!("{}: FAILED: {e}", item.what);
            }
        }
    }
    if purge {
        match std::fs::remove_dir_all(&state) {
            Ok(()) => println!("{}: removed", state.display()),
            Err(e) => {
                failed += 1;
                println!("{}: FAILED: {e}", state.display());
            }
        }
    }

    let left = residue();
    if left.is_empty() {
        println!("fno uninstall: nothing named fno is left on PATH, in LaunchAgents or running.");
    } else {
        println!("fno uninstall: still present (not removed by a known installer):");
        for line in &left {
            println!("  - {line}");
        }
    }
    if failed > 0 || !left.is_empty() {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strippers_remove_only_footnote_entries() {
        let mut doc = json!({
            "env": {"CARGO_BUILD_BUILD_DIR": "/u/.fno/cargo-build", "KEEP": "1"},
            "hooks": {
                "WorktreeRemove": [{"hooks": [{"type": "command", "command": "bash '/p/footnote/hooks/worktree-remove.sh'"}]}],
                "SessionStart": [
                    {"hooks": [
                        {"name": "fno-session-start", "command": "env FNO_PLATFORM=gemini /p/hooks/session-start.sh"},
                        {"command": "/mine/session-start-logger.sh"}
                    ]},
                    {"hooks": []}
                ]
            }
        });
        assert_eq!(strip_json(&mut doc), 3);
        assert_eq!(
            doc,
            json!({
                "env": {"KEEP": "1"},
                "hooks": {"SessionStart": [
                    {"hooks": [{"command": "/mine/session-start-logger.sh"}]},
                    {"hooks": []}
                ]}
            })
        );

        let codex = "model = \"x\"\n\n# Added by `fno config setup cli-hooks` - footnote SessionStart context injection.\n# Codex treats this as an UNMANAGED hook: approve/trust it in Codex before it\n# runs. Remove this block to uninstall.\n[[hooks.SessionStart]]\n\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = \"env FNO_PLATFORM=codex /p/hooks/session-start.sh\"\n[hooks.state.\"fno@footnote:hooks/codex-hooks.json:pre_tool_use:0:0\"]\ntrusted_hash = \"abc\"\n[plugins.\"other@x\"]\nenabled = true\n";
        assert_eq!(
            strip_codex_toml(codex).as_deref(),
            Some("model = \"x\"\n\n[plugins.\"other@x\"]\nenabled = true\n")
        );
        assert_eq!(strip_codex_toml("model = \"x\"\n"), None);
        // A block that lost its command line ends at the first line the
        // template never writes, so the rest of the config survives.
        let edited = "# Added by `fno config setup cli-hooks` - footnote\n[[hooks.SessionStart]]\n[plugins.\"other@x\"]\nenabled = true\n";
        assert_eq!(
            strip_codex_toml(edited).as_deref(),
            Some("[plugins.\"other@x\"]\nenabled = true\n")
        );

        let rc = "alias ll=ls\n# fno: cargo build-dir\nexport CARGO_BUILD_BUILD_DIR=\"/u/.fno/b\"\nexport KEEP=1\n";
        assert_eq!(
            strip_rc(rc).as_deref(),
            Some("alias ll=ls\nexport KEEP=1\n")
        );

        let cron = "MAILTO=x\n# BEGIN autocorrect-managed\n*/15 * * * * /r/autocorrect-watcher.sh\n# END autocorrect-managed\nKEEP=1\n";
        assert_eq!(strip_crontab(cron).as_deref(), Some("MAILTO=x\nKEEP=1\n"));
        // A lost END sentinel strips to the last line, like the installer's awk.
        assert_eq!(
            strip_crontab("A\n# BEGIN autocorrect-managed\njob").as_deref(),
            Some("A\n")
        );
        assert_eq!(strip_crontab("MAILTO=x\n"), None);

        assert_eq!(
            launch_agent_label("sh.fno.groom.plist", "").as_deref(),
            Some("sh.fno.groom")
        );
        assert_eq!(
            launch_agent_label(
                "com.user.autocorrect.plist",
                "<string>/r/scripts/autocorrect-review.sh</string>"
            )
            .as_deref(),
            Some("com.user.autocorrect")
        );
        assert_eq!(
            launch_agent_label("com.user.autocorrect.plist", "<string>/mine.sh</string>"),
            None
        );
        assert_eq!(launch_agent_label("com.apple.x.plist", ""), None);
    }
}
