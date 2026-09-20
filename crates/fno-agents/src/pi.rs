//! pi's session identity: where a session lives on disk, what names it, and
//! what a duplicate looks like.
//!
//! pi is a DUAL-LANE harness. `pi --mode rpc` is the driving lane, and a plain
//! interactive `pi` on the same session id is the watching lane, which JOINS
//! the rpc session rather than starting a rival one. Both lanes address the
//! same session, so both need the same answer to "which session is this?".
//!
//! The whole of that answer is the PAIR `(cwd, session_id)`. pi stores sessions
//! under a cwd-scoped directory, so the same id in two worktrees is two
//! different sessions and a resume from the canonical checkout cannot see a
//! session started in a worktree.
//!
//! # The create hazard this module exists to make visible
//!
//! `--session-id` adopts an existing session and creates one when it is
//! absent, and nothing in the flag, the output, or the exit code says which of
//! the two it did. Four simultaneous creates on one id produced four session
//! files 49ms apart, all four exiting 0 and all four internally perfect; a
//! later resume of that id picked the OLDEST and named none of the rest.
//! Serialising that decision is fno's job and lives in the Python spawn lane
//! (`fno.agents.harnesses.pi`), which holds an `fno agents claim` across the
//! create only. This module supplies the reading half: what is on disk now.

use std::path::{Path, PathBuf};

/// pi's provider for this fleet. `--provider` alone is not enough:
/// `--provider openai-codex` WITHOUT `--model` does not resolve to gpt-5.5, it
/// falls through to a Bedrock model and dies with "Token is expired. To refresh
/// this SSO session run 'aws sso login'", which names AWS and misdirects
/// completely. Always pass both. Overridable by env so a different subscription
/// does not need a rebuild.
pub const PI_DEFAULT_PROVIDER: &str = "openai-codex";
/// pi's model for this fleet. See [`PI_DEFAULT_PROVIDER`] for why it is never
/// omitted.
pub const PI_DEFAULT_MODEL: &str = "gpt-5.5";

/// The route a pi launch carries: the argv tokens, which input decided them,
/// and the posture note every receipt names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiRoute {
    pub tokens: Vec<String>,
    pub route_source: &'static str,
    pub note: String,
}

/// The ONE pi route for keeper and pane. An explicit model carrying `/` is a
/// `provider/id` pattern pi reads the provider from, so `--model` alone; a
/// bare explicit model carries both flags (the Bedrock trap); no model
/// defers to pi's own settings when they name BOTH a provider and a model,
/// else fno's default pair. Effort, tools and deny-tools map to pi's own
/// first-class flags.
pub fn pi_route(model: &str, effort: &str, tools: &str, deny_tools: &str) -> PiRoute {
    pi_route_in(model, effort, tools, deny_tools, &pi_agent_dir())
}

/// [`pi_route`] against an explicit agent dir, so a test drives the
/// settings branch on a scratch tree.
pub fn pi_route_in(
    model: &str,
    effort: &str,
    tools: &str,
    deny_tools: &str,
    agent_dir: &Path,
) -> PiRoute {
    const POSTURE: &str = "pi runs unsandboxed and shows no approval prompts";
    let mut tokens: Vec<String> = Vec::new();
    let route_source;
    let model = model.trim();
    let mut damage = String::new();
    if model.is_empty() {
        match read_route_settings(agent_dir) {
            Ok((provider, model_setting)) if provider.is_some() && model_setting.is_some() => {
                // pi resolves provider and model from its own settings:
                // naming either would only override the user's configured
                // default.
                route_source = "pi-settings";
            }
            Ok(_) => {
                tokens = vec![
                    "--provider".to_string(),
                    pi_provider(),
                    "--model".to_string(),
                    pi_model(),
                ];
                route_source = "fno-default";
            }
            Err(reason) => {
                // The settings file exists but says nothing usable; the
                // launch still needs a route, so the fno default pair rides
                // and the damage names itself on the receipt.
                tokens = vec![
                    "--provider".to_string(),
                    pi_provider(),
                    "--model".to_string(),
                    pi_model(),
                ];
                route_source = "fno-default";
                damage = format!("; pi settings unreadable: {reason}");
            }
        }
    } else if model.contains('/') {
        tokens = vec!["--model".to_string(), model.to_string()];
        route_source = "explicit";
    } else {
        tokens = vec![
            "--provider".to_string(),
            pi_provider(),
            "--model".to_string(),
            model.to_string(),
        ];
        route_source = "explicit";
    }
    for (flag, value) in [
        ("--thinking", effort.trim()),
        ("--tools", tools.trim()),
        ("--exclude-tools", deny_tools.trim()),
    ] {
        if !value.is_empty() {
            tokens.push(flag.to_string());
            tokens.push(value.to_string());
        }
    }
    PiRoute {
        tokens,
        route_source,
        note: format!("{POSTURE}{damage}"),
    }
}

/// `(defaultProvider, defaultModel)` from the agent dir's settings.json.
/// `Ok((None, None))` when the file is absent or names neither; `Err`
/// naming the file when it exists but cannot be read or parsed, so a
/// damaged settings file is VISIBLE in the route note instead of silently
/// routing the launch to the fno default pair.
fn read_route_settings(agent_dir: &Path) -> Result<(Option<String>, Option<String>), String> {
    let path = agent_dir.join("settings.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(e) => return Err(format!("{} is unreadable: {e}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let get = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Ok((get("defaultProvider"), get("defaultModel")))
}

/// The provider fno passes to pi, `FNO_PI_PROVIDER` winning over the default.
pub fn pi_provider() -> String {
    env_or("FNO_PI_PROVIDER", PI_DEFAULT_PROVIDER)
}

/// The model fno passes to pi, `FNO_PI_MODEL` winning over the default.
pub fn pi_model() -> String {
    env_or("FNO_PI_MODEL", PI_DEFAULT_MODEL)
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// pi's agent dir: `PI_CODING_AGENT_DIR` when set, a leading `~` expanded,
/// else `$HOME/.pi/agent`. That env var is the one pi itself reads; the
/// store-root var this module's readers honored before is read by no pi
/// code at all, so a relocated install read the wrong tree on every store question.
pub fn pi_agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            if let Some(rest) = dir.strip_prefix("~/") {
                return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest);
            }
            return PathBuf::from(dir);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".pi")
        .join("agent")
}

/// How a session store lays sessions out. pi's default store scopes one
/// directory per cwd; a set session dir (`--session-dir`,
/// `PI_CODING_AGENT_SESSION_DIR`, settings `sessionDir`) is FLAT: pi uses it
/// as-is and never appends the cwd segment, so matching a session to a cwd
/// means reading each candidate file's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreLayout {
    /// `<root>/--<cwd>--/<ts>_<id>.jsonl`, pi's default layout.
    CwdScoped,
    /// `<root>/<ts>_<id>.jsonl`, each file's header carrying its cwd.
    Flat,
}

/// pi's session store as THIS fno process resolves it: where it lives, how it
/// lays sessions out, and which input decided that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiStore {
    pub root: PathBuf,
    pub layout: StoreLayout,
    pub source: &'static str,
}

impl PiStore {
    /// A CwdScoped store at `root`: the scratch shape a test injects.
    pub fn cwd_scoped(root: PathBuf) -> PiStore {
        PiStore {
            root,
            layout: StoreLayout::CwdScoped,
            source: "test",
        }
    }
}

/// Resolve pi's session store the way pi itself does, for one `cwd`:
///
/// 1. `PI_CODING_AGENT_SESSION_DIR` (flat, source `env`).
/// 2. `<cwd>/.pi/settings.json` naming `sessionDir`: an `Err`, because pi
///    honors a project setting only after its own project-trust decision,
///    which fno cannot read. Reporting a store here could read a store pi
///    is not using.
/// 3. `<agent dir>/settings.json` naming `sessionDir` (flat, source
///    `settings`; a relative value joins to `cwd`).
/// 4. `<agent dir>/sessions` (cwd-scoped, source `default`).
///
/// An unparseable settings file is an `Err` naming the file and the serde
/// position: reading past a damaged input could answer from a store pi is
/// not using.
pub fn pi_store(cwd: &Path) -> Result<PiStore, String> {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PiStore {
                root: PathBuf::from(dir),
                layout: StoreLayout::Flat,
                source: "env",
            });
        }
    }
    let project = cwd.join(".pi").join("settings.json");
    if let Some(value) = read_session_dir_setting(&project)? {
        return Err(format!(
            "{} names sessionDir {value:?}; pi decides this through project trust; fno cannot read that decision",
            project.display()
        ));
    }
    let global = pi_agent_dir().join("settings.json");
    if let Some(value) = read_session_dir_setting(&global)? {
        let path = PathBuf::from(&value);
        let root = if path.is_relative() {
            cwd.join(path)
        } else {
            path
        };
        return Ok(PiStore {
            root,
            layout: StoreLayout::Flat,
            source: "settings",
        });
    }
    Ok(PiStore {
        root: pi_agent_dir().join("sessions"),
        layout: StoreLayout::CwdScoped,
        source: "default",
    })
}

/// `sessionDir` from a settings file: `Ok(None)` when the file is absent or
/// names none, an `Err` when it exists but does not parse.
fn read_session_dir_setting(path: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} is unreadable: {e}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    Ok(value
        .get("sessionDir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// pi's on-disk encoding of a working directory: every path separator and the
/// colon become a single `-`, and the result is fenced with `--` at both ends.
///
/// Derived from live directories and pi's own encoder:
///
/// ```text
/// /Users/bb16/code/footnote/footnote  -> --Users-bb16-code-footnote-footnote--
/// /private/tmp                        -> --private-tmp--
/// /a/b:c                              -> --a-b-c--
/// ```
///
/// A third observed directory, a probe run under a dot-prefixed component,
/// showed that a DOT inside a component survives unchanged: only the
/// separators and the colon are rewritten.
pub fn encode_cwd(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let body = raw.trim_start_matches('/').replace(['/', '\\', ':'], "-");
    format!("--{body}--")
}

/// What a lookup of one `(cwd, session_id)` pair found on disk.
///
/// `Unknown` is a first-class outcome and never collapses into `None`. Two
/// different facts produce an empty answer and they call for opposite actions:
///
///   * the session directory does not exist, so this reading cannot see
///     anything and must not be read as "no duplicates";
///   * the directory exists and holds no file for this id, which is a real
///     `None`, and still does not prove the session is absent (see below).
///
/// A pi session's file materialises at the FIRST TURN ATTEMPT, not at create.
/// A live rpc session held twelve seconds with no prompt sent leaves the
/// directory empty. So a `None` from a directory that exists still means "no
/// turn has been attempted yet", never "no session". The instrument that covers
/// that blind window is fno's own claim registry, which records at acquire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLookup {
    /// The session directory is not readable, so this reading proves nothing.
    Unknown { dir: PathBuf, reason: String },
    /// No file for this id. Not proof the session is absent.
    None,
    /// Exactly one session file carries this id.
    One { file: PathBuf },
    /// More than one session file carries this id. Every one of them is named,
    /// oldest first, and no caller may pick between them.
    Duplicate { files: Vec<PathBuf> },
}

/// Read the session files for one `(cwd, session_id)` pair, oldest first.
///
/// Ordering is by FILENAME, which carries an ISO-8601 timestamp prefix
/// (`<ISO>_<session-id>.jsonl`), so a lexicographic sort is chronological and
/// needs no stat call and no parse.
///
/// Ranking by CONTENT is forbidden and this function deliberately gives a
/// caller no means to do it. An empty assistant `content` array marks a turn
/// that was ATTEMPTED AND FAILED, not an idle or empty session, so preferring
/// the "fuller" file discards the one that errored, which is usually the one a
/// human needs to read.
pub fn lookup_sessions(cwd: &Path, session_id: &str) -> SessionLookup {
    let store = match pi_store(cwd) {
        Ok(store) => store,
        Err(reason) => {
            return SessionLookup::Unknown {
                dir: pi_agent_dir().join("sessions"),
                reason,
            }
        }
    };
    lookup_sessions_in(&store, cwd, session_id)
}

/// [`lookup_sessions`] against an explicitly resolved [`PiStore`], so a caller
/// that already holds the resolved store (keeper mail confirm, tests) reads
/// with the same matching rules rather than resolving twice.
pub fn lookup_sessions_in(store: &PiStore, cwd: &Path, session_id: &str) -> SessionLookup {
    match store.layout {
        StoreLayout::CwdScoped => lookup_sessions_under(&store.root, cwd, session_id),
        StoreLayout::Flat => flat_lookup(&store.root, cwd, session_id),
    }
}

/// [`lookup_sessions`] against an explicit sessions root, so a caller that
/// owns its own store tree (tests, isolated lanes) resolves with the same
/// matching rules rather than a mirror of them.
pub fn lookup_sessions_under(root: &Path, cwd: &Path, session_id: &str) -> SessionLookup {
    let dir = root.join(encode_cwd(cwd));
    let suffix = format!("_{session_id}.jsonl");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            return SessionLookup::Unknown {
                dir,
                reason: error.to_string(),
            }
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
        .collect();
    files.sort();
    match files.len() {
        0 => SessionLookup::None,
        1 => SessionLookup::One {
            file: files.remove(0),
        },
        _ => SessionLookup::Duplicate { files },
    }
}

/// The cap on how much of a flat-store file a header read may take. The
/// header is the file's first record; anything past one line is session body.
const HEADER_READ_CAP: u64 = 64 * 1024;

/// A flat store holds every cwd's sessions side by side, so a file matches
/// only when its HEADER's `cwd` equals the asked cwd. Only each candidate's
/// first line is read (64 KiB cap). A header that cannot be read or parsed is
/// `Unknown` naming the file: we cannot prove the parseable siblings are the
/// only matches, and a wrong `one` would green-light a resume onto a session
/// that is not the one asked about.
fn flat_lookup(root: &Path, cwd: &Path, session_id: &str) -> SessionLookup {
    let suffix = format!("_{session_id}.jsonl");
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            return SessionLookup::Unknown {
                dir: root.to_path_buf(),
                reason: error.to_string(),
            }
        }
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
        .collect();
    candidates.sort();
    let asked = cwd.to_string_lossy();
    let mut matched: Vec<PathBuf> = Vec::new();
    for path in candidates {
        let header = match read_first_line(&path, HEADER_READ_CAP) {
            Ok(header) => header,
            Err(error) => {
                return SessionLookup::Unknown {
                    dir: root.to_path_buf(),
                    reason: format!("cannot read session header in {}: {error}", path.display()),
                }
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&header) {
            Ok(parsed) => parsed,
            Err(error) => {
                return SessionLookup::Unknown {
                    dir: root.to_path_buf(),
                    reason: format!("unparseable session header in {}: {error}", path.display()),
                }
            }
        };
        if parsed.get("cwd").and_then(|c| c.as_str()) == Some(asked.as_ref()) {
            matched.push(path);
        }
    }
    match matched.len() {
        0 => SessionLookup::None,
        1 => SessionLookup::One {
            file: matched.remove(0),
        },
        _ => SessionLookup::Duplicate { files: matched },
    }
}

/// The first line of a file, reading at most `cap` bytes.
fn read_first_line(path: &Path, cap: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(cap).read_to_end(&mut buf)?;
    let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// The refusal a resume owes an ambiguous id, or `None` when there is nothing
/// ambiguous to refuse.
///
/// It names EVERY session found, with its timestamp, and selects none. Naming
/// only the one being resumed is the codex short-id precedent, where a refusal
/// that named the victim's own row steered a worker to a wrong conclusion.
///
/// pi's own behaviour here is the defect this refuses to inherit: it picks the
/// oldest file, prints nothing, and leaves the other sessions unreachable by
/// the only handle fno has for them.
pub fn duplicate_resume_refusal(
    cwd: &Path,
    session_id: &str,
    lookup: &SessionLookup,
) -> Option<String> {
    let SessionLookup::Duplicate { files } = lookup else {
        return None;
    };
    let mut message = format!(
        "pi session id {session_id:?} in {} resolves to {} sessions, so this resume is refused \
         rather than guessing. pi itself would pick the oldest and say nothing, leaving the \
         others unreachable by this id. Every one of them, oldest first:",
        cwd.display(),
        files.len()
    );
    for file in files {
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unnamed>");
        let stamp = name.split('_').next().unwrap_or(name);
        message.push_str(&format!("\n  {stamp}  {}", file.display()));
    }
    message.push_str(
        "\nNone was selected. Do not rank these by content: an empty assistant content array \
         marks a turn that was attempted and FAILED, so the emptier file is often the one worth \
         reading. Resume one by its file path with `pi --session <path>`.",
    );
    Some(message)
}

/// The claim key that serialises the CREATE decision for one pi session.
///
/// This is a SESSION-ID key, not a node key. The standing rule that
/// `fno agents claim acquire` is never called by hand is about NODE claims,
/// where `target init` already claims the node and a manual acquire creates a
/// double claim. This key lives in a different key space, is taken by the spawn
/// lane rather than by a person, and is released in the same operation.
///
/// The cwd is IN the key because pi's session lookup is cwd-scoped: the same id
/// in two worktrees is two different sessions and must not contend.
pub fn create_claim_key(cwd: &Path, session_id: &str) -> String {
    format!("pi-session:{}:{session_id}", cwd.display())
}

/// Whether a create-claim reading means "a create is in flight, do not join".
///
/// `Live` and `Suspect` both mean HELD. `Suspect` is an unexpired TTL whose
/// holder is not provably alive, and the acquire path already refuses to steal
/// one, so treating it as free here would let an attach walk into a window
/// acquire itself will not enter.
///
/// The other three are not evidence of a create. `Free` is no claim, `Stale` is
/// an expired one, and `Corrupted` is a file that proves nothing - and failing
/// closed on an unreadable claim would refuse an operator's attach over a
/// damaged byte rather than over a race.
pub fn attach_blocked_by_create(state: crate::claims::ClaimState) -> bool {
    matches!(
        state,
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
    )
}

/// How long the create claim is held, in milliseconds.
///
/// The ruling on this node set 30s, justified by measurement: pi reaches
/// session-id adoption in 0.64s (that IS the create-decision span), and a full
/// create through the first session file took 5.81s, 4.96s and 4.94s across
/// three runs. The claim primitive refuses anything under a minute
/// (`MIN_TTL_MS`), so 30s is not available and this is the FLOOR rather than a
/// chosen value. The ruling's reason survives it: 60s is about ten times the
/// slowest measured create, and the leak it bounds is a crashed create holding
/// one session id unusable for at most a minute.
///
/// The scope is the CREATE DECISION ONLY, never the session lifetime, and the
/// two claim modes are why. A PID-liveness claim dies with its holder and is
/// reapable; an explicit-TTL claim survives a crash for the whole TTL. A
/// forking spawn lane must use a TTL, because the default anchors liveness to a
/// process that exits. So the TTL path is the one that gets used, and it is the
/// one that leaks: a long TTL taken for a session lifetime makes that id
/// unusable until it expires if the holder crashes before the first turn.
/// Keeping the scope short is what keeps the TTL small enough to be harmless.
///
/// On expiry the reading degrades to UNKNOWN and is re-checked. It never
/// degrades to free.
pub const CREATE_CLAIM_TTL_MS: u64 = 60_000;

/// The argv that opens pi's OWN interface on `session_id`.
///
/// An EXEC target, never a proxy, and the same shape PR 1255 established for
/// codex: the viewport replaces a pane with a real vendor process and draws
/// nothing itself. What differs is that pi needs no daemon and no socket. The
/// TUI reaches a live rpc session by naming the same id in the same cwd, which
/// was measured on 2026-08-28: the TUI came up on a session an rpc driver was
/// holding, rendered that session's own turns, and the session-file count for
/// the id stayed at one.
///
/// This is a JOIN, and it is only safe as one. Running it against an id that
/// does not exist yet is a CREATE, and creates are the unserialised half.
pub fn pi_attach_argv(session_id: &str) -> Vec<String> {
    vec![
        "pi".to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
        "--provider".to_string(),
        pi_provider(),
        "--model".to_string(),
        pi_model(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_matches_the_three_observed_directories() {
        assert_eq!(
            encode_cwd(Path::new("/Users/bb16/code/footnote/footnote")),
            "--Users-bb16-code-footnote-footnote--"
        );
        assert_eq!(encode_cwd(Path::new("/private/tmp")), "--private-tmp--");
        // A dot-prefixed component survives unchanged; only separators move.
        assert_eq!(
            encode_cwd(Path::new("/home/u/.local/tmp/piprobe")),
            "--home-u-.local-tmp-piprobe--"
        );
        // The colon rewrites like a separator: pi's own encoder rewrites it,
        // so a cwd carrying one must not encode differently here.
        assert_eq!(encode_cwd(Path::new("/a/b:c")), "--a-b-c--");
    }

    #[test]
    fn the_claim_key_carries_cwd_so_two_worktrees_never_contend() {
        let a = create_claim_key(Path::new("/repo/worktrees/one"), "s-1");
        let b = create_claim_key(Path::new("/repo/worktrees/two"), "s-1");
        assert_ne!(a, b, "one id in two worktrees is two sessions");
        assert!(a.starts_with("pi-session:"), "session key space, not node:");
    }

    /// A missing session directory reads UNKNOWN, never `None`. This is the
    /// whole point of the enum: an absence with two explanations cannot be
    /// reported as the one that happens to be convenient.
    #[test]
    fn an_unreadable_directory_reads_unknown_and_not_none() {
        let tmp = std::env::temp_dir().join(format!("pi-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.join("nonexistent-agent-dir"));
        let lookup = lookup_sessions(Path::new("/repo"), "s-1");
        assert!(
            matches!(lookup, SessionLookup::Unknown { .. }),
            "missing dir must read Unknown, got {lookup:?}"
        );
        assert_eq!(
            duplicate_resume_refusal(Path::new("/repo"), "s-1", &lookup),
            None
        );
        std::env::remove_var("PI_CODING_AGENT_DIR");
    }

    /// The refusal names EVERY session with its timestamp and selects none.
    #[test]
    fn the_duplicate_refusal_names_every_session_and_picks_none() {
        let files = vec![
            PathBuf::from("/s/2026-08-28T20-58-10-768Z_race.jsonl"),
            PathBuf::from("/s/2026-08-28T20-58-10-817Z_race.jsonl"),
        ];
        let lookup = SessionLookup::Duplicate { files };
        let message = duplicate_resume_refusal(Path::new("/repo"), "race", &lookup)
            .expect("a duplicate must refuse");
        assert!(message.contains("2026-08-28T20-58-10-768Z"), "{message}");
        assert!(message.contains("2026-08-28T20-58-10-817Z"), "{message}");
        assert!(message.contains("None was selected"), "{message}");
    }

    /// The attach argv always carries an explicit model. Omitting it is trap 2:
    /// pi falls through to a Bedrock model and reports an expired AWS SSO
    /// session, which names the wrong cloud entirely.
    #[test]
    fn the_attach_argv_always_pins_provider_and_model() {
        let argv = pi_attach_argv("s-1");
        assert_eq!(argv[..3], ["pi", "--session-id", "s-1"]);
        assert!(argv.contains(&"--model".to_string()), "{argv:?}");
        assert!(argv.contains(&"--provider".to_string()), "{argv:?}");
        assert!(
            !argv.contains(&"--mode".to_string()),
            "the pane lane is the plain TUI, never --mode rpc: {argv:?}"
        );
    }

    /// An attach into a live create is a second CREATE, not a join, and the
    /// session store cannot say so: pi writes its file at the first turn
    /// ATTEMPT, so the lookup reads `None` for a session being made right now.
    /// The claim is the only instrument that sees that window, so a held one
    /// blocks the attach and nothing else does.
    #[test]
    fn a_held_create_claim_blocks_an_attach_and_nothing_else_does() {
        use crate::claims::ClaimState;

        assert!(attach_blocked_by_create(ClaimState::Live));
        // Suspect is an unexpired TTL whose holder is not provably alive. The
        // acquire path refuses to steal one, so an attach must not walk into a
        // window acquire itself will not enter.
        assert!(attach_blocked_by_create(ClaimState::Suspect));

        assert!(!attach_blocked_by_create(ClaimState::Free));
        assert!(!attach_blocked_by_create(ClaimState::Stale));
        // A damaged claim file is evidence of nothing, and refusing an
        // operator's attach over one would fail closed on the wrong fact.
        assert!(!attach_blocked_by_create(ClaimState::Corrupted));
    }

    /// AC15-HP: a `provider/id` model stays `--model` only, effort maps to
    /// `--thinking`; AC15-ERR: a bare model carries the fno provider beside
    /// it.
    #[test]
    fn pi_route_maps_explicit_axes() {
        let route = pi_route("anthropic/claude-sonnet-5", "high", "", "");
        assert_eq!(
            route.tokens,
            vec![
                "--model".to_string(),
                "anthropic/claude-sonnet-5".to_string(),
                "--thinking".to_string(),
                "high".to_string(),
            ]
        );
        let route = pi_route("gpt-5.5", "", "", "");
        assert_eq!(
            route.tokens,
            vec![
                "--provider".to_string(),
                pi_provider(),
                "--model".to_string(),
                "gpt-5.5".to_string(),
            ]
        );
        // Tool axes map to pi's own flags when carried.
        let route = pi_route("", "", "read,edit", "bash");
        assert_eq!(
            route.tokens,
            vec![
                "--provider".to_string(),
                pi_provider(),
                "--model".to_string(),
                pi_model(),
                "--tools".to_string(),
                "read,edit".to_string(),
                "--exclude-tools".to_string(),
                "bash".to_string(),
            ]
        );
    }

    /// AC16-HP/AC16-EDGE: settings naming BOTH a provider and a model win
    /// outright; settings naming only one fall back to the fno default pair.
    #[test]
    fn pi_route_defers_to_pis_own_settings() {
        let tmp = std::env::temp_dir().join(format!("pi-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(
            tmp.join("settings.json"),
            r#"{"defaultProvider": "openai-codex", "defaultModel": "gpt-5.5"}"#,
        )
        .unwrap();
        let route = pi_route_in("", "", "", "", &tmp);
        assert_eq!(route.tokens, Vec::<String>::new());
        assert_eq!(route.route_source, "pi-settings");
        assert!(route.note.contains("unsandboxed"), "{}", route.note);

        std::fs::write(
            tmp.join("settings.json"),
            r#"{"defaultProvider": "openai-codex"}"#,
        )
        .unwrap();
        let route = pi_route_in("", "", "", "", &tmp);
        assert_eq!(route.route_source, "fno-default");
        assert!(route
            .tokens
            .windows(2)
            .any(|w| w[0] == "--provider" && w[1] == pi_provider()));

        std::fs::remove_dir_all(&tmp).ok();
    }
}
