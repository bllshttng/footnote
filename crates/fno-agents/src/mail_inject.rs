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
//! Reuses the G1 substrate end to end: roster resolution
//! ([`crate::claude_roster`]) -> `control.sock` + `control.key`, then the attach
//! handshake + `op:'reply'` inject ([`crate::claude_attach`] /
//! [`crate::claude_drive`]). The `<fno_mail>` envelope is rendered Python-side (the
//! single renderer, shared by the codex/gemini + relay paths) and injected
//! verbatim here, so this verb is a dumb transport.
//!
//! Delivery confirm = the recipient transcript GREW after the inject, i.e. the
//! injected USER turn was recorded. This is strictly stronger than the
//! pre-unification claude path, which reported "delivered" on socket-write success
//! with no landing check at all. For the TARGET case -- a session idle/blocked at
//! a prompt -- the injected turn is recorded promptly, so growth fires within a
//! poll interval.
//!
//! ponytail: growth-confirm is a best-effort proxy with two bounded edges. (1) A
//! BUSY recipient (mid tool call) queues the injected turn; if it is not recorded
//! within the poll budget we report not-confirmed and Python writes the durable
//! fallback, yet the queued inject still lands later -- a bounded DOUBLE delivery.
//! (2) A concurrent unrelated turn could false-positive a confirm. Hard
//! exactly-once would need recipient-side msg_id dedup on the <fno_mail> envelope
//! (follow-up); the bounded duplicate is the accepted tradeoff for live-first.

use std::io::Read;
use std::time::Duration;

use crate::claude_attach::{perform_attach, AttachRequest, UnixControlTransport};
use crate::claude_drive::{find_transcript, inject_reply, transcript_len, DriveError};
use crate::claude_roster::{read_control_key, ClaudeRoster};

/// Default transcript-growth poll budget: 40 * 250ms = 10s. A live blocked
/// session echoes the injected turn well within this; a miss demotes to durable.
const DEFAULT_ATTEMPTS: u32 = 40;
const DEFAULT_INTERVAL_MS: u64 = 250;

/// Parsed `mail-inject` flags. The turn TEXT is read from STDIN (sidesteps the
/// argv size limit for envelopes up to the 1 MiB send cap); everything else is a
/// flag.
#[derive(Debug, PartialEq)]
pub struct MailInjectArgs {
    /// Recipient: full session UUID OR its 8-hex short id (roster accepts either).
    pub session: String,
    pub attempts: u32,
    pub interval_ms: u64,
}

/// Parse `mail-inject` argv (everything after the verb). Pure + total so the flag
/// grammar is unit-tested without a daemon.
pub fn parse_args(rest: &[String]) -> Result<MailInjectArgs, (i32, String)> {
    let mut session: Option<String> = None;
    let mut attempts = DEFAULT_ATTEMPTS;
    let mut interval_ms = DEFAULT_INTERVAL_MS;
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
        attempts,
        interval_ms,
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

/// Print the outcome JSON to stdout and return its exit code.
fn emit(delivered: bool, reason: &str) -> i32 {
    println!("{}", outcome_json(delivered, reason));
    outcome_exit(delivered)
}

/// Run `mail-inject`. Resolve the recipient on the claude roster, attach to its
/// daemon `control.sock`, inject the STDIN text as an `op:'reply'` turn, and
/// confirm the recipient transcript grew. Every `not-delivered` reason is a clean
/// signal for Python to write the durable fallback. The live socket poll is the
/// only untested glue; every decision it makes is a tested function.
pub fn run_mail_inject(rest: &[String]) -> i32 {
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

    // Resolve the recipient on the claude daemon roster. Any miss == not live
    // reachable -> Python queues durable.
    let roster = match ClaudeRoster::load_default() {
        Ok(r) => r,
        Err(_) => return emit(false, "not-live"),
    };
    let worker = match roster.find(&args.session) {
        Some(w) => w,
        None => return emit(false, "not-live"),
    };
    let sock = match worker.resolve_control_sock() {
        Some(s) => s,
        None => return emit(false, "not-live"),
    };
    let short = worker.short_id().to_string();
    let auth = read_control_key();

    // Baseline the recipient transcript BEFORE injecting so growth proves OUR turn
    // landed. No transcript yet == we cannot confirm landing -> durable.
    let transcript = match find_transcript(&worker.session_id) {
        Some(p) => p,
        None => return emit(false, "no-transcript"),
    };
    let baseline = transcript_len(&transcript);

    let mut transport = match UnixControlTransport::connect(&sock) {
        Ok(t) => t,
        Err(_) => return emit(false, "io-error"),
    };
    if perform_attach(
        &mut transport,
        &AttachRequest::for_frame_stream(short.clone(), auth.clone()),
    )
    .is_err()
    {
        return emit(false, "attach-failed");
    }
    if let Err(e) = inject_reply(&mut transport, &short, &text, auth.as_deref(), None) {
        return match e {
            DriveError::UnsafeText => emit(false, "unsafe-text"),
            _ => emit(false, "io-error"),
        };
    }

    for _ in 0..args.attempts.max(1) {
        if transcript_len(&transcript) > baseline {
            return emit(true, "delivered");
        }
        std::thread::sleep(Duration::from_millis(args.interval_ms));
    }
    emit(false, "not-confirmed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
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
