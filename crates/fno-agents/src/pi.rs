//! pi's session identity: where a session lives on disk, what names it, and
//! what a duplicate looks like (x-c198).
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

/// pi's session store root, `PI_HOME`-relative when that is set.
pub fn pi_sessions_root() -> PathBuf {
    let home = std::env::var("PI_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.pi", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home).join("agent").join("sessions")
}

/// pi's on-disk encoding of a working directory: every path separator becomes a
/// single `-`, and the result is fenced with `--` at both ends.
///
/// Derived from three live directories rather than from pi's source:
///
/// ```text
/// /Users/bb16/code/footnote/footnote  -> --Users-bb16-code-footnote-footnote--
/// /private/tmp                        -> --private-tmp--
/// /Users/bb16/.claude/jobs/…/piprobe  -> --Users-bb16-.claude-jobs-…-piprobe--
/// ```
///
/// A dot in a path component survives unchanged, as `.claude` above shows.
pub fn encode_cwd(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let body = raw.trim_start_matches('/').replace('/', "-");
    format!("--{body}--")
}

/// The cwd-scoped directory pi keeps `cwd`'s sessions in.
pub fn session_dir(cwd: &Path) -> PathBuf {
    pi_sessions_root().join(encode_cwd(cwd))
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
    let dir = session_dir(cwd);
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
        assert_eq!(
            encode_cwd(Path::new("/Users/bb16/.claude/jobs/15096b9a/tmp/piprobe")),
            "--Users-bb16-.claude-jobs-15096b9a-tmp-piprobe--"
        );
    }

    #[test]
    fn the_claim_key_carries_cwd_so_two_worktrees_never_contend() {
        let a = create_claim_key(Path::new("/repo/.claude/worktrees/one"), "s-1");
        let b = create_claim_key(Path::new("/repo/.claude/worktrees/two"), "s-1");
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
        std::env::set_var("PI_HOME", tmp.join("nonexistent-pi-home"));
        let lookup = lookup_sessions(Path::new("/repo"), "s-1");
        assert!(
            matches!(lookup, SessionLookup::Unknown { .. }),
            "missing dir must read Unknown, got {lookup:?}"
        );
        assert_eq!(
            duplicate_resume_refusal(Path::new("/repo"), "s-1", &lookup),
            None
        );
        std::env::remove_var("PI_HOME");
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
}
