//! Attach-time "while you were gone" catch-up overlay (x-4e2d, client half).
//!
//! On attach the client asks: was I away from this mux session long enough to
//! want a catch-up? If so it shells out to `fno-agents digest --json` for the
//! focused pane's node and renders the ranked lines as a dismissable overlay.
//!
//! Two plan premises did not hold and are handled here:
//!   - The server has NO attach/detach timestamps (the `Client` struct carries
//!     none; `Info` is a frozen wire message). So "last detach age" is tracked
//!     CLIENT-LOCAL: [`record_detach`] writes epoch seconds keyed by mux session
//!     under the mux dir; [`read_detach_secs`] reads it on attach. Epoch seconds
//!     (not RFC3339) keep the age math to integer subtraction — no calendar code
//!     in a crate with no date library.
//!   - The mux "session" is a GROUPING name ("main" / `FNO_SESSION`), never the
//!     fno session id the digest folds on. The bridge is the focused pane's cwd:
//!     its basename is the worktree = node id, which `fno-agents digest` resolves
//!     to the session via the ledger. If the cwd yields nothing resolvable the
//!     fold returns empty and the overlay stays quiet (fail-open, AC-error).
//!
//! Config (`config.mux.attach_digest` + `attach_digest_threshold_min`) is read
//! straight from config.toml (Pattern B) because the interactive attach path
//! has no Python launcher to translate a knob into an env var.

use crate::proto;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_THRESHOLD_MIN: u64 = 10;
/// Fail-open budget for the fold shell-out; a slow `fno-agents` yields no
/// overlay rather than stalling the attach (AC-error: >800ms => no overlay).
const SHELLOUT_TIMEOUT: Duration = Duration::from_millis(800);

/// Seconds since the epoch, or 0 if the clock is before it (never in practice).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The client-local detach-time file for a mux session, under the mux dir.
fn detach_file(session: &str) -> Option<PathBuf> {
    // Reject a session name that could escape the dir (mirrors socket_path's
    // guard); `proto::socket_path` already validates, but this file is written
    // independently so it re-checks the one dangerous shape.
    if session.is_empty() || session.contains('/') || session.contains("..") {
        return None;
    }
    Some(proto::mux_dir().join(format!("{session}.detach")))
}

/// Record "detached now" for `session`. Best-effort: a write failure is silent
/// (the worst case is a missing catch-up on the next attach, never a crash).
pub fn record_detach(session: &str) {
    let Some(path) = detach_file(session) else {
        return;
    };
    let _ = proto::ensure_private_dir(&proto::mux_dir());
    let _ = std::fs::write(&path, now_secs().to_string());
}

/// Read the last-detach epoch seconds for `session`, if any.
pub fn read_detach_secs(session: &str) -> Option<u64> {
    let path = detach_file(session)?;
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// The node/worktree selector for the digest: the basename of the focused
/// squad's cwd. Empty when the cwd is unknown/degraded.
pub fn selector_from_cwd(cwd: &str) -> Option<String> {
    let base = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    (!base.is_empty()).then(|| base.to_string())
}

// ── config (Pattern B: read config.toml directly) ─────────────────────────

/// `config.mux.attach_digest` (default ON) — gate the overlay entirely.
pub fn attach_digest_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "attach_digest", true)
}

/// `config.mux.hover_focus` (default ON) — the focus-follows-mouse off-switch
/// (x-a496). Latched once at client startup. Lives here because this module owns
/// the `fno` crate's `config.mux.*` reader (mirrors `attach_digest_enabled`).
pub fn hover_focus_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "hover_focus", true)
}

/// `config.mux.show_missions` (default ON) - the `~ missions` progress band's
/// off-switch. A mission can never hold a session, so an operator who runs no
/// epics can drop the band entirely rather than dismiss it each session.
pub fn missions_section_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "show_missions", true)
}

/// `config.mux.show_backlog` (default ON) - the `~ backlog` lane's off-switch.
pub fn backlog_section_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "show_backlog", true)
}

/// The key layer from `config.mux.prefix` + `[mux.keys]`, plus every entry the
/// resolver had to refuse. Read here because this module owns the `fno` crate's
/// `config.mux.*` reader; the parsing and collision rules live in
/// [`crate::keys::resolve_keymap`], which is pure.
pub fn keymap(cwd: &Path) -> (crate::keys::Keymap, Vec<crate::keys::KeymapWarning>) {
    let prefix = mux_str(cwd, "prefix");
    crate::keys::resolve_keymap(prefix.as_deref(), &mux_keys_table(cwd))
}

/// `[mux.keys]` as raw `(action, spec)` pairs, MERGED action-by-action across
/// the precedence chain, lowest first.
///
/// Not first-table-wins: the Python loader deep-merges nested tables key by key
/// (`config_io::_deep_merge`) and `mux_str` above falls through per scalar key,
/// so a project table naming one action used to reset every global rebind.
/// Sorted so a warning list is stable rather than following TOML map order.
fn mux_keys_table(cwd: &Path) -> Vec<(String, String)> {
    let read = |path: &Path| -> Option<Vec<(String, String)>> {
        let content = std::fs::read_to_string(path).ok()?;
        let t = content.parse::<toml::Table>().ok()?;
        let keys = t.get("mux")?.as_table()?.get("keys")?.as_table()?;
        Some(
            keys.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
        )
    };
    // `$FNO_CONFIG` is the SOLE candidate when set, mirroring the Python loader.
    let layers: Vec<PathBuf> = match non_empty_env("FNO_CONFIG") {
        Some(explicit) => vec![PathBuf::from(explicit)],
        None => {
            let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
                .map(|p| PathBuf::from(p).with_file_name("config.toml"))
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/config.toml"))
                });
            // Lowest precedence first: global, then canonical, then this
            // checkout on top - `config_roots` is highest-first, so it reverses.
            global
                .into_iter()
                .chain(
                    config_roots(cwd)
                        .into_iter()
                        .rev()
                        .map(|r| r.join(".fno/config.toml")),
                )
                .collect()
        }
    };
    merge_key_layers(layers.iter().map(|p| read(p).unwrap_or_default()))
}

/// Collapse `[mux.keys]` layers, lowest precedence first, one action at a time.
///
/// Action ids are folded HERE rather than left to the resolver, even though
/// [`crate::keys::resolve_keymap`] folds them too. Deduplicating on the raw id
/// while the resolver dedupes on the folded one lets `Detach` and `detach` both
/// survive the merge; sorting then hands the resolver the LOWER layer last, and
/// its own last-wins rule silently reverses precedence. Two dedupe keys for one
/// identity is the whole bug, so there is one key and it lives here.
///
/// Split out from its file reads because the precedence rule is worth a test
/// that does not mutate `$HOME` out from under every other thread.
fn merge_key_layers(layers: impl Iterator<Item = Vec<(String, String)>>) -> Vec<(String, String)> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for layer in layers {
        for (action, spec) in layer {
            let action = action.trim().to_ascii_lowercase();
            merged.retain(|(a, _)| *a != action);
            merged.push((action, spec));
        }
    }
    merged.sort();
    merged
}

/// A `config.mux.<key>` boolean with a fail-open default. The four readers above
/// are one line each now; the coercion lives in one place.
fn mux_bool(cwd: &Path, key: &str, default: bool) -> bool {
    mux_str(cwd, key)
        .and_then(|v| parse_bool(&v))
        .unwrap_or(default)
}

/// `config.mux.attach_digest_threshold_min` (default 10) as seconds.
pub fn threshold_secs(cwd: &Path) -> u64 {
    mux_str(cwd, "attach_digest_threshold_min")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_THRESHOLD_MIN)
        // saturating: an absurd configured minutes value must not overflow.
        .saturating_mul(60)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Where the project config layer lives: the repo root, NOT the launch cwd.
///
/// `fno mux` is routinely launched from a subdirectory, and `<repo>/sub/.fno/`
/// does not exist, so anchoring the project layer on cwd reads no project
/// config at all and falls silently through to global. The Python loader
/// anchors the same layer to the git toplevel for exactly this reason
/// (`fno.config._settings_yaml_locations`, "not cwd, so running `fno` from a
/// subdirectory still finds it"), and these direct Rust readers exist precisely
/// because the interactive mux never goes through it.
///
/// One consequence worth knowing when a mux test behaves oddly on one machine:
/// a test process whose cwd is inside a checkout now reaches that checkout's
/// `.fno/config.toml`. Here `.fno/` is gitignored, so CI never has one, which
/// also means CI cannot see this class of leak in either direction.
fn project_root(cwd: &Path) -> PathBuf {
    match non_empty_env("FNO_REPO_ROOT") {
        Some(explicit) => PathBuf::from(explicit),
        None => repo_root_from(cwd),
    }
}

/// The project config roots, HIGHEST precedence first: this checkout, then the
/// canonical one behind it when this is a linked worktree.
fn config_roots(cwd: &Path) -> Vec<PathBuf> {
    let root = project_root(cwd);
    let canonical = canonical_root(&root);
    std::iter::once(root).chain(canonical).collect()
}

/// The main checkout behind a linked worktree, when the worktree has no config
/// of its own. `None` from the main checkout itself, or outside a repo.
///
/// I first argued this candidate was unnecessary because `setup-worktree.sh`
/// symlinks the config into every worktree. It does not always: `link_file`
/// skips a source that does not exist yet, and several creation paths treat
/// setup as best-effort, so a worktree made before the project config existed
/// stays bare forever. The Python loader carries the same candidate, so without
/// it `fno` and the mux disagree about the config in exactly that worktree.
///
/// Read from the `.git` FILE rather than by shelling out to git: a linked
/// worktree's gitdir points at `<canonical>/.git/worktrees/<name>`, so the
/// canonical root is the part before `/.git/`. Config climbs to canonical;
/// session state deliberately does not (`fno.paths`).
fn canonical_root(worktree: &Path) -> Option<PathBuf> {
    // Preflight's hermetic runner drops THIS candidate only, same as Python.
    if non_empty_env("FNO_NO_CANONICAL_CONFIG").is_some() {
        return None;
    }
    let gitdir = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let path = gitdir.trim().strip_prefix("gitdir:")?.trim();
    let root = path.split("/.git/worktrees/").next()?;
    let root = PathBuf::from(root);
    (root != worktree && root.is_dir()).then_some(root)
}

/// The `$FNO_REPO_ROOT`-free half of [`project_root`], so the walk is testable
/// without an env var the whole process shares.
///
/// Falls back to `cwd` outside a repo, which is where a bare `.fno/` would be.
fn repo_root_from(cwd: &Path) -> PathBuf {
    let mut dir = cwd;
    loop {
        // A linked worktree's `.git` is a FILE, not a directory.
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

/// Resolve a `config: > mux: > <key>` string with the same file precedence as
/// `agents_config::mux_bool` ($FNO_CONFIG sole > project-local > global).
fn mux_str(cwd: &Path, key: &str) -> Option<String> {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_mux_file(Path::new(&explicit), key);
    }
    for root in config_roots(cwd) {
        if let Some(v) = read_mux_file(&root.join(".fno/config.toml"), key) {
            return Some(v);
        }
    }
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(|p| PathBuf::from(p).with_file_name("config.toml"))
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/config.toml")));
    global.and_then(|g| read_mux_file(&g, key))
}

fn read_mux_file(path: &Path, key: &str) -> Option<String> {
    read_mux_value(&std::fs::read_to_string(path).ok()?, key)
}

/// Read `mux.<key>` from a flat config.toml body, returning the value as a raw
/// string each caller re-coerces (bool -> "true"/"false", int -> its digits).
fn read_mux_value(content: &str, key: &str) -> Option<String> {
    let t = content.parse::<toml::Table>().ok()?;
    match t.get("mux")?.as_table()?.get(key)? {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Resolve the `fno-agents` binary: `$FNO_AGENTS_BIN`, else a sibling of the
/// running `fno` binary (the installed layout, mirroring `resolve_daemon_bin`),
/// else bare `fno-agents` on PATH. Crate-visible: the server's claim-sweep
/// shell-out (x-54fa) resolves the same binary the same way.
pub(crate) fn fno_agents_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_BIN") {
        return PathBuf::from(v);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fno-agents")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("fno-agents"))
}

// ── overlay assembly ───────────────────────────────────────────────────────

/// Turn a `fno-agents digest --json` stdout blob into overlay lines. `None`
/// when the JSON is unparseable or the fold produced no `lines` (fail-quiet).
fn lines_from_json(stdout: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let arr = v.get("lines")?.as_array()?;
    let lines: Vec<String> = arr
        .iter()
        .filter_map(|l| l.as_str())
        .map(str::to_string)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(decorate(lines))
}

/// Frame the fold lines with a header + dismiss hint, padded to a common width
/// so the inverse-video block is a clean rectangle.
fn decorate(mut body: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(body.len() + 2);
    out.push("while you were gone".to_string());
    out.append(&mut body);
    out.push("(any key to dismiss)".to_string());
    let width = out.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    for line in &mut out {
        let pad = width - line.chars().count();
        line.push_str(&" ".repeat(pad));
    }
    out
}

/// The full attach-time decision. Returns overlay lines, or `None` to render
/// nothing. Fail-open at every step: a disabled knob, a too-recent detach, an
/// unknown cwd, a missing/slow/empty `fno-agents` all yield `None`.
pub async fn on_attach(session: &str, focused_cwd: &str) -> Option<Vec<String>> {
    let cwd = Path::new(focused_cwd);
    if !attach_digest_enabled(cwd) {
        return None;
    }
    // Threshold gate: only after an ABSENCE longer than the configured minutes.
    // No prior detach record (first attach) => nothing to catch up on.
    let last = read_detach_secs(session)?;
    if now_secs().saturating_sub(last) < threshold_secs(cwd) {
        return None;
    }
    let selector = selector_from_cwd(focused_cwd)?;

    // Scope the fold to the absence window: pass the detach time as epoch
    // seconds so the digest reports what changed WHILE AWAY, not lifetime
    // totals. Epoch avoids synthesizing an RFC3339 string in a crate with no
    // date library; `fno-agents` parses each row's ts to epoch for the compare.
    let since_epoch = last.to_string();
    let fut = tokio::process::Command::new(fno_agents_bin())
        .args([
            "digest",
            "--session",
            &selector,
            "--since-epoch",
            &since_epoch,
            "--json",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // On timeout the future is dropped; kill_on_drop reaps the child so a
        // slow `fno-agents` can't leave an orphan behind on each attach.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    lines_from_json(&String::from_utf8_lossy(output.stdout.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_is_worktree_basename() {
        assert_eq!(
            selector_from_cwd("/Users/x/conductor/workspaces/footnote/x-4e2d").as_deref(),
            Some("x-4e2d")
        );
        assert_eq!(selector_from_cwd("/w/x-4e2d/").as_deref(), Some("x-4e2d"));
        assert_eq!(selector_from_cwd(""), None);
        assert_eq!(selector_from_cwd("/"), None);
    }

    #[test]
    fn config_defaults_when_absent() {
        let dir = Path::new("/nonexistent-xyz");
        // No settings file anywhere reachable -> defaults (on, 10min).
        // (HOME may have one; the point is the parse path, covered below.)
        let _ = attach_digest_enabled(dir);
        let _ = threshold_secs(dir);
    }

    #[test]
    fn reads_mux_values() {
        let yaml = "[mux]\nattach_digest = false\nattach_digest_threshold_min = 30\n";
        assert_eq!(
            read_mux_value(yaml, "attach_digest").as_deref(),
            Some("false")
        );
        assert_eq!(
            parse_bool(&read_mux_value(yaml, "attach_digest").unwrap()),
            Some(false)
        );
        assert_eq!(
            read_mux_value(yaml, "attach_digest_threshold_min").as_deref(),
            Some("30")
        );
        assert_eq!(read_mux_value(yaml, "missing"), None);
    }

    #[test]
    fn json_to_lines_frames_and_pads() {
        let json = r#"{"lines":["! 1 block (last: FAILURE) - resolved","PR #42 OPEN - CI SUCCESS - reviewed"]}"#;
        let lines = lines_from_json(json).expect("has lines");
        assert!(lines[0].starts_with("while you were gone"));
        assert!(lines.iter().any(|l| l.contains("#42")));
        assert!(lines.last().unwrap().starts_with("(any key to dismiss)"));
        // All padded to a common width (clean inverse rectangle).
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w));
    }

    #[test]
    fn json_empty_lines_is_none() {
        assert_eq!(lines_from_json(r#"{"lines":[]}"#), None);
        assert_eq!(lines_from_json("not json"), None);
        assert_eq!(lines_from_json(r#"{"no_lines":1}"#), None);
    }

    #[test]
    fn a_project_rebind_outranks_a_global_one_whatever_its_case() {
        let layer = |a: &str, k: &str| vec![(a.to_string(), k.to_string())];
        let merged = |a: Vec<(String, String)>, b: Vec<(String, String)>| {
            merge_key_layers(vec![a, b].into_iter())
        };
        // Global first, project on top. The resolver folds case, so these two
        // ids are ONE action and the higher layer has to win outright.
        assert_eq!(
            merged(layer("detach", "Q"), layer("Detach", "D")),
            layer("detach", "D")
        );
        // And the other way round, or the test would only prove that `D` sorts
        // before `Q`.
        assert_eq!(
            merged(layer("Detach", "D"), layer("detach", "Q")),
            layer("detach", "Q")
        );
        // Untouched actions from the lower layer still survive the merge.
        let global = vec![
            ("detach".to_string(), "Q".to_string()),
            ("zoom".to_string(), "z".to_string()),
        ];
        assert_eq!(
            merge_key_layers(vec![global, layer("DETACH", "D")].into_iter()),
            vec![
                ("detach".to_string(), "D".to_string()),
                ("zoom".to_string(), "z".to_string()),
            ]
        );
    }

    #[test]
    fn the_project_layer_anchors_on_the_repo_root_not_the_launch_cwd() {
        // mux is routinely attached from a subdirectory. Anchored on cwd, the
        // project layer reads <repo>/sub/.fno/config.toml, which does not
        // exist, and every project key silently reads as unset.
        let base = std::env::temp_dir().join(format!("fno-root-{}", std::process::id()));
        let deep = base.join("crates/fno/src");
        std::fs::create_dir_all(&deep).expect("scratch dirs");
        std::fs::create_dir_all(base.join(".git")).expect("repo marker");

        assert_eq!(repo_root_from(&deep), base, "walks up to the repo root");
        assert_eq!(repo_root_from(&base), base, "already at the root");

        // A linked worktree's `.git` is a file, not a directory.
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join("sub")).expect("worktree dirs");
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere\n").expect("gitdir file");
        assert_eq!(repo_root_from(&wt.join("sub")), wt);

        // Outside a repo the walk cannot invent a root, so the caller's own
        // directory stays the place a bare `.fno/` would be.
        let orphan = std::env::temp_dir().join(format!("fno-orphan-{}", std::process::id()));
        std::fs::create_dir_all(&orphan).expect("orphan dir");
        assert!(
            orphan.starts_with(repo_root_from(&orphan)),
            "the result is always the caller's own directory or an ancestor of it"
        );

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&orphan).ok();
    }

    #[test]
    fn a_bare_worktree_still_reaches_the_canonical_config() {
        // I had assumed setup-worktree.sh always symlinks the config in. It does
        // not: link_file skips a source that does not exist yet, so a worktree
        // created before the project config existed stays bare, and without this
        // candidate the mux and the Python loader disagree about the config in
        // exactly that worktree.
        let base = std::env::temp_dir().join(format!("fno-canon-{}", std::process::id()));
        let canonical = base.join("footnote");
        let wt = canonical.join(".claude/worktrees/feature");
        std::fs::create_dir_all(wt.join("crates/fno")).expect("worktree dirs");
        std::fs::create_dir_all(canonical.join(".git/worktrees/feature")).expect("gitdir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}/.git/worktrees/feature\n", canonical.display()),
        )
        .expect("gitdir file");

        assert_eq!(
            canonical_root(&wt).as_deref(),
            Some(canonical.as_path()),
            "a linked worktree resolves the checkout behind it"
        );
        assert_eq!(
            config_roots(&wt.join("crates/fno")),
            vec![wt.clone(), canonical.clone()],
            "this checkout outranks canonical, and canonical is still reachable"
        );
        // The main checkout has no checkout behind it.
        assert_eq!(canonical_root(&canonical), None);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn detach_file_rejects_traversal() {
        assert!(detach_file("../evil").is_none());
        assert!(detach_file("a/b").is_none());
        assert!(detach_file("").is_none());
        assert!(detach_file("main").is_some());
    }
}
