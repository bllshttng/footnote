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

/// (x-f75e) `config.mux.theme`: the chrome palette name, latched once at client
/// startup. An unset key reads as `None` (meaning "no preference") and resolves
/// to `terminal`. An UNKNOWN name also resolves to `terminal` but carries a
/// notice through the same channel a refused keymap rebind uses, because a
/// config that is quietly ignored is indistinguishable from one never written.
pub fn theme_for(cwd: &Path) -> (crate::theme::Theme, Option<crate::keys::KeymapWarning>) {
    match mux_str(cwd, "theme") {
        Some(name) => crate::theme::Theme::from_name(&name),
        None => (crate::theme::Theme::default_theme(), None),
    }
}

/// The key layer from `config.mux.prefix` + `[mux.keys]`, plus every entry the
/// resolver had to refuse. Read here because this module owns the `fno` crate's
/// `config.mux.*` reader; the parsing and collision rules live in
/// [`crate::keys::resolve_keymap`], which is pure.
pub fn keymap(cwd: &Path) -> (crate::keys::Keymap, Vec<crate::keys::KeymapWarning>) {
    let prefix = mux_str(cwd, "prefix");
    let (entries, read_warnings) = mux_keys_table(cwd);
    let (map, mut warnings) = crate::keys::resolve_keymap(prefix.as_deref(), &entries);
    // Read-time problems first: a file that would not parse explains every
    // rebind that is missing, so it outranks any one refused entry.
    for w in read_warnings.into_iter().rev() {
        warnings.insert(0, w);
    }
    if let Some(w) = unreadable_explicit_config() {
        warnings.insert(0, w);
    }
    (map, warnings)
}

/// A warning when `$FNO_CONFIG` names a file this reader cannot parse.
///
/// The Python loader reads an explicitly pinned file AS-IS and parses YAML by
/// suffix (`config_io::_load_raw`), while every Rust reader here is TOML-only.
/// So a `.yaml` pin leaves `fno config` showing values the mux never sees.
///
/// TOML-only is the settled convention rather than an oversight
/// (`fno_agents::agents_config` documents the same choice and warns from
/// `warn_once_if_yaml`), so this matches it instead of adding a YAML parser and
/// a third way to read one file. It says so LOUDER than its sibling, though: a
/// stderr line is invisible under a TUI that is about to take the terminal, and
/// the mux has a notice surface, so the refusal rides the same channel as a
/// refused rebind. Silence is the failure being fixed.
fn unreadable_explicit_config() -> Option<crate::keys::KeymapWarning> {
    warn_if_not_toml(Path::new(&non_empty_env("FNO_CONFIG")?))
}

/// The suffix rule, split from the env read so it is testable without mutating
/// a variable the whole process shares.
///
/// A file with NO extension is left alone: Python would parse it as YAML, but a
/// `.toml`-less pin is more likely a deliberate path than a mistake, and a
/// warning that cries wolf on every run teaches operators to ignore the channel
/// this whole notice depends on.
fn warn_if_not_toml(path: &Path) -> Option<crate::keys::KeymapWarning> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    // Meaning first, path last, and the meaning fits 38 columns exactly. The
    // notice strip clips from the right on a 40-column terminal, which is the
    // supported minimum, so a path-first message rendered there as a truncated
    // path and nothing else: technically a warning, practically silence.
    (!ext.eq_ignore_ascii_case("toml")).then(|| {
        crate::keys::KeymapWarning(format!(
            "keys on defaults: $FNO_CONFIG not TOML ({})",
            path.display()
        ))
    })
}

/// `[mux.keys]` as raw `(action, spec)` pairs, MERGED action-by-action across
/// the precedence chain, lowest first.
///
/// Not first-table-wins: the Python loader deep-merges nested tables key by key
/// (`config_io::_deep_merge`) and `mux_str` above falls through per scalar key,
/// so a project table naming one action used to reset every global rebind.
/// Sorted so a warning list is stable rather than following TOML map order.
/// Nothing here drops an entry quietly. A value the reader cannot use and a file
/// it cannot parse both leave with a warning, because "your key did nothing and
/// nothing said why" is the failure the whole notice channel exists to end - the
/// same one a refused rebind, a YAML pin and a clipped notice each produced.
fn mux_keys_table(cwd: &Path) -> (Vec<(String, String)>, Vec<crate::keys::KeymapWarning>) {
    let mut warnings: Vec<crate::keys::KeymapWarning> = Vec::new();
    let mut read = |path: &Path| -> Vec<(String, String)> {
        let (entries, mut found) = read_layer(path);
        warnings.append(&mut found);
        entries
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
    let merged = merge_key_layers(layers.iter().map(|p| read(p)));
    (merged, warnings)
}

/// One config layer: its `[mux.keys]`, plus everything about it that stopped
/// the reader.
///
/// Only `NotFound` is absence. Every other read error - permissions, invalid
/// UTF-8, an I/O fault - means a file the operator DOES have and the mux cannot
/// use, and collapsing those into "no config here" is the same silent drop as
/// discarding a value the resolver should have refused. `mux_str` reads these
/// same paths for the prefix and the other knobs, so one warning per file
/// covers both rather than firing once per key.
fn read_layer(path: &Path) -> (Vec<(String, String)>, Vec<crate::keys::KeymapWarning>) {
    match std::fs::read_to_string(path) {
        Ok(content) => keys_from_toml(path, &content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(e) => (
            Vec::new(),
            vec![crate::keys::KeymapWarning(format!(
                "keys on defaults: cannot read {} ({e})",
                path.display()
            ))],
        ),
    }
}

/// One config file's `[mux.keys]`, plus everything it could not use.
///
/// Split from the file read so both refusals are testable without a scratch
/// directory or a process-wide variable.
fn keys_from_toml(
    path: &Path,
    content: &str,
) -> (Vec<(String, String)>, Vec<crate::keys::KeymapWarning>) {
    let mut warnings = Vec::new();
    let Ok(table) = content.parse::<toml::Table>() else {
        // Python logs on a parse failure for exactly this reason. The mux says
        // it on screen instead, because a malformed file defaults EVERY key and
        // an operator staring at a dead keyboard has nowhere else to look.
        warnings.push(crate::keys::KeymapWarning(format!(
            "keys on defaults: bad TOML in {}",
            path.display()
        )));
        return (Vec::new(), warnings);
    };
    let Some(keys) = table
        .get("mux")
        .and_then(|m| m.as_table())
        .and_then(|m| m.get("keys"))
        .and_then(|k| k.as_table())
    else {
        return (Vec::new(), warnings);
    };
    let entries = keys
        .iter()
        .filter_map(|(k, v)| match v.as_str() {
            Some(s) => Some((k.clone(), s.to_string())),
            None => {
                // `detach = 3` parses as valid TOML and is not a key. Dropping
                // it at this boundary robbed the resolver of the chance to
                // refuse it, so the entry vanished with nothing said.
                warnings.push(crate::keys::KeymapWarning(format!(
                    "config.mux.keys.{k}: needs a quoted key, not {v}"
                )));
                None
            }
        })
        .collect();
    (entries, warnings)
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
    config_roots_with(cwd, canonical_suppressed_by_env())
}

/// [`config_roots`] with the suppression decision passed in, so a test can
/// exercise the walk without depending on an env var it does not control.
fn config_roots_with(cwd: &Path, suppressed: bool) -> Vec<PathBuf> {
    let root = project_root(cwd);
    let canonical = canonical_root_with(&root, suppressed);
    std::iter::once(root).chain(canonical).collect()
}

/// Whether `$FNO_NO_CANONICAL_CONFIG` drops the canonical candidate.
///
/// Split from the env read so the rule is testable without mutating a variable
/// the whole process shares.
fn canonical_suppressed(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == std::ffi::OsStr::new("1"))
}

/// The live env read, in ONE place. Everything below takes the answer as an
/// argument, so a test never inherits it: `scripts/ci/preflight.sh` exports
/// `FNO_NO_CANONICAL_CONFIG=1` for its hermetic runner, which used to make the
/// canonical-candidate test assert `Some(...)` against a function forced to
/// return `None` - green everywhere except the one gate that runs before a
/// push.
fn canonical_suppressed_by_env() -> bool {
    canonical_suppressed(std::env::var_os("FNO_NO_CANONICAL_CONFIG").as_deref())
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
///
/// `fno_agents::paths::canonical_repo_root` answers the same question by
/// running `git worktree list --porcelain`. This is a second implementation
/// because crate `fno` does not depend on `fno-agents`, and the two differ in
/// method, not only in code: this one cannot see a repo whose git dir lives
/// outside the checkout, and it costs no subprocess on the attach path. Change
/// one and check the other.
///
/// `suppressed` is passed IN rather than read here, so a test never inherits
/// the env: `canonical_suppressed_by_env` is the one live read, in `config_roots`.
///
/// Preflight's hermetic runner drops THIS candidate only. Exactly "1"; any
/// other value is inert, matching `config/__init__.py` and
/// `fno_agents::agents_config` verbatim. A third reader with its own idea of
/// truthiness would resurrect the very split-brain this candidate fixes, just
/// for operators who set it to "0" or "true".
fn canonical_root_with(worktree: &Path, suppressed: bool) -> Option<PathBuf> {
    if suppressed {
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
    fn an_unreadable_config_is_not_the_same_as_an_absent_one() {
        // Absence is the normal case and stays quiet. Every OTHER read error is
        // a file the operator DOES have and the mux cannot use, and collapsing
        // those into "no config here" is the same silent drop as discarding a
        // value the resolver should have refused.
        let dir = std::env::temp_dir().join(format!("fno-readlayer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");

        let absent = dir.join("config.toml");
        assert_eq!(read_layer(&absent), (Vec::new(), Vec::new()), "quiet");

        // Invalid UTF-8 reproduces the class deterministically, with no
        // permission games that a root-run CI would skip straight past.
        let bad = dir.join("invalid-utf8.toml");
        std::fs::write(&bad, [0xff, 0xfe, 0x00]).expect("write");
        let (entries, warnings) = read_layer(&bad);
        assert!(entries.is_empty());
        assert_eq!(warnings.len(), 1, "must not read as absent: {warnings:?}");
        assert!(
            warnings[0].0.starts_with("keys on defaults: cannot read"),
            "meaning first, so a 40-column strip keeps it: {:?}",
            warnings[0].0
        );

        // A directory where a file belongs is the other everyday shape.
        let as_dir = dir.join("adir.toml");
        std::fs::create_dir_all(&as_dir).expect("dir");
        assert_eq!(read_layer(&as_dir).1.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_the_key_reader_cannot_use_disappears_quietly() {
        let p = Path::new("/tmp/config.toml");

        // A value that is not a string parses as valid TOML and is not a key.
        // Filtering it out here left the resolver nothing to refuse, so the
        // entry vanished and the keyboard silently kept its default.
        let (entries, warnings) =
            keys_from_toml(p, "[mux.keys]\ndetach = 3\nzoom = false\nfind = \"F\"\n");
        assert_eq!(entries, vec![("find".to_string(), "F".to_string())]);
        assert_eq!(warnings.len(), 2, "both bad values reported: {warnings:?}");
        for want in ["detach", "zoom"] {
            assert!(
                warnings.iter().any(|w| w.0.contains(want)),
                "{want} must be named: {warnings:?}"
            );
        }

        // A file that will not parse defaults EVERY key, so it says so once
        // rather than once per missing binding.
        let (entries, warnings) = keys_from_toml(p, "[mux.keys\ndetach = \"Q\"\n");
        assert!(entries.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].0.contains("bad TOML") && warnings[0].0.contains("defaults"),
            "and says what the operator is getting: {:?}",
            warnings[0].0
        );

        // The ordinary cases stay quiet: a good file, and one with no [mux.keys]
        // at all, which is what almost every config looks like.
        assert_eq!(keys_from_toml(p, "[mux.keys]\ndetach = \"Q\"\n").1.len(), 0);
        assert_eq!(keys_from_toml(p, "[mux]\nhover_focus = false\n").1.len(), 0);
    }

    #[test]
    fn a_yaml_fno_config_says_so_instead_of_reading_as_empty() {
        // Python reads an explicitly pinned file as-is and parses YAML by
        // suffix, while every Rust reader here is TOML-only. That combination
        // used to mean `fno config` showed values the mux silently never saw.
        // TOML-only stays (fno_agents::agents_config settled the same way); what
        // changes is that it says so.
        for yaml in ["/tmp/settings.yaml", "/tmp/settings.yml", "/tmp/x.YAML"] {
            let w = warn_if_not_toml(Path::new(yaml))
                .unwrap_or_else(|| panic!("{yaml} must warn, not read as empty"));
            assert!(w.0.contains(yaml), "the warning names the file: {}", w.0);
            // Meaning BEFORE the path: the notice strip clips from the right, so
            // a path-first message is a truncated path and nothing else on a
            // 40-column terminal.
            let head: String = w.0.chars().take(38).collect();
            assert!(
                head.contains("defaults") && head.contains("not TOML"),
                "the first 38 columns must carry the meaning, got {head:?}"
            );
        }
        assert!(warn_if_not_toml(Path::new("/tmp/config.toml")).is_none());
        assert!(warn_if_not_toml(Path::new("/tmp/config.TOML")).is_none());
        // No extension is left alone rather than warned about on every run.
        assert!(warn_if_not_toml(Path::new("/tmp/fnoconfig")).is_none());
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
        // Deliberately not the real harness worktree location: the gitdir parse
        // is what is under test, and spelling that location out would add the
        // kind of hardcoded path literal the placement lint exists to prevent.
        let wt = canonical.join("wt/feature");
        std::fs::create_dir_all(wt.join("crates/fno")).expect("worktree dirs");
        std::fs::create_dir_all(canonical.join(".git/worktrees/feature")).expect("gitdir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}/.git/worktrees/feature\n", canonical.display()),
        )
        .expect("gitdir file");

        // Suppression is passed IN, never inherited: preflight's hermetic
        // runner exports FNO_NO_CANONICAL_CONFIG=1, which forced the plain
        // `canonical_root` to None and made this test fail in the one place it
        // most needed to run - the gate before a push. Taking it as an argument
        // also means these asserts cannot race a sibling test mutating a
        // process-wide variable.
        assert_eq!(
            canonical_root_with(&wt, false).as_deref(),
            Some(canonical.as_path()),
            "a linked worktree resolves the checkout behind it"
        );
        assert_eq!(
            config_roots_with(&wt.join("crates/fno"), false),
            vec![wt.clone(), canonical.clone()],
            "this checkout outranks canonical, and canonical is still reachable"
        );
        // The main checkout has no checkout behind it.
        assert_eq!(canonical_root_with(&canonical, false), None);
        // ...and the preflight seam really does drop the candidate.
        assert_eq!(
            canonical_root_with(&wt, true),
            None,
            "FNO_NO_CANONICAL_CONFIG=1 drops the canonical candidate"
        );
        assert_eq!(
            config_roots_with(&wt.join("crates/fno"), true),
            vec![wt.clone()],
            "and leaves this checkout as the only root"
        );

        // Only the exact value "1" suppresses the candidate, matching
        // config/__init__.py ("Exactly \"1\"; any other value inert") and
        // fno_agents::agents_config. A third reader with its own idea of
        // truthiness would recreate the split-brain for anyone who set it to
        // "0" or "true" meaning to turn it OFF.
        use std::ffi::OsStr;
        assert!(canonical_suppressed(Some(OsStr::new("1"))));
        for inert in ["0", "true", "2", "", "yes"] {
            assert!(
                !canonical_suppressed(Some(OsStr::new(inert))),
                "{inert:?} must be inert, not a suppression"
            );
        }
        assert!(!canonical_suppressed(None));

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
