//! `fno-agents mail-receipt` - the mail send receipt renderer.
//!
//! The receipt prose (the JSON send receipt, the durable-demotion status
//! assembly, the drain-window clause, the live-lane story, the NOT LANDED
//! block and the stderr recovery ladders) is pure formatting over arguments,
//! so it lives here once and Python keeps thin transports. The lookups that
//! need session or store state (transcript age, the owner TTL table, the
//! deferred-liveness head) stay Python-side and ride in as arguments.
//!
//! Contract: the Python suites assert the parsed receipt shape and the
//! status substrings, so the assembly must match the retired Python bodies
//! exactly, including Python's banker's rounding on the drain window.

use serde_json::json;

/// The live-lane failure reasons: the recipient WAS live and reachable but
/// the inject did not confirm. The durable preamble must not say "is not
/// live" for these - the receipt names the real cause instead.
const LIVE_LANE_REASONS: [&str; 6] = [
    "not-confirmed",
    "attach-failed",
    "io-error",
    "mux-send-failed",
    "unsafe-text",
    "no-confirm-source",
];

fn is_live_lane_failure(reason: Option<&str>) -> bool {
    let Some(reason) = reason else {
        return false;
    };
    reason
        .split(';')
        .any(|token| LIVE_LANE_REASONS.contains(&token) || token.starts_with("mux-send-failed-"))
}

/// Python `round()`: banker's rounding, half to even. The drain window
/// rounds the TTL hours, and a half-hour table value must read the same as
/// it always has.
fn py_round(x: f64) -> i64 {
    let f = x.fract();
    if (f - 0.5).abs() < f64::EPSILON {
        let floor = x.floor() as i64;
        if floor % 2 == 0 {
            floor
        } else {
            x.ceil() as i64
        }
    } else {
        x.round() as i64
    }
}

fn window_clause(ttl_hours: f64) -> String {
    if ttl_hours <= 0.0 {
        return String::new();
    }
    let window = if ttl_hours < 1.0 {
        format!("~{}m", py_round(ttl_hours * 60.0))
    } else {
        format!("~{}h", py_round(ttl_hours))
    };
    format!(" - typically drains within {window} - an empty unread before then is not a failure")
}

/// The positive-stdout wording for a live-lane failure demotion. `age_of`
/// is the coalesced age recipient (age_target or target): the suffix rides
/// only when it is present.
fn durable_leg_story(
    reason: Option<&str>,
    suffix: Option<&str>,
    age_of: Option<&str>,
) -> Option<String> {
    if !is_live_lane_failure(reason) {
        return None;
    }
    let mut story = String::from("live leg unconfirmed");
    for token in (reason.unwrap_or("")).split(';') {
        if let Some(waited) = token.strip_prefix("waited-") {
            story.push_str(&format!(" after {waited}"));
            break;
        }
    }
    if age_of.is_some() {
        if let Some(s) = suffix {
            story.push_str(s);
        }
    }
    Some(format!("{story}; durable leg holds"))
}

fn json_line(msg_id: &str, subject: Option<&str>, to: &str, status: &str) -> String {
    json!({
        "msg_id": msg_id,
        "subject": subject,
        "to": to,
        "status": status,
    })
    .to_string()
}

fn demotion(
    msg_id: &str,
    reason: Option<&str>,
    ttl_hours: f64,
    target: Option<&str>,
    project: Option<&str>,
    age_of: Option<&str>,
    suffix: Option<&str>,
    subject: Option<&str>,
) -> String {
    let mut token = match durable_leg_story(reason, suffix, age_of) {
        Some(story) => story,
        None => {
            let mut t = reason.unwrap_or("live-miss").to_string();
            if (t == "live-miss" || t.starts_with("transcript-")) && age_of.is_some() {
                if let Some(s) = suffix {
                    t.push_str(s);
                }
            }
            t
        }
    };
    let mut status = format!("queued (durable) [{token}]");
    status.push_str(&window_clause(ttl_hours));
    if let Some(p) = project {
        status.push_str(&format!(" [project {p}]"));
    }
    let _ = &mut token;
    json_line(msg_id, subject, target.or(project).unwrap_or(""), &status)
}

fn not_landed(
    msg_id: &str,
    pane: Option<i64>,
    target: &str,
    harness: Option<&str>,
    session_id: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "{msg_id} NOT LANDED - not claimed on the bus, not in the recipient transcript"
    )];
    if let Some(pane) = pane {
        lines.push(format!(
            "  read the frame:         fno mux pane read {pane}"
        ));
        lines.push(format!(
            "  envelope in composer?   fno mux pane send {pane} --raw --submit   # presses Enter"
        ));
        lines.push(format!(
            "  then verify:            fno agents peek {target} --grep {msg_id}"
        ));
    } else if harness == Some("codex") && session_id.is_some() {
        lines
            .push("  fno cannot inject a codex thread with no pane; the session's own".to_string());
        lines.push(
            "  surface (the user's Codex window) must receive it. The durable copy".to_string(),
        );
        lines.push("  drains on the recipient's next `fno agents mail unread` poll.".to_string());
        lines.push(format!(
            "  verify later:           fno agents peek {target} --grep {msg_id}"
        ));
    } else {
        lines.push(format!(
            "  verify:                 fno agents peek {target} --grep {msg_id}"
        ));
    }
    lines.join("\n")
}

const PROJECT_ARM: &str = concat!(
    "mail: project inbox ",
    "{TARGET}",
    " has no live drain; queued durably as recovery only - a session must drain the project inbox to read this, and may never do so\n  this is NOT delivery. Address a live session instead: `fno agents top` to find one, then `fno agents mail send <short-id>`",
);

const LOCK_ARM: &str = concat!(
    "mail: live delivery to ",
    "{TARGET}",
    " was not attempted (another verb held ",
    "{TARGET}",
    "'s agent lock past the wait); queued durably. That holder is any verb on this agent - a send, an ask, a spawn, a stop, an rm - so the token proves nothing about the recipient in either direction. Do not resurrect it on this evidence, and do not read it as healthy either: check it.\n  a busy peer may not drain soon, so the rungs that stay open,\n  in this order - a bare re-send DOUBLE-DELIVERS, since the queued\n  copy still lands at the recipient's next drain:\n    fno agents peek ",
    "{TARGET}",
    "     # still taking turns, or just stopped?\n    fno agents mail withdraw <id>      # retract the queued copy FIRST\n    fno agents mail send ",
    "{TARGET}",
    " '<message>'  # then retry live\n  a withdraw that refuses because the recipient already claimed\n  the message is telling you it LANDED. Stop there: re-sending on\n  top of that is the double delivery this ladder exists to avoid.",
);

const LIVE_ARM: &str = concat!(
    "mail: live delivery to ",
    "{TARGET}",
    " not confirmed (",
    "{REASON}",
    "); queued durably as recovery only - the recipient was live and reachable, so the message may still land past the confirm window or sit until the recipient drains its inbox\n  live delivery NOT confirmed - do not wait for a reply, recover:\n    fno agents peek ",
    "{TARGET}",
    "     # did it land? a busy peer may have queued it\n    fno agents resume ",
    "{TARGET}",
    "   # wakes it (claude) or resumes it (other harnesses), then re-send\n    fno agents attach ",
    "{TARGET}",
    "   # drive it yourself (claude)\n    fno agents mail withdraw <id>      # none of the above? retract it",
);

const PLAIN_ARM: &str = concat!(
    "{HEAD}",
    "  live delivery NOT confirmed - do not wait for a reply, recover:\n    fno agents peek ",
    "{TARGET}",
    "     # did it land? a busy peer may have queued it\n    fno agents resume ",
    "{TARGET}",
    "   # wakes it (claude) or resumes it (other harnesses), then re-send\n    fno agents attach ",
    "{TARGET}",
    "   # drive it yourself (claude)\n    fno agents mail withdraw <id>      # none of the above? retract it",
);

fn warn_deferred(target: &str, arm: &str, reason: Option<&str>, head: Option<&str>) -> String {
    let arm_text = match arm {
        "project" => PROJECT_ARM,
        "lock" => LOCK_ARM,
        "live" => LIVE_ARM,
        _ => PLAIN_ARM,
    };
    arm_text
        .replace("{TARGET}", target)
        .replace("{REASON}", reason.unwrap_or_default())
        .replace("{HEAD}", head.unwrap_or_default())
}

/// Entry: flag-value args in, one rendered block on stdout, exit 0. A
/// missing required flag is a transport bug, so it refuses loud on stderr
/// with exit 2 rather than printing a wrong receipt.
pub fn run_mail_receipt(args: &[String]) -> i32 {
    let mut flags: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut positional: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            let name = args[i].trim_start_matches('-');
            if i + 1 < args.len() {
                flags.insert(name, args[i + 1].clone());
                i += 2;
            } else {
                flags.insert(name, String::new());
                i += 1;
            }
        } else {
            positional.push(&args[i]);
            i += 1;
        }
    }
    let get = |k: &str| flags.get(k).map(String::as_str);
    let sub = positional.first().map(|s| s.as_str()).unwrap_or("");
    let out = match sub {
        "json" => json_line(
            get("msg-id").unwrap_or_default(),
            get("subject"),
            get("to").unwrap_or_default(),
            get("status").unwrap_or_default(),
        ),
        "window" => window_clause(get("ttl-hours").and_then(|v| v.parse().ok()).unwrap_or(0.0)),
        "story" => match durable_leg_story(get("reason"), get("suffix"), get("age-of")) {
            Some(s) => s,
            None => String::new(),
        },
        "demotion" => demotion(
            get("msg-id").unwrap_or_default(),
            get("reason"),
            get("ttl-hours").and_then(|v| v.parse().ok()).unwrap_or(0.0),
            get("target"),
            get("project"),
            get("age-of"),
            get("suffix"),
            get("subject"),
        ),
        "not-landed" => not_landed(
            get("msg-id").unwrap_or_default(),
            get("pane").and_then(|v| v.parse().ok()),
            get("target").unwrap_or_default(),
            get("harness"),
            get("session-id"),
        ),
        "warn-deferred" => warn_deferred(
            get("target").unwrap_or_default(),
            get("arm").unwrap_or("plain"),
            get("reason"),
            get("head"),
        ),
        other => {
            eprintln!("mail-receipt: unknown subcommand {other:?}");
            return 2;
        }
    };
    println!("{out}");
    0
}
