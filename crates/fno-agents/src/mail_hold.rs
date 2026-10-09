//! Arm or lift a busy-mode hold for ANOTHER session (transport-only client
//! action; the harness hook entries (`hook prompt`, `hook stop`), `lead
//! cancel`, and the sideline menu's Release hold entry reach it through the
//! binary path like the other early dispatches).
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
//!
//! The delivery gate (C15, C16) lives here too: `--gate` answers one JSON
//! verdict telling a caller whether mail to a session delivers live now.
//! Own sends and `control:` mail pass a hold; the receipt names when it
//! ends. It is the single authority the Python injectors consult; the
//! Python gate bodies were one-call ports of it (law d-b6cc1a2a).

use std::io::{Read, Write};
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

/// The one token-to-row rule, `hold.addresses` mirrored: a token addresses a
/// row when it names its full session id (case-normalized), the row's
/// canonical first-eight handle, its daemon short_id, or its name. The
/// check-in resolves by full session id, the status verb by the short
/// handle, senders by any of the four; one matcher so every reader of a hold
/// answers the same question instead of the gate resolving fewer addresses
/// than the writers stamp.
fn row_matches_token(entry: &crate::state::RegistryEntry, token: &str) -> bool {
    let wanted = identity_key(token);
    entry.harness_session_id.as_deref().map_or(false, |sid| {
        let key = identity_key(sid);
        key == wanted || key.get(..8) == Some(wanted.as_str())
    }) || entry.short_id == token
        || entry.name == token
}

/// Find the registry row `session_id` addresses, returning its index.
fn row_for_session(registry: &crate::state::Registry, session_id: &str) -> Option<usize> {
    registry
        .entries
        .iter()
        .position(|e| row_matches_token(e, session_id))
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

/// Read the current session's hold clock and registry delivery stamp together.
/// Check-in needs both: either can hold delivery while the other is absent.
pub(crate) fn self_status(session_id: &str) -> Result<serde_json::Value, String> {
    let registry = load_registry(&AgentsHome::shared_registry_json())
        .map_err(|error| format!("agent registry unreadable: {error}"))?;
    let index = row_for_session(&registry, session_id)
        .ok_or_else(|| format!("no registry row carries session {session_id}"))?;
    let entry = &registry.entries[index];
    let matched = entry
        .harness_session_id
        .as_deref()
        .ok_or_else(|| format!("registry row for session {session_id} has no session id"))?;
    let delivery_policy = entry.delivery_policy.clone();
    let clock = read_clock(&identity_key(matched));
    let now = chrono::Utc::now();
    let until = clock.as_ref().and_then(|clock| clock.until);
    Ok(serde_json::json!({
        "clock_live": clock.as_ref().is_some_and(|clock| clock.live(now)),
        "clock_until": until.map(|until| until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "delivery_policy": delivery_policy,
        // The machine-armed mark: a live clock the conversation
        // rules wrote, so the check-in labels the hold as its own state
        // instead of a DND the lead thinks it armed itself.
        "conversation": clock
            .as_ref()
            .is_some_and(|clock| clock.live(now) && clock.source.as_deref() == Some(CONVERSATION_SOURCE)),
    }))
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

/// The clock read's shape: the extend print's fields plus source, with null
/// fields omitted so a reader keys off a key's absence, not a null check.
fn clock_json(clock: &Clock) -> serde_json::Value {
    let stamp = |d: chrono::DateTime<chrono::Utc>| d.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut v = serde_json::json!({
        "window_s": clock.window_s,
        "clock_kind": clock.clock_kind,
    });
    let obj = v.as_object_mut().expect("json! builds an object");
    if let Some(until) = clock.until {
        obj.insert("until".to_string(), serde_json::json!(stamp(until)));
    }
    if let Some(ceiling) = clock.ceiling {
        obj.insert("ceiling".to_string(), serde_json::json!(stamp(ceiling)));
    }
    if let Some(source) = &clock.source {
        obj.insert("source".to_string(), serde_json::json!(source));
    }
    v
}

/// hold.py clock_description, ported: the operator-facing clock prose the
/// status line and the receipts share. The ceiling's None spelling matches
/// the Python interpolation ("ceiling legacy unbounded") byte for byte.
fn clock_description(clock: &Clock) -> String {
    let Some(until) = clock.until else {
        return "no expiry".to_string();
    };
    let stamp = |d: chrono::DateTime<chrono::Utc>| d.format("%H:%M:%S UTC").to_string();
    if clock.clock_kind == "wall" {
        return format!("wall clock, fixed deadline {}", stamp(until));
    }
    match clock.ceiling {
        Some(ceiling) => format!("quiet minutes idle clock, ceiling {}", stamp(ceiling)),
        None => "quiet minutes idle clock, ceiling legacy unbounded".to_string(),
    }
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

/// The delivery gate's answer for one send (C15, C16).
pub(crate) struct GateVerdict {
    /// True when the mail may deliver live now.
    pub deliver: bool,
    /// Why a held send delivered anyway: the caller IS the recipient
    /// (`own`), or the body opens with a `control:` directive line
    /// (`control`).
    pub pass: Option<&'static str>,
    /// The C16 receipt naming when the hold ends. `None` for a hand-stamped
    /// hold with no clock, where the Python caller keeps its existing text.
    pub receipt: Option<String>,
    /// The live clock's `until`, for the verdict JSON. `None` when untimed
    /// or delivering.
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

/// The registry row the gate's token addresses, as (harness session id,
/// name, delivery policy), under the shared [`row_matches_token`] rule.
fn lookup_gate_row(token: &str) -> Option<(String, String, Option<String>)> {
    let registry = load_registry(&AgentsHome::shared_registry_json()).ok()?;
    let entry = registry
        .entries
        .iter()
        .find(|e| row_matches_token(e, token))?;
    Some((
        entry.harness_session_id.clone()?,
        entry.name.clone(),
        entry.delivery_policy.clone(),
    ))
}

/// Every clock address the gate sweeps for one row, in `hold.addresses`
/// order: the full identity key leads, then the canonical first-eight
/// handle (the pre-migration writer key), then the token verbatim.
fn gate_clock_addresses(sid: &str, token: &str) -> Vec<String> {
    let mut out = vec![identity_key(sid)];
    if let Some(first8) = sid.get(..8) {
        out.push(identity_key(first8));
    }
    if !out.iter().any(|h| h == &identity_key(token)) {
        out.push(identity_key(token));
    }
    out
}

/// True when the caller resolving its own identity IS the held row (C15:
/// a session's own sends pass its own hold). The resolver is the authority;
/// the direct canonical-stamp read is its deterministic half, so a caller
/// whose stamp is complete but unproven still passes its own hold.
fn caller_is_recipient(sid: &str) -> bool {
    let home = AgentsHome::from_env();
    let owned =
        crate::spawn_context::resolve_self_identity(&|k| std::env::var(k).ok(), None, None, &home);
    if owned
        .session_id
        .as_deref()
        .map(|s| identity_key(s) == identity_key(sid))
        .unwrap_or(false)
    {
        return true;
    }
    // The resolver's first input, read directly: a complete canonical stamp
    // (FNO_HARNESS_NAME + FNO_HARNESS_SESSION_ID) names the caller.
    let name = std::env::var("FNO_HARNESS_NAME").unwrap_or_default();
    let session = std::env::var("FNO_HARNESS_SESSION_ID").unwrap_or_default();
    !name.trim().is_empty()
        && !session.trim().is_empty()
        && crate::claims::same_session_id(&session, sid)
}

/// The C15 control pass: the body's first non-blank line -- after one
/// leading `<fno_mail ...>` open tag is stripped (the wrapped lane puts the
/// directive after the tag), or after the delivered-mail header line is
/// stripped (header framing puts it first; the directive rides the body
/// under it) -- starts with `control:` (case-insensitive, the budget rule).
/// A header counts only when the store backs its sender
/// ([`crate::mail_header::verified_sender`]); an unbacked header line is
/// prose, never a directive.
fn body_is_control(body: &str) -> bool {
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    let rest = if first.len() >= 9 && first[..9].eq_ignore_ascii_case("<fno_mail") {
        match first.find('>') {
            Some(gt) => &first[gt + 1..],
            None => first,
        }
    } else if crate::mail_header::is_header_line(first)
        && crate::mail_header::verified_sender(body, crate::chats::message_sender).is_some()
    {
        lines.next().unwrap_or("")
    } else {
        first
    };
    rest.trim_start().to_lowercase().starts_with("control:")
}

/// The C16 receipt for a live timed clock, in the row's local time. A
/// conversation clock with more than the grace left names the answer end
/// (its `until` is only the crash backstop, so a clock time there would be
/// false); a conversation clock inside its grace reads as the wall clock it
/// now is. An idle clock says "or later", because activity restarts it.
fn hold_receipt(clock: &Clock, name: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let until = clock.until?;
    let hhmm = |t: chrono::DateTime<chrono::Utc>| {
        t.with_timezone(&chrono::Local).format("%H:%M").to_string()
    };
    let conversation = clock.source.as_deref() == Some(CONVERSATION_SOURCE);
    if conversation && until - now > chrono::Duration::seconds(GRACE_S) {
        return Some(format!(
            "held: {name} is in a conversation with the user; delivers about 2 minutes after the answer ends"
        ));
    }
    if clock.clock_kind == "idle" {
        return Some(format!(
            "held until about {} or later: {name} is in do-not-disturb and the quiet-minutes clock restarts on activity; delivers itself then",
            hhmm(until)
        ));
    }
    Some(format!(
        "held until about {}: {name} is in do-not-disturb (wall clock); delivers itself then",
        hhmm(until)
    ))
}

/// The delivery gate (C15, C16): does mail to `token` deliver live now?
/// Resolution, clock sweep, own-send and control passes all read; the gate
/// never writes, so it runs inside callers that may hold the registry lock.
/// A lapsed clock delivers; a hand-stamped hold (no clock) stays held with
/// no receipt, because the turn-boundary surface it promises is real.
pub(crate) fn gate(
    token: &str,
    body: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> GateVerdict {
    let deliver = |pass| GateVerdict {
        deliver: true,
        pass,
        receipt: None,
        until: None,
    };
    let Some((sid, name, policy)) = lookup_gate_row(token) else {
        return deliver(None);
    };
    if policy.as_deref() != Some("bus-only") {
        return deliver(None);
    }
    let clock = gate_clock_addresses(&sid, token)
        .iter()
        .find_map(|h| read_clock(h));
    let held = match clock.as_ref() {
        // No clock file: a hand-stamped hold with no end, no receipt.
        None => None,
        Some(c) => match c.until {
            // Lapsed timed clock: the hold no longer holds.
            Some(u) if u <= now => return deliver(None),
            // Live timed clock.
            Some(u) => Some((c, u)),
            // `until: null` records a deliberate permanent policy: held.
            None => None,
        },
    };
    if caller_is_recipient(&sid) {
        return deliver(Some("own"));
    }
    if body.is_some_and(body_is_control) {
        return deliver(Some("control"));
    }
    let Some((clock, until)) = held else {
        // Hand-stamped (no clock, or `until: null`): held, no receipt.
        return GateVerdict {
            deliver: false,
            pass: None,
            receipt: None,
            until: None,
        };
    };
    GateVerdict {
        deliver: false,
        pass: None,
        receipt: hold_receipt(clock, &name, now),
        until: Some(until),
    }
}

/// The one JSON line `--gate` prints; Python parses the verdict field.
fn gate_json(verdict: &GateVerdict) -> String {
    serde_json::json!({
        "verdict": if verdict.deliver { "deliver" } else { "hold" },
        "pass": verdict.pass,
        "receipt": verdict.receipt,
        "until": verdict.until.map(|u| u.to_rfc3339()),
    })
    .to_string()
}

/// The `--gate --park-on-hold` answer for a held body (C15): park the
/// payload and answer the `parked` JSON line with the park receipt. The
/// session's own sends type now (the pane's hold-pass stands down for them),
/// and so does a genuinely deliverable body; a `control:`-prefixed body on
/// the raw door parks too, because typed text carries no control envelope
/// for anything downstream to honor. An empty body or a failed park answers
/// None and the caller prints the plain verdict instead.
fn gate_parked_line(verdict: &GateVerdict, session_id: &str, body: &str) -> Option<String> {
    let typable =
        verdict.pass.as_deref() == Some("own") || (verdict.deliver && verdict.pass.is_none());
    if body.trim().is_empty() || typable {
        return None;
    }
    // The receipt promises a run-when, so a hold with no deadline never
    // parks: a hand-stamped hold (until: null or no clock) never lapses, the
    // runner would sit on the payload to its 24 h bound, and the raw lane's
    // documented answer for that case is the non-zero refusal. A
    // control-prefixed body carries a deliver verdict with no until and
    // parks as typed text, so only the held-and-undated shape refuses.
    if !verdict.deliver && verdict.until.is_none() {
        return None;
    }
    let receipt = park_payload_inner(session_id, body).ok()?;
    Some(
        serde_json::json!({
            "verdict": "parked",
            "pass": serde_json::Value::Null,
            "receipt": receipt,
            "until": verdict.until.map(|u| u.to_rfc3339()),
        })
        .to_string(),
    )
}

/// The parked raw payloads for one held session, beside its clock in the one
/// hold store.
fn parked_dir(handle: &str) -> PathBuf {
    state_root()
        .join("mail-hold")
        .join(format!("{handle}.parked"))
}

fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The park receipt, on the gate's time rule: a wall or grace clock names
/// its local hour, a conversation or hand-stamped hold just says held.
fn park_receipt(session_id: &str, payload_first_line: &str, name: &str) -> String {
    let now = chrono::Utc::now();
    let verdict = gate(session_id, None, now);
    let time_part = match (&verdict.receipt, verdict.until) {
        (Some(r), _) if r.starts_with("held until about ") => {
            let hhmm = r
                .trim_start_matches("held until about ")
                .split(':')
                .take(2)
                .collect::<Vec<_>>()
                .join(":");
            // The receipt's clock is already local and prefixed; reuse it.
            format!("held until about {hhmm}")
        }
        _ => "held".to_string(),
    };
    format!("{time_part}: {payload_first_line} runs on {name} when the hold ends")
}

/// `--park` (C15): a raw send to a held session waits instead of refusing.
/// The payload lands in the hold store, one detached runner is made sure of,
/// and the sender gets a receipt naming when it will run. The runner
/// inherits this process's environment, so the replayed send is stamped
/// with the original sender.
fn park_payload(session_id: &str, payload: &str) -> i32 {
    match park_payload_inner(session_id, payload) {
        Ok(receipt) => {
            println!("{receipt}");
            0
        }
        Err(code) => code,
    }
}

/// The body of [`park_payload`]: park one payload and answer the receipt
/// line, or the caller verb's exit code on failure. Shared with the
/// `--gate --park-on-hold` arm, which answers the receipt in JSON instead.
fn park_payload_inner(session_id: &str, payload: &str) -> Result<String, i32> {
    let Some((matched, name, _)) = lookup_gate_row(session_id) else {
        eprintln!("mail-hold: no registry row carries session {session_id}");
        return Err(3);
    };
    let handle = identity_key(&matched);
    let dir = parked_dir(&handle);
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("mail-hold: could not create the park directory under the hold store");
        return Err(1);
    }
    let file = dir.join(format!(
        "{}-{}.txt",
        chrono::Utc::now().timestamp_millis(),
        std::process::id()
    ));
    if std::fs::write(&file, payload).is_err() {
        eprintln!("mail-hold: could not write the parked payload");
        return Err(1);
    }
    // One runner per held session: a live pid in `runner.pid` stands.
    let pid_file = dir.join("runner.pid");
    let spawn = match std::fs::read_to_string(&pid_file) {
        Ok(text) => text
            .trim()
            .parse::<i32>()
            .map(|pid| !process_alive(pid))
            .unwrap_or(true),
        Err(_) => true,
    };
    if spawn {
        let mut cmd = Command::new(runner_bin());
        cmd.args(["mail-hold", "--run-parked", "--session", session_id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        match cmd.spawn() {
            Ok(child) => {
                let _ = std::fs::write(&pid_file, child.id().to_string());
            }
            Err(exc) => {
                eprintln!("mail-hold: parked the payload but the runner did not start: {exc}");
                return Err(1);
            }
        }
    }
    let first_line = payload.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    Ok(park_receipt(session_id, first_line, &name))
}

/// The binary a spawned runner execs: `FNO_AGENTS_RUNNER_BIN` when declared,
/// else this process's own executable. The override is a test seam: a suite
/// that parks through the real gate pins a stub here so no detached runner
/// escapes the sandbox.
fn runner_bin() -> std::ffi::OsString {
    if let Some(pin) = std::env::var_os("FNO_AGENTS_RUNNER_BIN").filter(|v| !v.is_empty()) {
        return pin;
    }
    std::env::current_exe()
        .map(|exe| exe.into_os_string())
        .unwrap_or_else(|_| std::ffi::OsString::from("fno-agents"))
}

/// What one poll of the parked store did.
enum DrainPoll {
    /// The gate delivered and every payload went out.
    Sent,
    /// The hold is still live (or the store momentarily unreadable): sleep.
    Waiting,
    /// The gate said deliver but a send failed or a payload is unreadable,
    /// so the same files will fail again until something changes.
    Stalled,
}

/// Seconds between the runner's gate polls.
const RUN_PARKED_POLL_S: u64 = 5;

/// The ceiling on a stalled poll's backoff: the sender was told the payload
/// runs when the hold ends, so a failing transport never strands it - the
/// runner retries at doubling intervals up to this cap until its bound.
const RUN_PARKED_STALL_BACKOFF_CAP_S: u64 = 600;

/// The runner's absolute ceiling: a hold can legitimately last hours (a
/// hand-stamped one may never lapse), so the runner keeps polling until its
/// files drain, its session's row vanishes, or this bound fires.
const RUN_PARKED_BOUND_S: u64 = 24 * 3600;

/// One runner poll (C15): with the hold still live it answers
/// [`DrainPoll::Waiting`]; when the gate delivers, every parked file is sent
/// oldest first through the normal raw door and deleted on exit 0
/// ([`DrainPoll::Sent`]). A failed send keeps its file, logs to stderr and
/// answers [`DrainPoll::Stalled`], so the runner bounds the retries instead
/// of spinning.
fn drain_parked_once(session_id: &str) -> DrainPoll {
    let Some((matched, _, _)) = lookup_gate_row(session_id) else {
        return DrainPoll::Waiting;
    };
    let handle = identity_key(&matched);
    let dir = parked_dir(&handle);
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return DrainPoll::Waiting;
    };
    let mut files: Vec<PathBuf> = read_dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    files.sort();
    if files.is_empty() {
        return DrainPoll::Waiting;
    }
    let now = chrono::Utc::now();
    if !gate(session_id, None, now).deliver {
        return DrainPoll::Waiting;
    }
    for file in files {
        let Ok(payload) = std::fs::read_to_string(&file) else {
            eprintln!(
                "mail-hold: parked payload {} unreadable; leaving it parked",
                file.display()
            );
            return DrainPoll::Stalled;
        };
        let mut cmd = Command::new(crate::scrape::fno_py());
        cmd.args(["agents", "mail", "send", session_id, &payload, "--raw"]);
        match cmd.status() {
            Ok(status) if status.success() => {
                let _ = std::fs::remove_file(&file);
            }
            other => {
                eprintln!(
                    "mail-hold: parked payload {} did not send ({other:?}); it stays parked",
                    file.display()
                );
                return DrainPoll::Stalled;
            }
        }
    }
    DrainPoll::Sent
}

/// The poll loop behind [`run_parked`], with the constants as parameters so
/// the backoff is testable in milliseconds.
fn run_parked_loop(
    session_id: &str,
    dir: &std::path::Path,
    poll: std::time::Duration,
    bound: std::time::Duration,
) -> i32 {
    let start = std::time::Instant::now();
    let mut stalled = 0u32;
    while start.elapsed() < bound {
        // A row that vanishes mid-run (the session ended, or a test's home
        // was torn down) leaves every parked file undeliverable: exit and
        // leave the files for the next park to re-arm over.
        if lookup_gate_row(session_id).is_none() {
            break;
        }
        let files = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "txt"))
                    .count()
            })
            .unwrap_or(0);
        if files == 0 {
            break;
        }
        match drain_parked_once(session_id) {
            DrainPoll::Sent => {
                stalled = 0;
                continue;
            }
            // Held (or the store momentarily unreadable): the normal poll
            // interval, and the stall ladder resets.
            DrainPoll::Waiting => {
                stalled = 0;
                std::thread::sleep(poll);
            }
            // The gate said deliver but the send failed: back off along the
            // ladder instead of spinning, and keep trying until the bound.
            DrainPoll::Stalled => {
                stalled += 1;
                let ladder = poll.saturating_mul(1u32 << stalled.min(8));
                std::thread::sleep(ladder.min(std::time::Duration::from_secs(
                    RUN_PARKED_STALL_BACKOFF_CAP_S,
                )));
            }
        }
    }
    0
}

/// `--run-parked`: the detached runner body. Polls the gate every 5 s,
/// drains parked files when the hold ends, and exits when the directory is
/// empty, when the session's row is gone or unknown, or after 24 hours -
/// always leaving undelivered files for the next park to pick up.
fn run_parked(session_id: &str) -> i32 {
    let Some((matched, _, _)) = lookup_gate_row(session_id) else {
        return 3;
    };
    let dir = parked_dir(&identity_key(&matched));
    let pid_file = dir.join("runner.pid");
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    let code = run_parked_loop(
        session_id,
        &dir,
        std::time::Duration::from_secs(RUN_PARKED_POLL_S),
        std::time::Duration::from_secs(RUN_PARKED_BOUND_S),
    );
    let _ = std::fs::remove_file(&pid_file);
    code
}

/// How far past its `until` a clock must sit before the sweep considers its
/// release timer dead: the live timer polls every 15 s, so 60 s of silence
/// means it is gone.
const LAPSED_TIDY_MARGIN_S: i64 = 60;

/// The registry rows whose `bus-only` stamp outlived its clock by more than
/// [`LAPSED_TIDY_MARGIN_S`]: the handles a stale-hold tidy re-arms a release
/// timer for (C16). A live clock is not picked (its own timer is winning),
/// a hand-stamped hold with no clock is never touched, and an unstamped row
/// is none of this sweep's business. Pure so the pick is testable without a
/// spawner.
pub(crate) fn lapsed_hold_handles(
    registry: &crate::state::Registry,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut out = Vec::new();
    for entry in registry
        .entries
        .iter()
        .filter(|e| e.delivery_policy.as_deref() == Some("bus-only"))
    {
        let Some(sid) = entry.harness_session_id.as_deref() else {
            continue;
        };
        let Some(clock) = gate_clock_addresses(sid, sid)
            .iter()
            .find_map(|h| read_clock(h))
        else {
            continue;
        };
        if matches!(clock.until, Some(u) if u < now - chrono::Duration::seconds(LAPSED_TIDY_MARGIN_S))
        {
            out.push(identity_key(sid));
        }
    }
    out
}

/// The C16 sweep arm: every lapsed hold gets its release timer re-armed, so
/// the [DND] badge, the delivery gate and the DND column all read a flag
/// that cannot outlive its clock by more than a sweep plus a poll. The
/// Python release sees a lapsed clock, unstamps the row and delivers what
/// was held.
pub(crate) fn tidy_lapsed_holds(home: &AgentsHome, now: chrono::DateTime<chrono::Utc>) {
    let Ok(registry) = load_registry(&home.registry_json()) else {
        return;
    };
    for handle in lapsed_hold_handles(&registry, now) {
        spawn_release_timer(&handle);
    }
}

/// `fno-agents mail-hold --session <id> [--off | --gate]`
///
/// Arm (default): run the conversation arm and report its outcome.
/// `--off`: clear the clock and unstamp the policy, so a cancelled team's
/// mail delivers normally instead of holding forever on a stamped row with
/// no clock (the never-lapses state). No row for the session: exit 3,
/// nothing written.
/// `--release`: `--off` plus the standard release leg - the sideline
/// menu's Release hold. The stamp lifts exactly as `--off`, the clock is
/// rewritten EXPIRED, and the detached `hold-release` timer an expiring
/// clock arms wakes once, releases, and drains what the hold kept on the
/// bus: the same effect `fno agents mail hold --off` has inside the held
/// session.
/// `--gate`: the delivery gate (C15, C16). Reads the optional body on
/// stdin, prints one JSON verdict line, exits 0. It never writes the
/// REGISTRY: it runs inside callers that may hold the registry lock, and
/// its answer is the single authority every Python injector consults.
/// With `--park-on-hold`, a held non-empty body is also parked (C15) and
/// the verdict comes back `parked` with the park receipt; a failed park
/// falls through to the plain hold verdict.
/// `--render-digest` reads the held-release JSON payload from stdin and
/// writes the Rust-rendered delivery body to stdout; it needs no session id.
/// `--park` / `--run-parked` (C15): a raw send to a held session parks the
/// payload in the hold store; one detached runner polls the gate every 5 s
/// and replays it through the normal raw door when the hold ends.
pub fn run_mail_hold(args: &[String]) -> i32 {
    let mut session: Option<&String> = None;
    let mut off = false;
    let mut release = false;
    let mut gate_mode = false;
    let mut render_digest = false;
    let mut iter = args.iter();
    let mut park = false;
    let mut park_on_hold = false;
    let mut run_parked_mode = false;
    let mut arm_handle: Option<&String> = None;
    let mut arm_kind: Option<&String> = None;
    let mut arm_minutes: Option<&String> = None;
    let mut clear_handle: Option<&String> = None;
    let mut extend_handle: Option<&String> = None;
    let mut read_handle: Option<&String> = None;
    let mut read_first_handles: Vec<&String> = Vec::new();
    let mut describe_handles: Vec<&String> = Vec::new();
    let mut status_line_handle: Option<&String> = None;
    let mut policy_value: Option<&String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next(),
            "--off" => off = true,
            "--release" => release = true,
            "--gate" => gate_mode = true,
            "--render-digest" => render_digest = true,
            "--park" => park = true,
            "--park-on-hold" => park_on_hold = true,
            "--run-parked" => run_parked_mode = true,
            "--arm" => arm_handle = iter.next(),
            "--kind" => arm_kind = iter.next(),
            "--minutes" => arm_minutes = iter.next(),
            "--clear" => clear_handle = iter.next(),
            "--extend" => extend_handle = iter.next(),
            "--read" => read_handle = iter.next(),
            "--read-first" => read_first_handles = iter.by_ref().collect(),
            "--describe" => describe_handles = iter.by_ref().collect(),
            "--status-line" => status_line_handle = iter.next(),
            "--policy" => policy_value = iter.next(),
            other => {
                eprintln!("mail-hold: unknown argument {other:?}");
                return 2;
            }
        }
    }
    if let Some(handle) = arm_handle {
        // The Python arm/arm_wall bodies, ported: idle re-arms on activity
        // (its ceiling is twice the window), wall is a fixed deadline.
        let kind = arm_kind.map(String::as_str).unwrap_or("idle");
        let minutes: i64 = arm_minutes.and_then(|m| m.parse().ok()).unwrap_or(5);
        let window_s = std::cmp::max(1, minutes * 60);
        let now = chrono::Utc::now();
        let until = now + chrono::Duration::seconds(window_s);
        let ceiling = if kind == "wall" {
            None
        } else {
            Some(now + chrono::Duration::seconds(window_s * 2))
        };
        return match write_clock(handle, until, window_s, kind, ceiling, None) {
            Ok(()) => {
                // The third drain trigger (cmd_hold's detached release timer,
                // ported): the hold lifts on the clock, not only at the next
                // prompt or send. A failed spawn answers on stderr and the
                // fallbacks still cover it.
                spawn_release_timer(handle);
                0
            }
            Err(e) => {
                eprintln!("mail-hold: arm failed: {e}");
                2
            }
        };
    }
    if let Some(handle) = clear_handle {
        // Absent is success, not an error.
        match hold_sidecar_path(handle).metadata() {
            Ok(_) => {
                let _ = std::fs::remove_file(hold_sidecar_path(handle));
            }
            Err(_) => {}
        }
        return 0;
    }
    if let Some(handle) = extend_handle {
        // Re-arm an idle hold, or answer empty when there is no live timed
        // hold to extend (no clock, a permanent policy, or one lapsed). A
        // live wall hold returns unchanged so the policy stays live without
        // moving.
        let now = chrono::Utc::now();
        let Some(clock) = read_clock(handle) else {
            return 0;
        };
        let Some(until) = clock.until else {
            return 0;
        };
        let window_s = clock.window_s;
        if until <= now {
            return 0;
        }
        if clock.clock_kind == "wall" {
            println!(
                "{}",
                serde_json::json!({
                    "until": until.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                    "window_s": window_s,
                    "clock_kind": clock.clock_kind,
                })
            );
            return 0;
        }
        let ceiling = clock
            .ceiling
            .unwrap_or_else(|| until + chrono::Duration::seconds(window_s));
        if ceiling <= now {
            return 0;
        }
        let new_until = std::cmp::min(now + chrono::Duration::seconds(window_s), ceiling);
        if write_clock(
            handle,
            new_until,
            window_s,
            "idle",
            Some(ceiling),
            clock.source.as_deref(),
        )
        .is_err()
        {
            return 2;
        }
        println!(
            "{}",
            serde_json::json!({
                "until": new_until.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                "window_s": window_s,
                "clock_kind": "idle",
                "ceiling": ceiling.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })
        );
        return 0;
    }
    if let Some(handle) = read_handle {
        // The clock read Python's read() used to parse out of the file.
        // Absent and unreadable both answer an empty stdout: on the Python
        // side each means "no clock", never an error.
        if let Some(clock) = read_clock(handle) {
            println!("{}", clock_json(&clock));
        }
        return 0;
    }
    if !read_first_handles.is_empty() {
        // First candidate that carries a readable clock, same shape. The
        // candidate sweep stays in Python (it needs the registry entry).
        if let Some(clock) = read_first_handles.iter().find_map(|h| read_clock(h)) {
            println!("{}", clock_json(&clock));
        }
        return 0;
    }
    if !describe_handles.is_empty() {
        // The DND render's one read: the first candidate carrying a clock,
        // else the no-clock shape - never an error. `remaining` mirrors
        // hold.py remaining_label: integer-truncated seconds under a minute,
        // integer-ceiling minutes above.
        let now = chrono::Utc::now();
        let clock = describe_handles.iter().find_map(|h| read_clock(h));
        let (lapsed, remaining) = match &clock {
            None => (false, None),
            Some(c) => match c.until {
                None => (false, Some("held".to_string())),
                Some(until) => {
                    let ms = (until - now).num_milliseconds();
                    if ms <= 0 {
                        (true, None)
                    } else if ms < 60_000 {
                        (false, Some(format!("~{}s", ms / 1000)))
                    } else {
                        (false, Some(format!("~{}m", (ms + 59_999) / 60_000)))
                    }
                }
            },
        };
        println!(
            "{}",
            serde_json::json!({
                "lapsed": lapsed,
                "remaining": remaining,
                "source": clock.as_ref().and_then(|c| c.source.clone()),
                "clock_kind": clock.as_ref().map(|c| c.clock_kind.clone()),
                "until": clock
                    .as_ref()
                    .and_then(|c| c.until)
                    .map(|u| u.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            })
        );
        return 0;
    }
    if let Some(handle) = status_line_handle {
        // hold_cli cmd_hold's --status block, ported string for string. The
        // POLICY is resolved by the caller: it needs the registry row, the
        // line needs only the answer.
        let policy = policy_value.map(String::as_str).unwrap_or("");
        let now = chrono::Utc::now();
        let clock = read_clock(handle);
        let desc = match &clock {
            Some(c) => clock_description(c),
            None => "no expiry".to_string(),
        };
        let line = if policy != "bus-only" {
            format!("{handle}: no hold - mail delivers normally")
        } else if clock.as_ref().and_then(|c| c.source.as_deref()) == Some("conversation") {
            format!(
                "{handle}: holding mail, machine-armed while you talk ({desc}), \
                 lifts about 2 min after your answer"
            )
        } else if clock
            .as_ref()
            .map(|c| c.until.map(|u| u <= now).unwrap_or(false))
            .unwrap_or(false)
        {
            format!(
                "{handle}: holding mail, but the clock disagrees with the \
                 delivery gate - run `fno agents mail hold --off` to clear it"
            )
        } else if clock.as_ref().map(|c| c.until.is_none()).unwrap_or(true) {
            format!("{handle}: holding mail, no expiry (hand-stamped bus-only)")
        } else {
            let until = clock.as_ref().and_then(|c| c.until).expect("checked above");
            let ms = (until - now).num_milliseconds();
            let label = if ms < 60_000 {
                format!("~{}s", ms / 1000)
            } else {
                format!("~{}m", (ms + 59_999) / 60_000)
            };
            format!("{handle}: holding mail, {desc}, lifts in {label}")
        };
        println!("{line}");
        return 0;
    }
    if render_digest {
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() {
            eprintln!("mail-hold: could not read held-mail render input");
            return 2;
        }
        let release = match serde_json::from_str::<crate::mail_header::HeldRelease>(&input) {
            Ok(release) => release,
            Err(error) => {
                eprintln!("mail-hold: invalid held-mail render input: {error}");
                return 2;
            }
        };
        use std::io::Write as _;
        return match std::io::stdout()
            .write_all(crate::mail_header::render_held_release(&release).as_bytes())
        {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("mail-hold: could not write held-mail render: {error}");
                1
            }
        };
    }
    let Some(session_id) = session else {
        eprintln!("mail-hold: --session <session-id> is required");
        return 2;
    };
    if gate_mode {
        // The body is optional. A terminal stdin is never read (the gate is a
        // caller verb, not an interactive prompt); a piped or closed stdin is
        // drained, and a read failure degrades to the no-body gate.
        let mut body = String::new();
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            let _ = std::io::stdin().read_to_string(&mut body);
        }
        let verdict = gate(session_id, Some(&body), chrono::Utc::now());
        if park_on_hold {
            if let Some(line) = gate_parked_line(&verdict, session_id, &body) {
                println!("{line}");
                return 0;
            }
        }
        println!("{}", gate_json(&verdict));
        return 0;
    }
    if park {
        // The payload arrives on stdin (a terminal stdin is never read).
        let mut payload = String::new();
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            let _ = std::io::stdin().read_to_string(&mut payload);
        }
        return park_payload(session_id, &payload);
    }
    if run_parked_mode {
        return run_parked(session_id);
    }
    if off || release {
        // `--off` clears the clock and unstamps the policy. `--release`
        // (the sideline menu's Release hold) is that lift plus the standard
        // release leg: the clock is rewritten EXPIRED, so the detached
        // timer's first wake releases at once and drains what the hold kept
        // on the bus - the same effect `fno agents mail hold --off` has
        // inside the held session, through the timer's own verb.
        match set_policy(session_id, None) {
            Some(matched) => {
                let handle = identity_key(&matched);
                if release {
                    let _ = write_clock(
                        &handle,
                        chrono::Utc::now() - chrono::Duration::seconds(1),
                        1,
                        "wall",
                        None,
                        None,
                    );
                    spawn_release_timer(&handle);
                } else {
                    let _ = std::fs::remove_file(hold_sidecar_path(&handle));
                }
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

    /// Pin FNO_AGENTS_HOME (registry + journal), FNO_HOME (hold sidecars)
    /// and FNO_STATE_DIR (chats store) to one tempdir for `f`, under the
    /// crate-wide env lock (claim_verbs idiom). FNO_PY points at `true` so
    /// the detached release-timer spawn is a no-op the test never waits on.
    /// Prior values are restored, so an ambient FNO_HOME survives the test.
    pub(crate) fn with_hold_env(f: impl FnOnce(&std::path::Path)) {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior: Vec<(String, Option<std::ffi::OsString>)> =
            ["FNO_AGENTS_HOME", "FNO_HOME", "FNO_PY", "FNO_STATE_DIR"]
                .iter()
                .map(|k| (k.to_string(), std::env::var_os(k)))
                .collect();
        let td = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_AGENTS_HOME", td.path());
        std::env::set_var("FNO_HOME", td.path());
        std::env::set_var("FNO_PY", "true");
        std::env::set_var("FNO_STATE_DIR", td.path());
        f(td.path());
        for (key, value) in prior {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
    }

    /// One stored chats message row under `<dir>/chats/`, so the trust
    /// lookup ([`crate::chats::message_sender`]) finds `id` naming `sender`.
    /// The index is never built; the scan fallback reads the file.
    pub(crate) fn write_chats_message(dir: &std::path::Path, id: &str, sender: &str) {
        let chat = dir.join("chats").join(format!("pair-{sender}-worker"));
        std::fs::create_dir_all(&chat).unwrap();
        let line = serde_json::json!({
            "type": "message", "id": id, "from": sender, "body": "note",
        });
        let path = chat.join("messages.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&line.to_string());
        text.push('\n');
        std::fs::write(path, text).unwrap();
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
        crate::registry_store::seed_raw(&dir.join("registry.json"), doc.to_string());
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
            let status = self_status(SID).unwrap();
            assert_eq!(status["clock_live"], true);
            assert_eq!(status["delivery_policy"], "bus-only");
            assert_eq!(status["conversation"], true);
            // The check-in label rides the same fact: the conversation leg
            // of the shared helper, at its live integration point.
            assert_eq!(
                crate::hold_label::hold_attention(&status).unwrap(),
                "mail held while the user talks to you (machine-armed; lifts about 2 min after your answer)"
            );
            // The status verb addresses the same hold by the canonical
            // short handle; the gate must hold on it, not deliver.
            let first8 = identity_key(SID).get(..8).unwrap().to_string();
            assert!(!gate(&first8, None, chrono::Utc::now()).deliver);
            std::fs::remove_file(clock_path(dir, SID)).unwrap();
            let status = self_status(SID).unwrap();
            assert_eq!(status["clock_live"], false);
            assert!(status["clock_until"].is_null());
            assert_eq!(status["delivery_policy"], "bus-only");
            assert_eq!(status["conversation"], false);
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
            // Off, addressed the way a sender or the status verb addresses
            // the row: the canonical short handle, not the full session id.
            let first8 = identity_key(SID).get(..8).unwrap().to_string();
            assert_eq!(
                run_mail_hold(&["--session".into(), first8, "--off".into()]),
                0
            );
            assert!(!clock_path(dir, SID).exists(), "the clock file is gone");
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert!(registry.entries[0].delivery_policy.is_none());
        });
    }

    #[test]
    fn release_lifts_the_hold_and_arms_the_standard_release_leg() {
        with_hold_env(|dir| {
            let _g = env_guard(&["FNO_PY", "PARK_LOG"]);
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            write_clock(
                &identity_key(SID),
                chrono::Utc::now() + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                Some(CONVERSATION_SOURCE),
            )
            .unwrap();
            // The release leg is the standard detached timer; the stub FNO_PY
            // logs its argv, so the test polls the log for the spawn.
            let stub = write_stub_py(dir);
            let log = dir.join("release.log");
            std::env::set_var("FNO_PY", stub.to_string_lossy().to_string());
            std::env::set_var("PARK_LOG", log.to_string_lossy().to_string());
            assert_eq!(
                run_mail_hold(&["--session".into(), SID.into(), "--release".into()]),
                0
            );
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            assert!(registry.entries[0].delivery_policy.is_none());
            // The clock is EXPIRED, not deleted: the timer's first wake must
            // read a lapsed hold and release, never exit on a vanished one.
            let row = clock(dir, SID);
            let until = chrono::DateTime::parse_from_rfc3339(row["until"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
            assert!(
                until <= chrono::Utc::now(),
                "the clock reads expired to the timer"
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let logged = loop {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                if text.contains("hold-release") || std::time::Instant::now() > deadline {
                    break text;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            };
            assert!(
                logged.contains("hold-release"),
                "the standard release leg spawned: {logged:?}"
            );
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
            // A manual hold carries no source, so the same registry stamp
            // reads as plain DND, never the machine-armed state - at the
            // fact and at the label.
            let status = self_status(SID).unwrap();
            assert_eq!(status["conversation"], false);
            assert_eq!(
                crate::hold_label::hold_attention(&status).unwrap(),
                "DND on"
            );
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
            run_mail_hold(&["--bogus".into()]),
            2,
            "an unknown argument refuses"
        );
    }

    /// A registry row already stamped `bus-only`, the gate's held shape.
    fn stamped_row(name: &str, session: &str) -> serde_json::Value {
        let mut row = registry_row(name, session);
        row["delivery_policy"] = serde_json::Value::String("bus-only".into());
        row
    }

    /// Restores snapshotted env vars on drop, so a panicking assert cannot
    /// leak a stamp into the next test (the crate env lock serializes them).
    struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(v) => std::env::set_var(&key, v),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }

    fn env_guard(keys: &[&str]) -> EnvGuard {
        EnvGuard(
            keys.iter()
                .map(|k| (k.to_string(), std::env::var_os(k)))
                .collect(),
        )
    }

    #[test]
    fn gate_delivers_an_unstamped_or_absent_row() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            let now = chrono::Utc::now();
            let v = gate(SID, None, now);
            assert!(v.deliver && v.pass.is_none() && v.receipt.is_none() && v.until.is_none());
            // No row at all: deliver (fail open toward live delivery).
            let v = gate("nobody-here", None, now);
            assert!(v.deliver);
            // An unstamped row with a stray clock still delivers.
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            assert!(gate(SID, None, now).deliver);
        });
    }

    #[test]
    fn gate_delivers_a_lapsed_clock_and_holds_a_live_one() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            let now = chrono::Utc::now();
            write_clock(
                &identity_key(SID),
                now - chrono::Duration::seconds(60),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            let v = gate(SID, None, now);
            assert!(v.deliver, "a clock whose until is past delivers");
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            let v = gate(SID, None, now);
            assert!(!v.deliver);
            assert!(v.receipt.is_some());
        });
    }

    #[test]
    fn gate_holds_a_hand_stamped_row_with_no_receipt() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            let now = chrono::Utc::now();
            // No clock at all, and an explicit `until: null` permanent clock:
            // both hand-stamped shapes hold, and neither prints a receipt (the
            // turn-boundary surface they promise is real).
            let v = gate(SID, None, now);
            assert!(!v.deliver && v.receipt.is_none() && v.until.is_none());
            std::fs::create_dir_all(dir.join("mail-hold")).unwrap();
            std::fs::write(
                clock_path(dir, &identity_key(SID)),
                "{\"until\": null, \"window_s\": null, \"clock_kind\": \"idle\", \"ceiling\": null}\n",
            )
            .unwrap();
            let v = gate(SID, None, now);
            assert!(!v.deliver && v.receipt.is_none());
        });
    }

    #[test]
    fn gate_passes_the_recipients_own_send() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            let now = chrono::Utc::now();
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // Pin the caller's self identity to the held row through the env
            // getter the resolver reads (a complete canonical stamp), with a
            // drop guard so a failed assert cannot leak the stamp.
            let _g = env_guard(&["FNO_HARNESS_NAME", "FNO_HARNESS_SESSION_ID"]);
            std::env::set_var("FNO_HARNESS_NAME", "claude");
            std::env::set_var("FNO_HARNESS_SESSION_ID", SID);
            let v = gate(SID, None, now);
            assert!(v.deliver && v.pass == Some("own"));
            // A different caller stays held.
            std::env::set_var(
                "FNO_HARNESS_SESSION_ID",
                "eeeeeeee-9999-8888-7777-666655554444",
            );
            let v = gate(SID, None, now);
            assert!(!v.deliver);
        });
    }

    #[test]
    fn gate_passes_control_mail_and_holds_a_mid_line_mention() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            let now = chrono::Utc::now();
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            let v = gate(SID, Some("<fno_mail from=\"k\">control: stop"), now);
            assert!(v.deliver && v.pass == Some("control"));
            let v = gate(SID, Some("CONTROL: stop the budget clock"), now);
            assert!(v.deliver && v.pass == Some("control"));
            let v = gate(SID, Some("the control: word mid-line"), now);
            assert!(!v.deliver, "a mention is prose, not the directive");
            // The directive must open the FIRST line, after the tag.
            let v = gate(
                SID,
                Some("<fno_mail from=\"k\">\nstop, control: said late"),
                now,
            );
            assert!(!v.deliver);
            // Header framing: the directive rides the body under the header,
            // and the pass needs the store to back the header's sender.
            write_chats_message(dir, "msg-7", "k");
            write_chats_message(dir, "msg-8", "k");
            let v = gate(SID, Some("`@k · msg-7 · control note`\ncontrol: stop"), now);
            assert!(v.deliver && v.pass == Some("control"));
            // An id the store does not hold (or holds under another sender)
            // reads as prose, never as the directive.
            let v = gate(SID, Some("`@k · msg-9 · control note`\ncontrol: stop"), now);
            assert!(!v.deliver, "an unbacked header is not a control pass");
            let v = gate(SID, Some("`@j · msg-7 · control note`\ncontrol: stop"), now);
            assert!(!v.deliver, "a mismatched sender is not a control pass");
            // ...and never from the second body line.
            let v = gate(
                SID,
                Some("`@k · msg-8 · hello`\nbody prose\ncontrol: stop"),
                now,
            );
            assert!(!v.deliver);
        });
    }

    #[test]
    fn gate_receipts_name_each_clock_shape() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            let now = chrono::Utc::now();
            // Answering-phase conversation clock: no wall time, the answer end.
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(ANSWER_BACKSTOP_S),
                ANSWER_BACKSTOP_S,
                "wall",
                None,
                Some(CONVERSATION_SOURCE),
            )
            .unwrap();
            let receipt = gate(SID, None, now).receipt.unwrap();
            assert!(
                receipt == "held: worker is in a conversation with the user; delivers about 2 minutes after the answer ends",
                "{receipt}"
            );
            // Conversation clock inside its grace: reads as the wall clock it is.
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(GRACE_S - 10),
                GRACE_S,
                "wall",
                None,
                Some(CONVERSATION_SOURCE),
            )
            .unwrap();
            let receipt = gate(SID, None, now).receipt.unwrap();
            assert!(receipt.starts_with("held until about "), "{receipt}");
            assert!(
                receipt.ends_with("worker is in do-not-disturb (wall clock); delivers itself then"),
                "{receipt}"
            );
            // Idle clock: "or later", because activity restarts it.
            write_clock(
                &identity_key(SID),
                now + chrono::Duration::seconds(300),
                300,
                "idle",
                Some(now + chrono::Duration::seconds(600)),
                None,
            )
            .unwrap();
            let receipt = gate(SID, None, now).receipt.unwrap();
            assert!(
                receipt.starts_with("held until about ") && receipt.contains("or later"),
                "{receipt}"
            );
        });
    }

    #[test]
    fn run_mail_hold_gate_prints_one_json_verdict_and_writes_nothing() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            let before = std::fs::read_dir(dir.join("mail-hold"))
                .map(|rd| rd.count())
                .unwrap_or(0);
            // Live clock: --gate prints hold. stdin carries no body.
            write_clock(
                &identity_key(SID),
                chrono::Utc::now() + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // run_mail_hold reads stdin; the test harness cannot pipe one, so
            // the empty read must degrade to a no-body gate call.
            assert_eq!(
                run_mail_hold(&["--gate".into(), "--session".into(), SID.into()]),
                0
            );
            let after = std::fs::read_dir(dir.join("mail-hold"))
                .map(|rd| rd.count())
                .unwrap_or(0);
            assert_eq!(before + 1, after, "only the clock write above happened");
        });
    }

    /// The stub `fno-py` the runner drains through: appends its argv to
    /// `$PARK_LOG` and exits 1 when `$PARK_FAIL` is set, else 0.
    fn write_stub_py(dir: &std::path::Path) -> std::path::PathBuf {
        crate::write_exec_stub(
            dir,
            "stub-fno-py.sh",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PARK_LOG\"\n[ -n \"$PARK_FAIL\" ] && exit 1\nexit 0\n",
        )
    }

    fn parked_dir_for(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("mail-hold")
            .join(format!("{}.parked", identity_key(SID)))
    }

    #[test]
    fn park_writes_one_file_and_keeps_the_live_runner() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            write_clock(
                &identity_key(SID),
                chrono::Utc::now() + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // A live runner is already on duty (this test process's pid), so
            // park must not spawn a second one over it.
            let pdir = parked_dir_for(dir);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("runner.pid"), std::process::id().to_string()).unwrap();
            assert_eq!(park_payload(SID, "/compact"), 0);
            let parked: Vec<_> = std::fs::read_dir(&pdir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "txt"))
                .collect();
            assert_eq!(parked.len(), 1, "one parked file");
            let pid_text = std::fs::read_to_string(pdir.join("runner.pid")).unwrap();
            assert_eq!(pid_text.trim(), std::process::id().to_string());
        });
    }

    #[test]
    fn gate_park_on_hold_parks_the_body_and_answers_parked() {
        with_hold_env(|dir| {
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            write_clock(
                &identity_key(SID),
                chrono::Utc::now() + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // A live runner is on duty, so the park spawns nothing.
            let pdir = parked_dir_for(dir);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("runner.pid"), std::process::id().to_string()).unwrap();

            let verdict = gate(SID, Some("/compact"), chrono::Utc::now());
            let line = gate_parked_line(&verdict, SID, "/compact").expect("held body parks");
            let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed["verdict"], "parked");
            let receipt = parsed["receipt"].as_str().unwrap();
            assert!(
                receipt.contains("runs on worker when the hold ends"),
                "{receipt}"
            );
            let parked: Vec<_> = std::fs::read_dir(&pdir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "txt"))
                .collect();
            assert_eq!(parked.len(), 1, "the body sits in the hold store");

            // A control body parks too: typed text carries no control
            // envelope, so the hold keeps it until the hold ends. An empty
            // body never parks.
            let control = gate(SID, Some("control: stop"), chrono::Utc::now());
            let parked_control =
                gate_parked_line(&control, SID, "control: stop").expect("control body parks");
            let parsed_control: serde_json::Value = serde_json::from_str(&parked_control).unwrap();
            assert_eq!(parsed_control["verdict"], "parked");
            assert!(gate_parked_line(&verdict, SID, "   ").is_none());
            // A hold with no deadline (hand-stamped, no clock) never parks:
            // the receipt could not name a run-when, and the raw lane's
            // answer for that case is the plain refusal.
            std::fs::remove_file(clock_path(dir, SID)).unwrap();
            let permanent = gate(SID, Some("/compact"), chrono::Utc::now());
            assert!(
                permanent.until.is_none(),
                "a hand-stamped hold carries no deadline"
            );
            assert!(gate_parked_line(&permanent, SID, "/compact").is_none());
            let files_after: usize = std::fs::read_dir(&pdir)
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
                .count();
            assert_eq!(files_after, 2, "the body and the control body parked");
        });
    }

    #[test]
    fn the_runner_sends_nothing_while_held_then_drains_once_and_deletes() {
        with_hold_env(|dir| {
            let _g = env_guard(&["FNO_PY", "PARK_LOG"]);
            write_registry(dir, serde_json::json!([stamped_row("worker", SID)]));
            write_clock(
                &identity_key(SID),
                chrono::Utc::now() + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            let stub = write_stub_py(dir);
            let log = dir.join("drain.log");
            std::env::set_var("FNO_PY", stub.to_string_lossy().to_string());
            std::env::set_var("PARK_LOG", log.to_string_lossy().to_string());
            let pdir = parked_dir_for(dir);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("1-test.txt"), "/compact").unwrap();

            // Held: one poll drains nothing.
            assert!(matches!(drain_parked_once(SID), DrainPoll::Waiting));
            let logged = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(
                logged.is_empty(),
                "a held poll sends nothing, logged {logged:?}"
            );

            // Hold ends: one poll sends the payload once and deletes the file.
            run_mail_hold(&["--session".into(), SID.into(), "--off".into()]);
            assert!(matches!(drain_parked_once(SID), DrainPoll::Sent));
            let logged = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(
                logged.contains("agents mail send")
                    && logged.contains(SID)
                    && logged.contains("/compact"),
                "the replay went through the raw door: {logged:?}"
            );
            assert!(!pdir.join("1-test.txt").exists(), "the sent file is gone");
        });
    }

    #[test]
    fn a_failing_send_keeps_the_parked_file() {
        with_hold_env(|dir| {
            // The guard snapshots BEFORE any set_var, so this test's values
            // cannot leak into a sibling test on the shared process env.
            let _g = env_guard(&["FNO_PY", "PARK_LOG", "PARK_FAIL"]);
            // The row is unstamped so the gate delivers and the drain really
            // attempts the send; a stamped row would answer Waiting before
            // any send ran.
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            let stub = write_stub_py(dir);
            let log = dir.join("fail.log");
            std::env::set_var("FNO_PY", stub.to_string_lossy().to_string());
            std::env::set_var("PARK_LOG", log.to_string_lossy().to_string());
            std::env::set_var("PARK_FAIL", "1");
            let pdir = parked_dir_for(dir);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("1-test.txt"), "/compact").unwrap();

            assert!(
                matches!(drain_parked_once(SID), DrainPoll::Stalled),
                "a failed send stalls the drain"
            );
            assert!(pdir.join("1-test.txt").exists(), "the payload stays parked");
        });
    }

    #[test]
    fn the_runner_retries_a_stall_but_a_vanished_row_ends_the_wait() {
        with_hold_env(|dir| {
            let _g = env_guard(&["FNO_PY", "PARK_LOG", "PARK_FAIL"]);
            // The row is unstamped, so the gate delivers immediately and
            // every send fails: the shape of a hold that ended while the
            // transport is down. The sender was told the payload runs when
            // the hold ends, so the runner must keep trying, not strand it.
            write_registry(dir, serde_json::json!([registry_row("worker", SID)]));
            let stub = write_stub_py(dir);
            let log = dir.join("stall.log");
            std::env::set_var("FNO_PY", stub.to_string_lossy().to_string());
            std::env::set_var("PARK_LOG", log.to_string_lossy().to_string());
            std::env::set_var("PARK_FAIL", "1");
            let pdir = parked_dir_for(dir);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("1-test.txt"), "/compact").unwrap();

            let code = run_parked_loop(
                SID,
                &pdir,
                std::time::Duration::from_millis(2),
                std::time::Duration::from_secs(2),
            );
            assert_eq!(code, 0);
            assert!(pdir.join("1-test.txt").exists(), "the payload stays parked");
            let attempts = std::fs::read_to_string(&log).unwrap().lines().count();
            assert!(
                attempts >= 3,
                "the stall ladder kept retrying, got {attempts}"
            );

            // A vanished row ends the wait long before the bound: nothing
            // can deliver the file once no row carries the session.
            crate::registry_store::seed_raw(
                &dir.join("registry.json"),
                r#"{"schema_version":1,"agents":[]}"#,
            );
            let started = std::time::Instant::now();
            let code = run_parked_loop(
                SID,
                &pdir,
                std::time::Duration::from_millis(2),
                std::time::Duration::from_secs(30),
            );
            assert_eq!(code, 0);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "the vanished row ended the wait, took {:?}",
                started.elapsed()
            );
        });
    }

    #[test]
    fn the_tidy_picks_only_lapsed_clocks_past_the_margin() {
        with_hold_env(|dir| {
            let now = chrono::Utc::now();
            let sid_a = "aaaaaaaa-1111-2222-3333-444455556666";
            let sid_b = "bbbbbbbb-1111-2222-3333-444455556666";
            let sid_c = "cccccccc-1111-2222-3333-444455556666";
            let sid_d = "dddddddd-1111-2222-3333-444455556666";
            let sid_e = "eeeeeeee-1111-2222-3333-444455556666";
            write_registry(
                dir,
                serde_json::json!([
                    stamped_row("a", sid_a),
                    stamped_row("b", sid_b),
                    stamped_row("c", sid_c),
                    stamped_row("d", sid_d),
                    registry_row("e", sid_e),
                ]),
            );
            // a: lapsed 90s past its until -> picked.
            write_clock(
                &identity_key(sid_a),
                now - chrono::Duration::seconds(90),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // b: lapsed only 30s -> inside the margin, the live timer may
            // still be circling.
            write_clock(
                &identity_key(sid_b),
                now - chrono::Duration::seconds(30),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // c: live clock -> its own timer is winning.
            write_clock(
                &identity_key(sid_c),
                now + chrono::Duration::seconds(600),
                600,
                "wall",
                None,
                None,
            )
            .unwrap();
            // d: hand-stamped, no clock -> never touched.
            let registry = crate::state::load_registry(&dir.join("registry.json")).unwrap();
            let picked = lapsed_hold_handles(&registry, now);
            assert_eq!(picked, vec![identity_key(sid_a)], "only a lapsed 90s");
        });
    }
}
