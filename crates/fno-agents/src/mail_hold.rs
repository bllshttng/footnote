//! Arm or lift a busy-mode hold for ANOTHER session (transport-only client
//! action, registered in no client menu - the shrink law allows no new
//! client verbs; the harness hook entries (`hook prompt`, `hook stop`) and
//! `king cancel` reach it through the binary path like the other early
//! dispatches).
//!
//! The hold is the registry row's `delivery_policy = "bus-only"` stamp plus
//! the sidecar clock `fno.mail.hold` reads. The conversation rules (C2-C4,
//! C6) arm and rewrite that one clock: a real user message whose Enter the
//! mux witnessed arms the answering clock, the session's Stop fire shortens
//! it to the grace, and the release timer lifts it. A hold the user set on
//! purpose (a stamped row with a manual or missing clock) is never touched.
//! The files written here are byte-identical to what the Python writer
//! (`_write` in cli/src/fno/mail/hold.py) produces, so every Python reader
//! (the injector gate, `notify-self`, `hold-release`) sees one hold, never
//! two dialects.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry};

/// The turn-end grace (C3), in seconds: a Stop fire shortens a live
/// conversation clock to `now + GRACE_S`, so held mail flows about two
/// minutes after the answer ends. The user may change this value; the
/// change is this one line.
pub(crate) const GRACE_S: i64 = 120;

/// The answering-phase backstop (C2), in seconds: the answering clock is
/// written this far out, so a crash or an interrupt (Esc fires no Stop)
/// cannot hold mail forever. Not the C5 cap. The user may change this
/// value; the change is this one line.
pub(crate) const ANSWER_BACKSTOP_S: i64 = 3600;

/// The `source` mark on a clock the conversation rules wrote. A manual hold
/// carries no source, which is how the two are told apart.
const CONVERSATION_SOURCE: &str = "conversation";

/// The full normalized session id: the collision-free clock key. Mirrors
/// Python's `session_identity_key` (harness_identity.py), so the two legs
/// agree on the file both write. UUID-family ids compare case-insensitively;
/// opencode's `ses_` ids do not. The retired first-eight key stays readable
/// through hold.py's `addresses()` sweep.
pub(crate) fn identity_key(session_id: &str) -> String {
    if session_id.starts_with("ses_") {
        session_id.to_string()
    } else {
        session_id.to_lowercase()
    }
}

/// The state root the sidecar clock lives under: `$FNO_HOME`, else
/// `paths.state_dir` from the global config's `[paths]` table (`~` expands),
/// else `$HOME/.fno`. The Python resolver reads `FNO_STATE_DIR` and its full
/// settings stack instead of `FNO_HOME`, so the two agree wherever the
/// global config (or the default) carries the root, and a test env must pin
/// both variables to aim both legs at one directory.
fn state_root() -> PathBuf {
    if let Some(home) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(home);
    }
    let ambient = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".fno"))
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let global = ambient.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&global) else {
        return ambient;
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return ambient;
    };
    let Some(configured) = doc
        .get("paths")
        .and_then(|p| p.get("state_dir"))
        .and_then(|v| v.as_str())
    else {
        return ambient;
    };
    let expanded = configured
        .strip_prefix("~/")
        .map(|rest| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(rest))
                .unwrap_or_else(|| PathBuf::from(configured))
        })
        .unwrap_or_else(|| PathBuf::from(configured));
    if expanded.is_absolute() {
        expanded
    } else {
        ambient
    }
}

fn hold_sidecar_path(handle: &str) -> PathBuf {
    state_root()
        .join("mail-hold")
        .join(format!("{handle}.json"))
}

/// Find the registry row whose harness session id is `session_id`
/// (case-normalized), returning its index.
fn row_for_session(registry: &crate::state::Registry, session_id: &str) -> Option<usize> {
    let wanted = identity_key(session_id);
    registry.entries.iter().position(|e| {
        e.harness_session_id
            .as_deref()
            .map(|sid| identity_key(sid) == wanted)
            .unwrap_or(false)
    })
}

/// Stamp the row `bus-only` (or clear the stamp for `--off`) under the
/// cross-language registry lock. Returns the matched row's session id, or
/// None when no row carries it (fail-closed: no row, no clock).
fn set_policy(session_id: &str, policy: Option<&str>) -> Option<String> {
    let path = AgentsHome::shared_registry_json();
    let matched = update_registry(&path, |registry| {
        let i = row_for_session(registry, session_id)?;
        registry.entries[i].delivery_policy = policy.map(str::to_string);
        registry.entries[i].harness_session_id.clone()
    })
    .ok()?;
    matched
}

/// The matched row's (session id, delivery policy), read-only. The guard
/// needs the policy to tell a hold the user set on purpose from a
/// conversation clock.
fn lookup_row(session_id: &str) -> Option<(String, Option<String>)> {
    let registry = load_registry(&AgentsHome::shared_registry_json()).ok()?;
    let i = row_for_session(&registry, session_id)?;
    let entry = &registry.entries[i];
    Some((
        entry.harness_session_id.clone()?,
        entry.delivery_policy.clone(),
    ))
}

/// Write the sidecar clock in hold.py `_write`'s exact shape: one JSON
/// object, Python's `, `/`: ` separators and key order, trailing newline,
/// atomic via temp file + rename. `source` appends the conversation mark
/// last; Python `read()` ignores the unknown key.
fn write_clock(
    handle: &str,
    until: chrono::DateTime<chrono::Utc>,
    window_s: i64,
    clock_kind: &str,
    ceiling: Option<chrono::DateTime<chrono::Utc>>,
    source: Option<&str>,
) -> std::io::Result<()> {
    let stamp = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%SZ");
    let ceiling_part = match ceiling {
        Some(c) => format!("\"{}\"", stamp(c)),
        None => "null".to_string(),
    };
    let source_part = source
        .map(|s| format!(", \"source\": \"{s}\""))
        .unwrap_or_default();
    let payload = format!(
        "{{\"until\": \"{until}\", \"window_s\": {window_s}, \
         \"clock_kind\": \"{clock_kind}\", \"ceiling\": {ceiling_part}{source_part}}}\n",
        until = stamp(until),
    );
    let dir = state_root().join("mail-hold");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(payload.as_bytes())?;
    }
    std::fs::rename(&tmp, hold_sidecar_path(handle))
}

/// The sidecar clock as both writers leave it. A present-but-unparseable
/// stamp reads as None, never an error - hold.py's `read()` contract.
struct Clock {
    until: Option<chrono::DateTime<chrono::Utc>>,
    window_s: i64,
    clock_kind: String,
    ceiling: Option<chrono::DateTime<chrono::Utc>>,
    source: Option<String>,
}

impl Clock {
    /// Live = a future `until`. `until: null` and an expired clock are not
    /// live.
    fn live(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.until.map(|u| u > now).unwrap_or(false)
    }
}

/// The clock file as a [`Clock`]; a missing or unparseable file is None,
/// the same answer Python `read()` gives.
fn read_clock(handle: &str) -> Option<Clock> {
    let raw = std::fs::read_to_string(hold_sidecar_path(handle)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let parse = |s: &serde_json::Value| -> Option<chrono::DateTime<chrono::Utc>> {
        s.as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    let until_v = v.get("until").cloned().unwrap_or(serde_json::Value::Null);
    let ceiling_v = v.get("ceiling").cloned().unwrap_or(serde_json::Value::Null);
    // A present-but-unparseable stamp means the file is not a clock.
    let bad = |f: &serde_json::Value| f.is_string() && parse(f).is_none();
    if bad(&until_v) || bad(&ceiling_v) {
        return None;
    }
    let clock_kind = v
        .get("clock_kind")
        .and_then(|k| k.as_str())
        .unwrap_or("idle");
    if clock_kind != "idle" && clock_kind != "wall" {
        return None;
    }
    Some(Clock {
        until: parse(&until_v),
        window_s: v.get("window_s").and_then(|w| w.as_i64()).unwrap_or(0),
        clock_kind: clock_kind.to_string(),
        ceiling: parse(&ceiling_v),
        source: v.get("source").and_then(|s| s.as_str()).map(str::to_string),
    })
}

/// Spawn the Python release timer detached (stdio null, own process group):
/// the third drain trigger that lifts the hold and delivers the digest with
/// no further input. The timer re-reads the clock every poll, so a re-arm
/// simply keeps it sleeping, and its designed exit is a vanished clock.
fn spawn_release_timer(handle: &str) {
    let mut cmd = Command::new(crate::scrape::fno_py());
    cmd.args(["agents", "mail", "hold-release", "--handle", handle])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Err(exc) = cmd.spawn() {
        // The hold still lifts on the next send attempt or prompt; say why
        // the clock-alone lift will not fire.
        eprintln!("mail-hold: release timer did not start: {exc}");
    }
}

/// A stamped row with no clock never lapses, so a clock a concurrent
/// `tidy_lapsed` (hold.py) removed between the write and the stamp is
/// written back once. True when the clock exists after the call.
fn ensure_clock(handle: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    if read_clock(handle).is_some() {
        return true;
    }
    write_clock(
        handle,
        now + chrono::Duration::seconds(ANSWER_BACKSTOP_S),
        ANSWER_BACKSTOP_S,
        "wall",
        None,
        Some(CONVERSATION_SOURCE),
    )
    .is_ok()
}

/// A real user message (C2): the provenance classifier passes it, and it is
/// neither a `!` shell line nor a slash/dollar verb token. Everything else
/// is machinery, and machinery never arms the conversation hold.
pub(crate) fn is_real_message(prompt: &str) -> bool {
    let Ok(cleaned) = crate::provenance::classify(prompt) else {
        return false;
    };
    if cleaned.starts_with('!') {
        return false;
    }
    cleaned
        .split_whitespace()
        .next()
        .map(|tok| crate::provider::parse_verb_token(tok).is_none())
        .unwrap_or(false)
}

/// What [`arm_conversation`] did.
pub(crate) enum ArmOutcome {
    /// The answering clock is written and the row stamped.
    Armed,
    /// The row carries a hold the user set on purpose; nothing changed.
    StoodAside,
    /// No registry row carries the session.
    NoRow,
    /// The row raced out between the read and the stamp.
    RowRaced,
    /// The clock write failed.
    WriteFailed,
}

impl ArmOutcome {
    /// The `mail-hold` verb's exit code for the outcome.
    pub(crate) fn code(self) -> i32 {
        match self {
            ArmOutcome::Armed | ArmOutcome::StoodAside => 0,
            ArmOutcome::NoRow | ArmOutcome::RowRaced => 3,
            ArmOutcome::WriteFailed => 1,
        }
    }
}

/// The conversation arm (C2): write the answering clock (`wall`,
/// `ANSWER_BACKSTOP_S` out, `source: conversation`) and stamp the row
/// `bus-only`, clock before stamp as today. A row stamped `bus-only` whose
/// clock is absent, has `until: null`, or is live without
/// `source: conversation` carries a hold the user set on purpose; the
/// conversation rules never overwrite it. Silent: no stdout, no stderr, so
/// the hook entries can call it mid-render.
pub(crate) fn arm_conversation(session_id: &str) -> (ArmOutcome, Option<String>) {
    match lookup_row(session_id) {
        Some((matched, policy)) => arm_row(&matched, policy, session_id),
        None => (ArmOutcome::NoRow, None),
    }
}

/// The conversation arm over an already-resolved registry row, so a caller
/// that read the row for its own decision (the prompt hook's since_ms) arms
/// without a second registry read. `session_id` stays the stamp key.
fn arm_row(
    matched: &str,
    policy: Option<String>,
    session_id: &str,
) -> (ArmOutcome, Option<String>) {
    let handle = identity_key(matched);
    let now = chrono::Utc::now();
    let clock = read_clock(&handle);
    let live = clock.as_ref().map(|c| c.live(now)).unwrap_or(false);
    let live_conversation =
        live && clock.as_ref().and_then(|c| c.source.as_deref()) == Some(CONVERSATION_SOURCE);
    if policy.as_deref() == Some("bus-only")
        && (clock.is_none()
            || clock.as_ref().and_then(|c| c.until).is_none()
            || (live && !live_conversation))
    {
        return (ArmOutcome::StoodAside, Some(handle));
    }
    let prior_live_conversation = live_conversation;
    if write_clock(
        &handle,
        now + chrono::Duration::seconds(ANSWER_BACKSTOP_S),
        ANSWER_BACKSTOP_S,
        "wall",
        None,
        Some(CONVERSATION_SOURCE),
    )
    .is_err()
    {
        return (ArmOutcome::WriteFailed, Some(handle));
    }
    if set_policy(session_id, Some("bus-only")).is_none() {
        // The row raced out between the read and the stamp; the clock on
        // disk is inert without the flag.
        return (ArmOutcome::RowRaced, Some(handle));
    }
    ensure_clock(&handle, now);
    if !prior_live_conversation {
        // One release timer per conversation: the timer re-reads the clock
        // every poll, so a re-arm keeps the first timer sleeping.
        spawn_release_timer(&handle);
    }
    (ArmOutcome::Armed, Some(handle))
}

/// The UserPromptSubmit side (C2, C4): a real message whose Enter the mux
/// witnessed inside the window arms the conversation hold. The window is
/// the 30 s submit look-back, widened to the answer backstop while a
/// conversation clock is live: a follow-up the harness queues during a long
/// answer fires UserPromptSubmit only after the turn ends, often minutes
/// after its Enter, and without the wide look-back it would not restart the
/// grace. Silent on every path, never panics.
pub(crate) fn conversation_prompt(session_id: &str, prompt: &str) {
    if !is_real_message(prompt) {
        return;
    }
    let Some((matched, policy)) = lookup_row(session_id) else {
        return;
    };
    let handle = identity_key(&matched);
    let now = chrono::Utc::now();
    let now_ms = now.timestamp_millis();
    let live = read_clock(&handle).map(|c| c.live(now)).unwrap_or(false);
    let since_ms = if live {
        now_ms - ANSWER_BACKSTOP_S * 1000
    } else {
        now_ms - crate::operator_witness::SUBMIT_WINDOW_AFTER_MS
    };
    let journal = crate::paths::AgentsHome::from_env().events_jsonl();
    if crate::operator_witness::submitted_since(&journal, session_id, since_ms, now_ms) {
        // The row is already resolved; the arm runs on it directly instead
        // of looking it up a second time.
        arm_row(&matched, policy, session_id);
    }
}

/// The Stop side (C3, C6): a live conversation clock is shortened to
/// `now + GRACE_S`, never extended, so repeated Stop fires in a loop cannot
/// keep one alive and the release timer lifts the hold about two minutes
/// after the answer ends. A manual clock, an expired one, or no clock is
/// left alone. Silent, never panics.
pub(crate) fn conversation_turn_end(session_id: &str) {
    let Some((matched, _)) = lookup_row(session_id) else {
        return;
    };
    let handle = identity_key(&matched);
    let now = chrono::Utc::now();
    let Some(clock) = live_conversation_clock(&handle, now) else {
        return;
    };
    let grace_until = now + chrono::Duration::seconds(GRACE_S);
    let until = match clock.until {
        Some(u) if u < grace_until => u,
        _ => grace_until,
    };
    let _ = write_clock(
        &handle,
        until,
        clock.window_s,
        &clock.clock_kind,
        clock.ceiling,
        clock.source.as_deref(),
    );
}

/// The handle's live conversation clock, or None.
fn live_conversation_clock(handle: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Clock> {
    let clock = read_clock(handle)?;
    if !clock.live(now) || clock.source.as_deref() != Some(CONVERSATION_SOURCE) {
        return None;
    }
    Some(clock)
}

/// `fno-agents mail-hold --session <id> [--off]`
///
/// Arm (default): run the conversation arm and report its outcome.
/// `--off`: clear the clock and unstamp the policy, so a cancelled crown's
/// mail delivers normally instead of holding forever on a stamped row with
/// no clock (the never-lapses state). No row for the session: exit 3,
/// nothing written.
pub fn run_mail_hold(args: &[String]) -> i32 {
    let mut session: Option<&String> = None;
    let mut off = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next(),
            "--off" => off = true,
            other => {
                eprintln!("mail-hold: unknown argument {other:?}");
                return 2;
            }
        }
    }
    let Some(session_id) = session else {
        eprintln!("mail-hold: --session <session-id> is required");
        return 2;
    };
    if off {
        match set_policy(session_id, None) {
            Some(matched) => {
                let _ = std::fs::remove_file(hold_sidecar_path(&identity_key(&matched)));
                0
            }
            None => {
                eprintln!("mail-hold: no registry row carries session {session_id}");
                3
            }
        }
    } else {
        let (outcome, matched) = arm_conversation(session_id);
        match outcome {
            ArmOutcome::Armed => println!(
                "mail-hold: bus-only armed for {} (conversation)",
                matched.as_deref().unwrap_or(session_id)
            ),
            ArmOutcome::StoodAside => println!(
                "mail-hold: hold for {} stands (the conversation rules never overwrite a manual hold)",
                matched.as_deref().unwrap_or(session_id)
            ),
            ArmOutcome::NoRow | ArmOutcome::RowRaced => {
                eprintln!("mail-hold: no registry row carries session {session_id}")
            }
            ArmOutcome::WriteFailed => eprintln!(
                "mail-hold: could not write the conversation clock for {session_id}"
            ),
        }
        outcome.code()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Pin FNO_AGENTS_HOME (registry + journal) and FNO_HOME (hold sidecars)
    /// to one tempdir for `f`, under the crate-wide env lock (claim_verbs
    /// idiom). FNO_PY points at `true` so the detached release-timer spawn is
    /// a no-op the test never waits on. Prior values are restored, so an
    /// ambient FNO_HOME survives the test.
    pub(crate) fn with_hold_env(f: impl FnOnce(&std::path::Path)) {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior: Vec<(String, Option<std::ffi::OsString>)> =
            ["FNO_AGENTS_HOME", "FNO_HOME", "FNO_PY"]
                .iter()
                .map(|k| (k.to_string(), std::env::var_os(k)))
                .collect();
        let td = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_AGENTS_HOME", td.path());
        std::env::set_var("FNO_HOME", td.path());
        std::env::set_var("FNO_PY", "true");
        f(td.path());
        for (key, value) in prior {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
    }

    pub(crate) fn registry_row(name: &str, session: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name, "status": "live", "cwd": "/repo", "harness": "claude",
            "harness_session_id": session,
            "created_at": "2026-09-26T00:00:00Z",
        })
    }

    pub(crate) fn write_registry(dir: &std::path::Path, rows: serde_json::Value) {
        let doc = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": rows,
        });
        std::fs::write(dir.join("registry.json"), doc.to_string()).unwrap();
    }

    pub(crate) fn clock(dir: &std::path::Path, handle: &str) -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(dir.join("mail-hold").join(format!("{handle}.json"))).unwrap(),
        )
        .unwrap()
    }

    pub(crate) fn clock_path(dir: &std::path::Path, handle: &str) -> std::path::PathBuf {
        dir.join("mail-hold").join(format!("{handle}.json"))
    }

    /// Append one `operator_submit` witness row to the temp journal
    /// (`$FNO_AGENTS_HOME/events.jsonl`, the file `conversation_prompt`
    /// reads).
    pub(crate) fn witness_row(session: &str, submit_ms: i64) {
        use std::io::Write as _;
        let path = std::env::var_os("FNO_AGENTS_HOME")
            .map(std::path::PathBuf::from)
            .unwrap()
            .join("events.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let row = serde_json::json!({
            "ts": "2026-09-27T00:00:00Z",
            "type": "operator_submit",
            "source": "daemon",
            "data": {
                "mux_session": "main",
                "pane": 7,
                "via": "pane",
                "submit_ms": submit_ms,
                "resolution": "ok",
                "harness_session": session,
            }
        });
        writeln!(f, "{row}").unwrap();
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    const SID: &str = "cccccccc-1111-2222-3333-444455556666";

    #[test]
    fn arming_a_registered_session_stamps_the_row_and_writes_the_conversation_clock() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            let (outcome, _) = arm_conversation(SID);
            assert!(matches!(outcome, ArmOutcome::Armed));
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert_eq!(
                registry.entries[0].delivery_policy.as_deref(),
                Some("bus-only")
            );
            let row = clock(dir, SID);
            assert_eq!(row["clock_kind"], "wall");
            assert_eq!(row["window_s"], ANSWER_BACKSTOP_S);
            assert!(row["ceiling"].is_null());
            assert_eq!(row["source"], "conversation");
            let until = row["until"].as_str().unwrap();
            let until = chrono::DateTime::parse_from_rfc3339(until)
                .unwrap()
                .with_timezone(&chrono::Utc);
            let left = (until - chrono::Utc::now()).num_seconds();
            assert!(
                (3550..=3600).contains(&left),
                "the answering clock sits about 60 minutes out, got {left}s"
            );
        });
    }

    #[test]
    fn arming_an_unregistered_session_refuses_and_writes_nothing() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            let (outcome, _) = arm_conversation("dddddddd-1111-2222-3333-444455556666");
            assert!(matches!(outcome, ArmOutcome::NoRow));
            assert!(!clock_path(dir, "dddddddd-1111-2222-3333-444455556666").exists());
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert!(registry.entries[0].delivery_policy.is_none());
        });
    }

    #[test]
    fn off_clears_both_the_clock_and_the_stamp() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            assert_eq!(run_mail_hold(&["--session".into(), SID.into()]), 0);
            assert_eq!(
                run_mail_hold(&["--session".into(), SID.into(), "--off".into()]),
                0
            );
            assert!(!clock_path(dir, SID).exists(), "the clock file is gone");
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert!(registry.entries[0].delivery_policy.is_none());
        });
    }

    #[test]
    fn conversation_prompt_arms_on_a_witnessed_real_message() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            witness_row(SID, now_ms() - 1_000);
            conversation_prompt(SID, "can you check the logs");
            let row = clock(dir, SID);
            assert_eq!(row["source"], "conversation");
            assert_eq!(row["clock_kind"], "wall");
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert_eq!(
                registry.entries[0].delivery_policy.as_deref(),
                Some("bus-only")
            );
        });
    }

    #[test]
    fn conversation_prompt_needs_a_fresh_witness_for_this_session() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            // No witness at all.
            conversation_prompt(SID, "hello");
            assert!(!clock_path(dir, SID).exists(), "no witness, no arm");
            // A witness past the 30 s look-back.
            witness_row(SID, now_ms() - 31_000);
            conversation_prompt(SID, "hello");
            assert!(
                !clock_path(dir, SID).exists(),
                "a 31 s old witness is outside the window with no live clock"
            );
            // A witness for another session.
            witness_row("dddddddd-1111-2222-3333-444455556666", now_ms() - 1_000);
            conversation_prompt(SID, "hello");
            assert!(
                !clock_path(dir, SID).exists(),
                "another session's witness arms nothing"
            );
        });
    }

    #[test]
    fn a_live_conversation_clock_widens_the_witness_look_back() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            // The conversation is live; the follow-up was typed minutes ago
            // and the harness only fired UserPromptSubmit after the turn.
            arm_conversation(SID);
            witness_row(SID, now_ms() - 300_000);
            conversation_prompt(SID, "also fix the flake");
            let row = clock(dir, SID);
            let until = chrono::DateTime::parse_from_rfc3339(row["until"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
            let left = (until - chrono::Utc::now()).num_seconds();
            assert!(
                (3550..=3600).contains(&left),
                "the follow-up restored the answering clock, got {left}s"
            );
        });
    }

    #[test]
    fn machinery_prompts_arm_nothing() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            witness_row(SID, now_ms() - 1_000);
            for prompt in ["/compact", "$fno:review", "!ls", ""] {
                conversation_prompt(SID, prompt);
                assert!(
                    !clock_path(dir, SID).exists(),
                    "{prompt:?} must never arm the hold"
                );
            }
            let mail_tag = "<fno_mail from=\"x\">hi</fno_mail>";
            conversation_prompt(SID, mail_tag);
            assert!(
                !clock_path(dir, SID).exists(),
                "a mail-tagged prompt never arms the hold"
            );
        });
    }

    #[test]
    fn a_manual_hold_is_never_overwritten() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            // A live manual wall clock (no source) on a stamped row.
            let now = chrono::Utc::now();
            write_clock(
                &SID.to_string(),
                now + chrono::Duration::seconds(3600),
                3600,
                "wall",
                None,
                None,
            )
            .unwrap();
            set_policy(SID, Some("bus-only"));
            let before = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            witness_row(SID, now_ms() - 1_000);
            conversation_prompt(SID, "hello");
            let after = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            assert_eq!(before, after, "a manual hold is byte-identical after");
            // A hand-stamped row with no clock: still no clock after.
            let _ = std::fs::remove_file(clock_path(dir, SID));
            conversation_prompt(SID, "hello again");
            assert!(
                !clock_path(dir, SID).exists(),
                "a hand-stamped row with no clock gets no conversation clock"
            );
        });
    }

    /// A `/fno:dnd 60` wall clock (the exact bytes Python `_write` produces,
    /// no `source`) survives a turn end and a real witnessed message
    /// byte-identical: a DND you set keeps its full length (C10).
    #[test]
    fn a_dnd_you_set_keeps_its_full_length_wall() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            arm_conversation(SID);
            let ts = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let py_wall = format!(
                "{{\"until\": \"{}\", \"window_s\": 3600, \"clock_kind\": \"wall\", \"ceiling\": null}}\n",
                ts(chrono::Utc::now() + chrono::Duration::seconds(3600))
            );
            std::fs::write(clock_path(dir, SID), py_wall).unwrap();
            let before = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            conversation_turn_end(SID);
            witness_row(SID, now_ms() - 1_000);
            conversation_prompt(SID, "hello");
            let after = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            assert_eq!(before, after, "the wall clock is byte-identical after both");
        });
    }

    /// The idle variant (`--minutes 20`): same survival, byte-identical.
    #[test]
    fn a_dnd_you_set_keeps_its_full_length_idle() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            arm_conversation(SID);
            let ts = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let py_idle = format!(
                "{{\"until\": \"{}\", \"window_s\": 1200, \"clock_kind\": \"idle\", \"ceiling\": \"{}\"}}\n",
                ts(chrono::Utc::now() + chrono::Duration::seconds(1200)),
                ts(chrono::Utc::now() + chrono::Duration::seconds(2400))
            );
            std::fs::write(clock_path(dir, SID), py_idle).unwrap();
            let before = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            conversation_turn_end(SID);
            witness_row(SID, now_ms() - 1_000);
            conversation_prompt(SID, "hello");
            let after = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            assert_eq!(before, after, "the idle clock is byte-identical after both");
        });
    }

    #[test]
    fn turn_end_moves_a_conversation_clock_to_the_grace() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            arm_conversation(SID);
            conversation_turn_end(SID);
            let row = clock(dir, SID);
            let until = chrono::DateTime::parse_from_rfc3339(row["until"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
            let left = (until - chrono::Utc::now()).num_seconds();
            assert!(
                (1..=GRACE_S).contains(&left),
                "the clock now sits at most one grace out, got {left}s"
            );
            assert_eq!(row["source"], "conversation", "the source mark stays");
            assert_eq!(row["window_s"], ANSWER_BACKSTOP_S, "every other field kept");
            // Never extends: a second Stop during the grace cannot push the
            // deadline back out.
            std::thread::sleep(std::time::Duration::from_millis(1_100));
            conversation_turn_end(SID);
            let row = clock(dir, SID);
            let until2 = chrono::DateTime::parse_from_rfc3339(row["until"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
            assert!(until2 <= until, "a second Stop never extends the grace");
        });
    }

    #[test]
    fn turn_end_leaves_a_manual_clock_alone() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            let now = chrono::Utc::now();
            write_clock(
                &SID.to_string(),
                now + chrono::Duration::seconds(3600),
                3600,
                "wall",
                None,
                None,
            )
            .unwrap();
            set_policy(SID, Some("bus-only"));
            let before = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            conversation_turn_end(SID);
            let after = std::fs::read_to_string(clock_path(dir, SID)).unwrap();
            assert_eq!(before, after, "a manual clock is byte-identical after");
        });
    }

    #[test]
    fn a_second_arm_during_grace_restores_the_answering_clock() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            arm_conversation(SID);
            conversation_turn_end(SID);
            // The next real message lands inside the grace window.
            witness_row(SID, now_ms() - 1_000);
            conversation_prompt(SID, "one more thing");
            let row = clock(dir, SID);
            let until = chrono::DateTime::parse_from_rfc3339(row["until"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
            let left = (until - chrono::Utc::now()).num_seconds();
            assert!(
                (3550..=3600).contains(&left),
                "the answering clock is restored, got {left}s"
            );
        });
    }

    #[test]
    fn the_arm_rewrites_a_clock_deleted_between_write_and_stamp() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            arm_conversation(SID);
            std::fs::remove_file(clock_path(dir, SID)).unwrap();
            assert!(
                ensure_clock(&SID.to_string(), chrono::Utc::now()),
                "a stamped row with no clock never lapses, so the clock is written back"
            );
            assert!(clock_path(dir, SID).exists());
        });
    }

    #[test]
    fn two_codex_ids_in_one_uuidv7_window_get_distinct_clocks() {
        // A codex id is a UUIDv7: the first 12 hex chars are the ms timestamp,
        // so two ids opened in the same 65.536s bucket share the first eight.
        // Each row must get its own clock file, and releasing one must leave
        // the sibling stamped bus-only WITH its clock (the never-lapses state
        // is a stamped row whose clock is gone).
        with_hold_env(|dir| {
            write_registry(
                dir,
                serde_json::json!([
                    registry_row("alpha", "0198a3f2-77e3-7000-8000-000000000001"),
                    registry_row("beta", "0198a3f2-77e3-7000-8000-000000000002"),
                ]),
            );
            assert_eq!(
                run_mail_hold(&[
                    "--session".into(),
                    "0198a3f2-77e3-7000-8000-000000000001".into()
                ]),
                0
            );
            assert_eq!(
                run_mail_hold(&[
                    "--session".into(),
                    "0198a3f2-77e3-7000-8000-000000000002".into()
                ]),
                0
            );
            let holds = dir.join("mail-hold");
            assert!(holds
                .join("0198a3f2-77e3-7000-8000-000000000001.json")
                .exists());
            assert!(holds
                .join("0198a3f2-77e3-7000-8000-000000000002.json")
                .exists());
            assert!(
                !holds.join("0198a3f2.json").exists(),
                "the colliding first-eight key must never be written"
            );
            assert_eq!(
                run_mail_hold(&[
                    "--session".into(),
                    "0198a3f2-77e3-7000-8000-000000000001".into(),
                    "--off".into()
                ]),
                0
            );
            assert!(!holds
                .join("0198a3f2-77e3-7000-8000-000000000001.json")
                .exists());
            assert!(
                holds
                    .join("0198a3f2-77e3-7000-8000-000000000002.json")
                    .exists(),
                "the sibling's clock survives the release"
            );
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            let beta = registry.entries.iter().find(|e| e.name == "beta").unwrap();
            assert_eq!(beta.delivery_policy.as_deref(), Some("bus-only"));
        });
    }

    #[test]
    fn missing_session_argument_refuses() {
        assert_eq!(run_mail_hold(&[]), 2);
        assert_eq!(
            run_mail_hold(&[
                "--minutes".into(),
                "0".into(),
                "--session".into(),
                "x-cccccccc".into()
            ]),
            2,
            "--minutes is gone; it reads as an unknown argument"
        );
    }
}
