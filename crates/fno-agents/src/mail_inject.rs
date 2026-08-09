//! `mail-inject`: the one-shot LIVE-DELIVERY verb `fno mail send` calls to inject
//! an a2a turn into a LIVE adopted `claude --bg` session over the daemon
//! `control.sock`. Python's `_deliver_live` runs it as a binary subprocess and
//! falls back to the durable bus queue ONLY when this reports not-delivered
//! (live-inject-first, durable fallback -- node x-1f23, epic x-07c1).
//!
//! Binary-direct (Python subprocess), NOT a routable `fno agents` verb -- it is
//! dispatched via `matches!` in `client.rs`, like `version`/`--emit-schema`, so it
//! stays out of the verb-parity lists (`RUST_CLIENT_VERBS` / `CLIENT_VERB_USAGE`).
//!
//! Reuses the G1 substrate for roster resolution ([`crate::claude_roster`]) ->
//! `control.sock` + `control.key` and the attach handshake
//! ([`crate::claude_attach`]). Post-attach the socket is a RAW keystroke pipe, so
//! the turn is bracketed-PASTED as raw bytes and submitted with a wire-level CR --
//! NOT an `op:'reply'` JSON frame, which would land (auth key included) as literal
//! text in the recipient input box, unsent (node x-178e). The `<fno_mail>` envelope is
//! rendered Python-side (the single renderer, shared by the codex/gemini + relay
//! paths) and injected verbatim here, so this verb is a dumb transport.
//!
//! Delivery confirm = the injected turn's `<fno_mail>` open tag appears in the
//! recipient transcript AFTER the inject (content match, [`confirm_content_after`]).
//! A submitted turn is recorded verbatim; an unsent input box records nothing. This
//! replaces the earlier transcript-GROWTH proxy, which false-confirmed on a BUSY
//! recipient whose transcript grows continuously from an unrelated turn (node
//! x-178e).
//!
//! ponytail: content-confirm still has one bounded edge -- a BUSY recipient may
//! queue the injected turn past the poll budget; we report not-confirmed and Python
//! writes the durable fallback, yet the queued paste still lands later, a bounded
//! DOUBLE delivery. Hard exactly-once needs recipient-side msg_id dedup on the
//! envelope (follow-up); the bounded duplicate is the accepted live-first tradeoff.

use std::io::{self, BufRead, Read, Seek};
use std::path::Path;
use std::time::Duration;

use crate::claude_attach::{perform_attach, AttachRequest, UnixControlTransport};
use crate::claude_drive::{contains_detach_sentinel, find_transcript, transcript_len, DriveError};
use crate::claude_roster::{read_control_key, ClaudeRoster};

/// Default transcript-growth poll budget: 40 * 250ms = 10s. A live blocked
/// session echoes the injected turn well within this; a miss demotes to durable.
/// `pub` so the in-process ask-lane fallback (`claude_ask`) reuses the SAME
/// budget the shelled `mail-inject` verb uses, keeping the two paths byte-parity.
pub const DEFAULT_ATTEMPTS: u32 = 40;
pub const DEFAULT_INTERVAL_MS: u64 = 250;

/// Settle delay between the envelope inject and the wire-level CR submit. The
/// paste needs to register in the recipient input box before the Enter
/// keystroke lands; the proven recipe (2026-07-08, CC 2.1.205) used ~0.8s.
const CR_SETTLE_MS: u64 = 800;

/// Interval multiple at which the confirm loop re-sends the wire-level CR. The
/// initial CR (from `inject_with_submit`) can be swallowed mid-paste by a BUSY
/// recipient streaming a turn, leaving the envelope sitting unsent; re-Entering
/// every ~2s (8 * 250ms) lands it once the recipient drains. Idempotent: a bare
/// Enter on an empty/already-submitted input box is a no-op in CC.
const CR_RESUBMIT_EVERY: u32 = 8;

/// Live-inject target harness. `claude` is the default `control.sock` path;
/// `codex` routes to the app-server daemon ([`crate::codex_inject`], US8).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MailInjectProvider {
    Claude,
    Codex,
}

/// Axis-rename tombstone (x-bab1): the harness axis was `--provider`, now
/// `--harness/-H`. A model vendor routes only at spawn. Mirrors the Python
/// `_flag_aliases.PROVIDER_AXIS_TOMBSTONE` (kept in lockstep).
const PROVIDER_AXIS_TOMBSTONE: &str = concat!(
    "--provider was split at the axis rename: the CLI binary is --harness/-H; ",
    "a model vendor is only routable at spawn ",
    "(`fno agents spawn --provider <vendor> --model <m>`).",
);

/// Parsed `mail-inject` flags. The turn TEXT is read from STDIN (sidesteps the
/// argv size limit for envelopes up to the 1 MiB send cap); everything else is a
/// flag.
#[derive(Debug, PartialEq)]
pub struct MailInjectArgs {
    /// Recipient: full session UUID OR its 8-hex short id (roster accepts either)
    /// for claude; the codex threadId (full UUID) for codex.
    pub session: String,
    pub provider: MailInjectProvider,
    pub attempts: u32,
    pub interval_ms: u64,
    /// Sender mail handle for the audit event; absent on a direct binary call.
    pub sender: Option<String>,
}

/// Parse `mail-inject` argv (everything after the verb). Pure + total so the flag
/// grammar is unit-tested without a daemon.
pub fn parse_args(rest: &[String]) -> Result<MailInjectArgs, (i32, String)> {
    let mut session: Option<String> = None;
    let mut provider = MailInjectProvider::Claude;
    let mut attempts = DEFAULT_ATTEMPTS;
    let mut interval_ms = DEFAULT_INTERVAL_MS;
    let mut sender: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => {
                session = Some(
                    it.next()
                        .ok_or((2, "mail-inject: --session needs a value".to_string()))?
                        .to_string(),
                );
            }
            "--harness" | "-H" => {
                provider = match it.next().map(String::as_str) {
                    Some("claude") => MailInjectProvider::Claude,
                    Some("codex") => MailInjectProvider::Codex,
                    _ => {
                        return Err((
                            2,
                            "mail-inject: --harness must be claude or codex".to_string(),
                        ))
                    }
                };
            }
            "--provider" => return Err((2, PROVIDER_AXIS_TOMBSTONE.to_string())),
            "--sender" => {
                sender = Some(
                    it.next()
                        .ok_or((2, "mail-inject: --sender needs a value".to_string()))?
                        .to_string(),
                );
            }
            "--attempts" => {
                attempts = it.next().and_then(|v| v.parse().ok()).ok_or((
                    2,
                    "mail-inject: --attempts needs a positive integer".to_string(),
                ))?;
            }
            "--interval-ms" => {
                interval_ms = it.next().and_then(|v| v.parse().ok()).ok_or((
                    2,
                    "mail-inject: --interval-ms needs a positive integer".to_string(),
                ))?;
            }
            other => {
                return Err((2, format!("mail-inject: unknown flag: {other}")));
            }
        }
    }
    let session = session.ok_or((2, "mail-inject: --session is required".to_string()))?;
    Ok(MailInjectArgs {
        session,
        provider,
        attempts,
        interval_ms,
        sender,
    })
}

/// The single JSON outcome line Python parses: `{"delivered": bool, "reason": str}`.
/// Pure so the contract is unit-tested.
pub fn outcome_json(delivered: bool, reason: &str) -> String {
    serde_json::json!({ "delivered": delivered, "reason": reason }).to_string()
}

/// Exit code for an outcome: 0 when delivered, 1 otherwise. Python branches on the
/// JSON `delivered` field; the exit code is the same signal for shell callers.
pub fn outcome_exit(delivered: bool) -> i32 {
    i32::from(!delivered)
}

/// Append an `agent_raw_inject` audit record for an UNWRAPPED payload, or do
/// nothing when the payload carries an agent-authored envelope marker. An
/// unwrapped injection leaves no such tag in the recipient transcript, so the
/// audit moves to the ledger (x-f26c's greppability property). BOTH wrapped
/// forms are excluded, matching the Python mux-pane site: the ask-lane peer
/// follow-up (`claude::build_cross_session_container` -> `_mail_inject_claude`)
/// ships a `<cross-session-message>` through this very binary, and excluding
/// only `<fno_mail` logs every routine follow-up as a false raw-inject.
/// `confirmed` is the transport's own answer, so a call is audited AFTER the
/// send and never asserts an injection an absent daemon did not perform.
/// Best-effort: a write failure is swallowed and never propagates to the caller.
pub fn emit_raw_inject_audit(
    events_path: &Path,
    sender: Option<&str>,
    session: &str,
    text: &str,
    provider: MailInjectProvider,
    confirmed: bool,
) {
    let head = text.trim_start();
    if head.starts_with("<fno_mail") || head.starts_with("<cross-session-message") {
        return;
    }
    let (harness, lane) = match provider {
        MailInjectProvider::Claude => ("claude", "control.sock"),
        MailInjectProvider::Codex => ("codex", "codex-daemon"),
    };
    let payload_for_event: String = text.chars().take(512).collect();
    let mut fields = serde_json::Map::new();
    fields.insert("target_session".into(), session.to_string().into());
    fields.insert("payload".into(), payload_for_event.into());
    fields.insert("harness".into(), harness.into());
    fields.insert("lane".into(), lane.into());
    fields.insert("confirmed".into(), confirmed.into());
    if let Some(s) = sender {
        fields.insert("sender".into(), s.to_string().into());
    }
    let _ = crate::events::EventEmitter::new(events_path, "daemon")
        .emit_fields("agent_raw_inject", fields);
}

/// Print the outcome JSON to stdout and return its exit code.
fn emit(delivered: bool, reason: &str) -> i32 {
    println!("{}", outcome_json(delivered, reason));
    outcome_exit(delivered)
}

/// Bracketed-paste guards (xterm DEC mode 2004): the recipient TUI treats
/// everything between them as ONE paste event. Required because a `<fno_mail>`
/// envelope is multi-line (`open_tag\nbody\n</fno_mail>`), and a raw multi-line
/// write without them submits line-by-line -- the recipient records the open tag
/// alone (enough to satisfy the content confirm) while the body arrives as
/// separate input, dropping the message. Contract: `docs/architecture/fno-agents-deliver-gate.md`.
const PASTE_BEGIN: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Paste the envelope as RAW BYTES on the ATTACHED transport -- wrapped in
/// bracketed-paste guards so a multi-line body lands as ONE paste -- settle, then
/// send a separate raw `\r` byte as the Enter. Post-attach the `control.sock` is a
/// raw keystroke pipe (node x-178e): an `op:'reply'` JSON write here lands its
/// frames -- auth key included -- as literal text in the recipient input box,
/// unsent. So we type the turn exactly as a human would: paste, then a wire-level
/// CR. The CR is a distinct write, NOT `\r` appended to the paste -- an embedded
/// `\r` is paste content, only a separate keystroke is the Enter. Refuses text
/// carrying a detach sentinel before any write. Extracted so the raw sequence is
/// unit-testable against a `Fake` transport (settle=ZERO).
fn inject_with_submit<T: crate::claude_attach::ControlTransport>(
    transport: &mut T,
    text: &str,
    settle: Duration,
) -> Result<(), DriveError> {
    if contains_detach_sentinel(text) {
        return Err(DriveError::UnsafeText);
    }
    transport
        .send_line(&format!("{PASTE_BEGIN}{text}{PASTE_END}"))
        .map_err(|e| DriveError::Io(e.to_string()))?;
    std::thread::sleep(settle);
    transport
        .send_line("\r")
        .map_err(|e| DriveError::Io(e.to_string()))
}

/// Poll `confirmed` (a content check on the recipient transcript), re-sending the
/// raw wire-level CR every `CR_RESUBMIT_EVERY` intervals so a CR the busy recipient
/// swallowed mid-paste gets re-Entered once it drains. `Ok(())` on a confirmed
/// landing, `Err("not-confirmed")` on budget exhaustion. Extracted from the
/// transport + transcript so the retry cadence is unit-testable against a `Fake`
/// (interval=ZERO). Re-send errors are ignored: it is best-effort, and a dead
/// transport fails the confirm anyway.
fn confirm_with_cr_retry<T: crate::claude_attach::ControlTransport>(
    transport: &mut T,
    attempts: u32,
    interval: Duration,
    mut confirmed: impl FnMut() -> bool,
) -> Result<(), &'static str> {
    for i in 0..attempts.max(1) {
        if confirmed() {
            return Ok(());
        }
        std::thread::sleep(interval);
        if (i + 1) % CR_RESUBMIT_EVERY == 0 {
            let _ = transport.send_line("\r");
        }
    }
    Err("not-confirmed")
}

/// The escaped form of `marker` as it appears inside a transcript JSONL line: the
/// injected turn is stored as a JSON string, so quotes/backslashes in the marker
/// are escaped there too. Strip the surrounding quotes `serde_json` adds, leaving a
/// raw substring to search for.
fn escaped_marker(marker: &str) -> String {
    let s = serde_json::to_string(marker).unwrap_or_default();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or("")
        .to_string()
}

/// Confirm the injected turn LANDED by CONTENT, not transcript growth: scan lines
/// appended after `since_byte` for the injected turn's `marker` (its `<fno_mail>`
/// open tag). A submitted turn is recorded verbatim; an unsent input box records
/// nothing, and a busy recipient's unrelated growth never carries our marker -- so
/// this rejects the growth-only false positive (node x-178e). `since_byte` is a
/// prior full-file length, hence a clean line boundary.
fn confirm_content_after(path: &Path, marker: &str, since_byte: u64) -> io::Result<bool> {
    let escaped = escaped_marker(marker);
    if escaped.is_empty() {
        return Ok(false);
    }
    let mut file = std::fs::File::open(path)?;
    file.seek(io::SeekFrom::Start(since_byte))?;
    for line in io::BufReader::new(file).lines() {
        if line?.contains(&escaped) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Deliver `text` to `session` over the daemon `control.sock`: resolve the
/// recipient on the roster, attach, paste the envelope + wire-level CR submit, and
/// confirm by CONTENT that the injected turn landed in the recipient transcript.
/// `Ok(())` == delivered (the `<fno_mail>` marker appeared after the inject);
/// `Err(reason)` is a clean not-delivered signal whose value IS the `mail-inject`
/// JSON `reason` token.
///
/// The SINGLE control.sock wire implementation (Locked Decision 1, node
/// x-2681): both the `mail-inject` verb (`fno mail send`) and the Rust ask-lane
/// fallback (`claude_ask::ask_followup`) deliver through here, so the wire
/// contract lives in one place and can never drift. `text` is injected verbatim
/// -- a dumb transport; callers wrap it in the `<fno_mail>` /
/// `<cross-session-message>` envelope first.
pub fn deliver_via_control_sock(
    session: &str,
    text: &str,
    attempts: u32,
    interval_ms: u64,
) -> Result<(), &'static str> {
    // Resolve the recipient on the claude daemon roster. Any miss == not live
    // reachable.
    let roster = ClaudeRoster::load_default().map_err(|_| "not-live")?;
    let worker = roster.find(session).ok_or("not-live")?;
    let sock = worker.resolve_control_sock().ok_or("not-live")?;
    let short = worker.short_id().to_string();
    let auth = read_control_key();

    // Locate the recipient transcript. No transcript yet == we cannot confirm
    // landing.
    let transcript = find_transcript(&worker.session_id).ok_or("no-transcript")?;

    let mut transport = UnixControlTransport::connect(&sock).map_err(|_| "io-error")?;
    if perform_attach(
        &mut transport,
        &AttachRequest::for_frame_stream(short.clone(), auth.clone()),
    )
    .is_err()
    {
        return Err("attach-failed");
    }
    // Baseline the transcript byte-length AFTER attach, immediately before inject,
    // so attach side-effects cannot be mistaken for our turn landing (codex peer
    // P2); the content confirm scans only lines appended past this offset.
    let baseline = transcript_len(&transcript);
    // The injected turn's opening line -- its `<fno_mail>` open tag -- is the
    // content marker the confirm greps for; it is recorded verbatim once the turn
    // submits.
    let marker = text.lines().next().unwrap_or(text);
    inject_with_submit(&mut transport, text, Duration::from_millis(CR_SETTLE_MS)).map_err(|e| {
        match e {
            DriveError::UnsafeText => "unsafe-text",
            _ => "io-error",
        }
    })?;

    confirm_with_cr_retry(
        &mut transport,
        attempts,
        Duration::from_millis(interval_ms),
        || confirm_content_after(&transcript, marker, baseline).unwrap_or(false),
    )
}

/// Run `mail-inject`. Reads the turn TEXT from STDIN and delivers it to the
/// target harness (`--harness claude` over `control.sock`, default; `codex`
/// over the app-server daemon, US8); emits the single JSON outcome line Python
/// parses. Every `not-delivered` reason is a clean signal for Python to write
/// the durable fallback. The claude delivery stays sync ([`deliver_via_control_sock`]);
/// codex awaits [`crate::codex_inject::deliver_via_codex_daemon`] on the caller's
/// runtime (no nested runtime).
/// Read a brevity-cap env knob as an int, falling back to `default` when unset or
/// non-numeric. Mirrors Python `_cap_env_int` (cli/src/fno/mail/cli.py): the SAME
/// knob name governs BOTH front doors onto this transport, so the thresholds
/// cannot drift between the Python `fno mail send --raw` entry and this binary.
fn cap_env_int(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

/// Two-tier brevity cap on the injected turn, mirroring Python `_enforce_body_cap`
/// (cli/src/fno/mail/cli.py). This binary is the second front door onto the one
/// transport (`fno mail send --raw` is the first, capping in Python before
/// shelling out here); capping only one door reads as protection and ships green
/// while the other stays open, so both enforce. `n` is UTF-8 byte length (Rust
/// `String::len`), matching Python's `len(body.encode("utf-8"))`. Returns
/// `Some(exit_code)` to refuse (caller returns it without injecting); `None` to
/// proceed. The warn tier is a stderr note only and does not block, matching
/// Python. A tier <= 0 disables it (fail-open); both doors read the same knob, so
/// disabling one disables both.
fn enforce_body_cap(n: usize, warn: i64, refuse: i64) -> Option<i32> {
    if refuse > 0 && n > refuse as usize {
        eprintln!(
            "error: mail body is {n} bytes (cap {refuse}). Relay mail is re-read every turn; \
             put the detail in a node or doc and send a short pointer. \
             Disable with FNO_MAIL_BODY_REFUSE=0 (warn-only) or both knobs 0."
        );
        return Some(1);
    }
    if warn > 0 && n > warn as usize {
        eprintln!(
            "note: mail body is {n} bytes (over the {warn}-byte brevity guide); \
             prefer a short pointer with the detail in a node/doc."
        );
    }
    None
}

pub async fn run_mail_inject(rest: &[String]) -> i32 {
    let args = match parse_args(rest) {
        Ok(a) => a,
        Err((code, msg)) => {
            eprintln!("{msg}");
            return code;
        }
    };

    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("mail-inject: reading stdin: {e}");
        return emit(false, "io-error");
    }

    // Brevity cap, same two tiers as the Python front door. Refuses BEFORE any
    // delivery attempt, so an over-cap body never lands and is not audited as a
    // raw inject. Reads the shared knobs so the direct binary cannot be used to
    // bypass `fno mail send --raw`'s cap.
    if let Some(code) = enforce_body_cap(
        text.len(),
        cap_env_int("FNO_MAIL_BODY_WARN", 3000),
        cap_env_int("FNO_MAIL_BODY_REFUSE", 5000),
    ) {
        return code;
    }

    let result = match args.provider {
        MailInjectProvider::Claude => {
            deliver_via_control_sock(&args.session, &text, args.attempts, args.interval_ms)
        }
        MailInjectProvider::Codex => {
            crate::codex_inject::deliver_via_codex_daemon(&args.session, &text).await
        }
    };

    // Audit floor: record an unwrapped injection in the ledger (no `<fno_mail>`
    // marker survives in the recipient transcript, so x-f26c's greppability
    // property moves from transcript to event). AFTER the delivery, carrying its
    // answer: emitting first left a phantom record on every send to a session
    // with no daemon. Best-effort, never blocks.
    let home = crate::paths::AgentsHome::from_env();
    emit_raw_inject_audit(
        &home.events_jsonl(),
        args.sender.as_deref(),
        &args.session,
        &text,
        args.provider,
        result.is_ok(),
    );

    match result {
        Ok(()) => emit(true, "delivered"),
        Err(reason) => emit(false, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_attach::ControlTransport;
    use crate::claude_drive::DETACH_SENTINELS;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;

    /// Records every raw byte-write, so a test can assert the paste + CR sequence.
    struct Fake {
        sent: Vec<String>,
    }
    impl ControlTransport for Fake {
        fn send_line(&mut self, line: &str) -> io::Result<()> {
            self.sent.push(line.to_string());
            Ok(())
        }
        fn recv_line(&mut self) -> io::Result<Option<String>> {
            Ok(None)
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_accepts_optional_sender() {
        let a = parse_args(&argv(&["--session", "s1", "--sender", "0ab49ebc"])).unwrap();
        assert_eq!(a.session, "s1");
        assert_eq!(a.sender.as_deref(), Some("0ab49ebc"));
        let b = parse_args(&argv(&["--session", "s1", "--harness", "codex"])).unwrap();
        assert!(b.sender.is_none(), "sender defaults to absent");
    }

    #[test]
    fn raw_inject_audit_records_unwrapped_and_skips_envelope() {
        let dir = std::env::temp_dir().join(format!("rawinj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);

        // Unwrapped slash command -> one agent_raw_inject record.
        emit_raw_inject_audit(
            &path,
            Some("0ab49ebc"),
            "ses-9",
            "/code-review medium --fix",
            MailInjectProvider::Claude,
            true,
        );
        // Wrapped envelope -> no record (the marker survives in the transcript).
        emit_raw_inject_audit(
            &path,
            None,
            "ses-9",
            "<fno_mail from=\"a\">hi</fno_mail>",
            MailInjectProvider::Codex,
            true,
        );
        // The ask-lane peer follow-up rides this same binary wrapped in a
        // <cross-session-message> container; it carries its own transcript
        // marker, so auditing it would log every routine follow-up as a false
        // raw-inject (the Python mux site already excludes it).
        emit_raw_inject_audit(
            &path,
            None,
            "ses-9",
            "<cross-session-message from-name=\"peer\">\nstatus?\n</cross-session-message>",
            MailInjectProvider::Claude,
            true,
        );

        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines.len(), 1, "only the unwrapped payload is audited");
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["type"], "agent_raw_inject");
        assert_eq!(v["source"], "daemon");
        assert_eq!(v["data"]["target_session"], "ses-9");
        assert_eq!(v["data"]["payload"], "/code-review medium --fix");
        assert_eq!(v["data"]["harness"], "claude");
        assert_eq!(v["data"]["lane"], "control.sock");
        assert_eq!(v["data"]["sender"], "0ab49ebc");
        assert_eq!(v["data"]["confirmed"], true);

        // A not-delivered send is still audited (the bytes may have landed past
        // the confirm budget) but records confirmed:false, so an auditor reading
        // the ledger as ground truth cannot overcount phantom injections.
        emit_raw_inject_audit(
            &path,
            None,
            "ses-9",
            "/compact",
            MailInjectProvider::Claude,
            false,
        );
        let last: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(last["data"]["confirmed"], false);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    fn tmp_transcript(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mailinj-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("t.jsonl")
    }

    #[test]
    fn inject_with_submit_bracketed_pastes_then_separate_cr() {
        let mut t = Fake { sent: Vec::new() };
        let envelope = "<fno_mail from=\"a1b2c3d4\" node=\"x-178e\">\nhi MARKER\n</fno_mail>";
        inject_with_submit(&mut t, envelope, Duration::ZERO).unwrap();
        // The multi-line envelope is ONE bracketed paste, then a SEPARATE wire-level
        // CR -- not `\r` appended to the paste. Bracketed-paste guards keep the
        // embedded newlines from submitting the body line-by-line.
        assert_eq!(
            t.sent,
            vec![
                format!("{PASTE_BEGIN}{envelope}{PASTE_END}"),
                "\r".to_string()
            ]
        );
        // The paste carries the RAW envelope verbatim, NEVER an op:'reply' JSON frame
        // (the x-178e bug): no `op` key, and the control auth key is never typed in.
        assert!(t.sent[0].contains(envelope), "envelope pasted verbatim");
        assert!(
            !t.sent[0].contains("\"op\""),
            "envelope must be raw bytes, not a JSON op"
        );
        assert!(
            !t.sent[0].contains("auth"),
            "raw paste must never carry the control auth key"
        );
    }

    #[test]
    fn inject_with_submit_refuses_unsafe_envelope_and_writes_nothing() {
        let mut t = Fake { sent: Vec::new() };
        let err = inject_with_submit(&mut t, DETACH_SENTINELS[0], Duration::ZERO);
        assert!(matches!(err, Err(DriveError::UnsafeText)));
        assert!(t.sent.is_empty(), "unsafe envelope must not paste or CR");
    }

    #[test]
    fn body_cap_refuses_over_refuse_tier_and_passes_below_it() {
        // Defaults: warn 3000, refuse 5000. `String::len` is UTF-8 byte length,
        // matching Python's len(body.encode("utf-8")); the repro for the gap this
        // closes is an over-refuse body sailing through the direct binary.
        // Below both tiers: proceeds.
        assert_eq!(enforce_body_cap(100, 3000, 5000), None);
        // Over warn, under refuse: proceeds (warn is a non-blocking note).
        assert_eq!(enforce_body_cap(3001, 3000, 5000), None);
        // At the refuse cap exactly: allowed (strictly greater refuses).
        assert_eq!(enforce_body_cap(5000, 3000, 5000), None);
        // Over refuse: refused with exit 1, the same code Python's typer.Exit(1) yields.
        assert_eq!(enforce_body_cap(5001, 3000, 5000), Some(1));
        // A disabled cap (both knobs 0) fail-opens, matching Python.
        assert_eq!(enforce_body_cap(999_999, 0, 0), None);
    }

    #[test]
    fn busy_recipient_gets_raw_paste_then_retried_crs() {
        let mut t = Fake { sent: Vec::new() };
        inject_with_submit(&mut t, "hi MARKER", Duration::ZERO).unwrap();
        // Confirm never fires -> the loop exhausts its budget, re-Entering a raw CR
        // once per CR_RESUBMIT_EVERY window.
        let attempts = 2 * CR_RESUBMIT_EVERY; // two resubmit windows
        let r = confirm_with_cr_retry(&mut t, attempts, Duration::ZERO, || false);
        assert_eq!(r, Err("not-confirmed"));
        // paste + initial CR (inject_with_submit) + one CR per resubmit window.
        assert_eq!(t.sent.len() as u32, 2 + attempts / CR_RESUBMIT_EVERY);
        // Every write after the paste is a bare raw CR -- no JSON, no auth.
        for line in &t.sent[1..] {
            assert_eq!(line, "\r");
        }
    }

    #[test]
    fn confirm_stops_on_landing_without_extra_cr() {
        let mut t = Fake { sent: Vec::new() };
        let mut calls = 0;
        let r = confirm_with_cr_retry(&mut t, 40, Duration::ZERO, || {
            calls += 1;
            calls >= 2
        });
        assert_eq!(r, Ok(()));
        assert!(
            t.sent.is_empty(),
            "landing before a resubmit window sends no CR"
        );
    }

    #[test]
    fn content_confirm_rejects_growth_and_accepts_the_landed_envelope() {
        let path = tmp_transcript("content");
        let mut f = File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"older"}}}}"#
        )
        .unwrap();
        let baseline = transcript_len(&path);
        let marker = "<fno_mail from=\"a1b2c3d4\" node=\"x-178e\">";

        // A BUSY recipient GROWS the transcript with unrelated output -> growth
        // alone must NOT confirm.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"streaming something else"}}}}"#
        )
        .unwrap();
        assert!(
            !confirm_content_after(&path, marker, baseline).unwrap(),
            "growth without the marker must not confirm"
        );

        // The injected turn lands verbatim (JSON-escaped) -> confirm by content.
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"{}\nhi\n</fno_mail>"}}}}"#,
            escaped_marker(marker)
        )
        .unwrap();
        assert!(
            confirm_content_after(&path, marker, baseline).unwrap(),
            "the landed envelope confirms delivery"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn parse_args_requires_session() {
        assert_eq!(parse_args(&[]).unwrap_err().0, 2);
        assert_eq!(
            parse_args(&argv(&["--attempts", "5"])).unwrap_err().0,
            2,
            "no --session is an error even with other flags"
        );
    }

    #[test]
    fn parse_args_defaults_and_overrides() {
        let a = parse_args(&argv(&["--session", "a1b2c3d4"])).unwrap();
        assert_eq!(a.session, "a1b2c3d4");
        assert_eq!(a.provider, MailInjectProvider::Claude);
        assert_eq!(a.attempts, DEFAULT_ATTEMPTS);
        assert_eq!(a.interval_ms, DEFAULT_INTERVAL_MS);

        let b = parse_args(&argv(&[
            "--session",
            "a1b2c3d4-1111-2222-3333-444455556666",
            "--attempts",
            "3",
            "--interval-ms",
            "10",
        ]))
        .unwrap();
        assert_eq!(b.session, "a1b2c3d4-1111-2222-3333-444455556666");
        assert_eq!(b.attempts, 3);
        assert_eq!(b.interval_ms, 10);
    }

    #[test]
    fn parse_args_harness_defaults_claude_and_accepts_codex() {
        let d = parse_args(&argv(&["--session", "x"])).unwrap();
        assert_eq!(d.provider, MailInjectProvider::Claude);
        let c = parse_args(&argv(&["--session", "x", "--harness", "codex"])).unwrap();
        assert_eq!(c.provider, MailInjectProvider::Codex);
        // -H is the harness short flag.
        let h = parse_args(&argv(&["--session", "x", "-H", "codex"])).unwrap();
        assert_eq!(h.provider, MailInjectProvider::Codex);
        // Unknown harness is a usage error.
        assert_eq!(
            parse_args(&argv(&["--session", "x", "--harness", "gemini"]))
                .unwrap_err()
                .0,
            2
        );
    }

    #[test]
    fn parse_args_provider_is_the_axis_rename_tombstone() {
        // --provider was the harness axis; it now exits 2 with the axis map
        // (x-bab1), regardless of value. Reverting the tombstone arm makes this
        // test fail (AC6).
        let err = parse_args(&argv(&["--session", "x", "--provider", "codex"])).unwrap_err();
        assert_eq!(err.0, 2);
        assert!(
            err.1.contains("--harness/-H"),
            "tombstone points at --harness: {err:?}"
        );
    }

    #[test]
    fn parse_args_rejects_unknown_flag_and_missing_value() {
        assert_eq!(parse_args(&argv(&["--nope"])).unwrap_err().0, 2);
        assert_eq!(parse_args(&argv(&["--session"])).unwrap_err().0, 2);
        assert_eq!(
            parse_args(&argv(&["--session", "x", "--attempts", "notnum"]))
                .unwrap_err()
                .0,
            2
        );
    }

    #[test]
    fn outcome_json_is_the_python_contract() {
        let v: serde_json::Value = serde_json::from_str(&outcome_json(true, "delivered")).unwrap();
        assert_eq!(v["delivered"], true);
        assert_eq!(v["reason"], "delivered");
        let w: serde_json::Value = serde_json::from_str(&outcome_json(false, "not-live")).unwrap();
        assert_eq!(w["delivered"], false);
        assert_eq!(w["reason"], "not-live");
    }

    #[test]
    fn outcome_exit_maps_delivered_to_zero() {
        assert_eq!(outcome_exit(true), 0);
        assert_eq!(outcome_exit(false), 1);
    }
}
