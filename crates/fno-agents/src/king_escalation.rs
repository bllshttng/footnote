//! The king-escalation question renderer (x-ff27): one pure function set
//! behind a hidden JSON verb, reached as `fno-agents king-escalation-text`.
//! Python keeps the question fold (`reconcile_channel`) and the liveness
//! read; this side only renders text from the reading the producer passed.
//!
//! A question states only a reading its writer handed over. An empty set is
//! not a reading, so the renderer refuses it as data (`ok: false`) instead of
//! minting "a board the king could not read" for a board that was read fine.

use serde::{Deserialize, Serialize};
use std::io::Read;

/// How many stalled rows the question names before it says "and N more".
const MAX_LISTED_IDS: usize = 20;

const MARKER: &str = "king-escalation";

// Reading ids are minted here so no call site writes a string literal that
// can drift from the parser below. A scope's commas become `+` so a
// comma-splitting consumer never cuts the id in half.
pub fn reading_board_unreadable() -> String {
    "reading:board-unreadable".to_owned()
}

pub fn reading_questions_unreadable() -> String {
    "reading:questions-unreadable".to_owned()
}

pub fn reading_sources_unreadable() -> String {
    "reading:sources-unreadable".to_owned()
}

pub fn reading_undelivered(scope: &str) -> String {
    format!("reading:undelivered:{}", scope.replace(',', "+"))
}

pub fn reading_delivery_unreadable(scope: &str) -> String {
    format!("reading:delivery-unreadable:{}", scope.replace(',', "+"))
}

#[derive(Deserialize)]
pub struct EscalationRequest {
    pub stalled: Vec<String>,
    pub key: String,
    pub reason: String,
    #[serde(default)]
    pub live: Option<bool>,
    #[serde(default)]
    pub unknown_reason: Option<String>,
}

#[derive(Serialize)]
pub struct EscalationAnswer {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn refused(message: &str) -> EscalationAnswer {
    EscalationAnswer {
        ok: false,
        question: None,
        mail: None,
        message: Some(format!("king escalation refused: {message}")),
    }
}

fn subject_for_rows(ids: &[String]) -> String {
    let shown = ids[..ids.len().min(MAX_LISTED_IDS)].join(", ");
    let shown = if ids.len() > MAX_LISTED_IDS {
        format!("{shown}, and {} more", ids.len() - MAX_LISTED_IDS)
    } else {
        shown
    };
    format!(
        "{count} board row(s) nothing is clearing: {shown}",
        count = ids.len()
    )
}

/// One known `reading:` id -> the subject sentence it asserts. Returns None
/// for an unknown kind; the caller refuses.
fn subject_for_reading(id: &str) -> Option<String> {
    let scope_of = |prefix: &str| id.strip_prefix(prefix).map(|s| s.replace('+', ", "));
    if let Some(scope) = scope_of("reading:undelivered:") {
        return Some(format!(
            "a quiet board: no row is actionable, but scope {scope} still has undelivered nodes"
        ));
    }
    if let Some(scope) = scope_of("reading:delivery-unreadable:") {
        return Some(format!(
            "a quiet board whose delivery count for scope {scope} it could not read"
        ));
    }
    match id {
        "reading:board-unreadable" => Some("a board it could not read".to_owned()),
        "reading:questions-unreadable" => {
            Some("a clean board whose operator-question queue it could not read".to_owned())
        }
        "reading:sources-unreadable" => {
            Some("a quiet board with queues it could not read".to_owned())
        }
        _ => None,
    }
}

fn closing_for(live: Option<bool>, unknown_reason: Option<&str>, rows: bool) -> String {
    let mut closing = if live == Some(true) {
        if rows {
            "It is still reigning and holding these rows, so decide whether to \
             unblock them, defer them, or tell it to stand down."
                .to_owned()
        } else {
            "It is still reigning, so decide whether to act on this reading or \
             tell it to stand down."
                .to_owned()
        }
    } else {
        if rows {
            "It has exited, so nothing restarts it on its own - decide whether \
             to unblock these rows, defer them, or crown a new king."
                .to_owned()
        } else {
            "It has exited, so nothing restarts it on its own - decide whether \
             to act on this reading or crown a new king."
                .to_owned()
        }
    };
    if live.is_none() {
        if let Some(reason) = unknown_reason {
            closing.push_str(&format!(" (liveness unreadable: {reason})"));
        }
    }
    closing
}

/// The one entry point both branches fold through: the marker leads because
/// the recorded text is capped and `already_asked` must keep matching.
pub fn render(req: &EscalationRequest) -> EscalationAnswer {
    let mut rows: Vec<String> = vec![];
    let mut readings: Vec<String> = vec![];
    for id in &req.stalled {
        if id.starts_with("reading:") {
            readings.push(id.clone());
        } else {
            rows.push(id.clone());
        }
    }
    if rows.is_empty() && readings.is_empty() {
        return refused(
            "the stalled set is empty; an escalation states the reading its \
             producer passed, and an empty set states none",
        );
    }
    if !rows.is_empty() && !readings.is_empty() {
        return refused(&format!(
            "reading id(s) [{}] mixed with board row(s) [{}]: one question \
             states one reading of the board",
            readings.join(", "),
            rows.join(", ")
        ));
    }
    if readings.len() > 1 {
        return refused(&format!(
            "multiple readings [{}]: one question states one reading",
            readings.join(", ")
        ));
    }

    let (subject, rows_branch) = if readings.len() == 1 {
        let reading = &readings[0];
        match subject_for_reading(reading) {
            Some(s) => (s, false),
            None => return refused(&format!("unknown reading id [{reading}]")),
        }
    } else {
        rows.sort();
        rows.dedup();
        (subject_for_rows(&rows), true)
    };

    let closing = closing_for(req.live, req.unknown_reason.as_deref(), rows_branch);
    let question = format!(
        "[{MARKER}:{key}] The king stopped on {subject}. Reason given: {reason}. {closing}",
        key = req.key,
        reason = req.reason,
    );
    let mail = format!(
        "A crown under yours stopped on {subject}. Reason given: {reason}. \
         It presides over territory yours contains - check on it before this \
         reaches the operator.",
        subject = subject,
        reason = req.reason,
    );
    EscalationAnswer {
        ok: true,
        question: Some(question),
        mail: Some(mail),
        message: None,
    }
}

/// `fno-agents king-escalation-text`: the hidden binary-direct transport. One
/// JSON request on stdin, one JSON answer on stdout, exit 0 whenever an
/// answer was computed (a refusal is data); exit 2 on malformed args or an
/// unreadable request.
pub fn run_king_escalation_text(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: fno-agents king-escalation-text (one JSON request on stdin)");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("fno-agents king-escalation-text: unexpected arguments; the request rides stdin");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno-agents king-escalation-text: could not read stdin");
        return 2;
    }
    let req: EscalationRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents king-escalation-text: bad request: {e}");
            return 2;
        }
    };
    let answer = serde_json::to_string(&render(&req)).expect("serializes");
    println!("{answer}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [&str; 2] = ["x-1111", "x-2222"];
    const KEY: &str = "0a1b2c3d4e5f";
    const REASON: &str = "NoProgress";
    const DEAD_SENTENCE: &str = "It has exited, so nothing restarts it on its own";

    fn req(stalled: Vec<&str>, live: Option<bool>) -> EscalationRequest {
        EscalationRequest {
            stalled: stalled.into_iter().map(str::to_owned).collect(),
            key: KEY.to_owned(),
            reason: REASON.to_owned(),
            live,
            unknown_reason: None,
        }
    }

    fn question(req: &EscalationRequest) -> String {
        render(req).question.expect("ok case carries a question")
    }

    // --- the six cases ported out of test_king_escalate_liveness.py ---

    #[test]
    fn live_king_question_says_still_reigning() {
        let text = question(&req(IDS.to_vec(), Some(true)));
        assert!(text.contains("still reigning"));
        assert!(text.contains("stand down"));
        assert!(!text.contains(DEAD_SENTENCE));
    }

    #[test]
    fn dead_king_question_keeps_todays_text_verbatim() {
        let text = question(&req(IDS.to_vec(), Some(false)));
        assert!(text.contains(DEAD_SENTENCE));
        assert!(text.contains("crown a new king"));
        assert!(!text.contains("liveness unreadable"));
    }

    #[test]
    fn unknown_king_reads_dead_and_names_the_reason() {
        let mut r = req(IDS.to_vec(), None);
        r.unknown_reason = Some("registry unreadable: disk".to_owned());
        let text = question(&r);
        assert!(text.contains(DEAD_SENTENCE));
        assert!(text.contains("liveness unreadable"));
        assert!(text.contains("registry unreadable: disk"));
    }

    #[test]
    fn default_live_argument_stays_the_dead_sentence() {
        let text = question(&req(IDS.to_vec(), None));
        assert!(text.contains(DEAD_SENTENCE));
    }

    #[test]
    fn marker_still_leads_and_dedupe_key_ignores_liveness() {
        for live in [Some(true), Some(false), None] {
            let text = question(&req(IDS.to_vec(), live));
            assert!(text.starts_with(&format!("[{MARKER}:{KEY}]")));
        }
    }

    #[test]
    fn every_branch_carries_the_marker_and_reason() {
        for live in [Some(true), Some(false), None] {
            let text = question(&req(IDS.to_vec(), live));
            assert!(text.contains(&format!("[{MARKER}:{KEY}]")));
            assert!(text.contains(&format!("Reason given: {REASON}")));
        }
    }

    // --- AC2-HP: the row branch is byte-identical to today's Python text ---

    #[test]
    fn row_text_is_byte_identical_to_the_python_it_ports() {
        for live in [Some(true), Some(false), None] {
            let text = question(&req(IDS.to_vec(), live));
            let closing = match live {
                Some(true) => "It is still reigning and holding these rows, so \
                               decide whether to unblock them, defer them, or \
                               tell it to stand down."
                    .to_owned(),
                Some(false) => "It has exited, so nothing restarts it on its \
                                own - decide whether to unblock these rows, \
                                defer them, or crown a new king."
                    .to_owned(),
                None => "It has exited, so nothing restarts it on its own - \
                         decide whether to unblock these rows, defer them, or \
                         crown a new king."
                    .to_owned(),
            };
            let want = format!(
                "[king-escalation:{KEY}] The king stopped on 2 board row(s) \
                 nothing is clearing: x-1111, x-2222. Reason given: NoProgress. \
                 {closing}"
            );
            assert_eq!(text, want, "live={live:?}");
        }
    }

    #[test]
    fn row_list_caps_at_twenty_with_and_n_more() {
        let many: Vec<String> = (0..25).map(|i| format!("x-{i:04}")).collect();
        let mut r = req(many.iter().map(String::as_str).collect(), Some(true));
        r.stalled = many;
        let text = question(&r);
        assert!(text.contains("25 board row(s) nothing is clearing:"));
        assert!(text.contains(", and 5 more"));
    }

    #[test]
    fn row_dedupes_and_sorts_before_rendering() {
        let text = question(&req(vec!["x-2222", "x-1111", "x-2222"], Some(true)));
        assert!(text.contains("2 board row(s) nothing is clearing: x-1111, x-2222"));
    }

    // --- the reading branch (AC1-HP, AC6-HP) ---

    #[test]
    fn undelivered_reading_states_a_quiet_board_not_a_blind_one() {
        let text = question(&req(vec!["reading:undelivered:x-a792"], Some(true)));
        assert!(text.starts_with(&format!("[{MARKER}:{KEY}]")));
        assert!(text.contains("quiet board"));
        assert!(!text.contains("could not read"));
        assert!(!text.contains("these rows"));
    }

    #[test]
    fn specimen_question_replays_q_f347e7bc_without_the_false_assertion() {
        let r = EscalationRequest {
            stalled: vec!["reading:undelivered:x-a792".to_owned()],
            key: "87c620d35067".to_owned(),
            reason: "NoProgress".to_owned(),
            live: Some(true),
            unknown_reason: None,
        };
        let text = question(&r);
        assert!(text.starts_with("[king-escalation:87c620d35067]"));
        assert!(text.contains("quiet board"));
        assert!(text.contains("scope x-a792 still has undelivered nodes"));
        assert!(!text.contains("could not read"));
        assert!(!text.contains("these rows"));
    }

    #[test]
    fn scope_commas_survive_the_id_round_trip() {
        assert_eq!(
            reading_undelivered("x-1111,x-2222"),
            "reading:undelivered:x-1111+x-2222"
        );
        let text = question(&req(vec!["reading:undelivered:x-1111+x-2222"], Some(true)));
        assert!(text.contains("scope x-1111, x-2222 still has undelivered nodes"));
    }

    #[test]
    fn every_reading_kind_carries_its_subject() {
        let cases = [
            (reading_board_unreadable(), "a board it could not read"),
            (
                reading_questions_unreadable(),
                "a clean board whose operator-question queue it could not read",
            ),
            (
                reading_sources_unreadable(),
                "a quiet board with queues it could not read",
            ),
            (
                reading_undelivered("x-a792"),
                "a quiet board: no row is actionable, but scope x-a792 still has undelivered nodes",
            ),
            (
                reading_delivery_unreadable("x-a792"),
                "a quiet board whose delivery count for scope x-a792 it could not read",
            ),
        ];
        for (id, subject) in cases {
            let text = question(&req(vec![id.as_str()], Some(true)));
            assert!(text.contains(subject), "{id}: {text}");
        }
    }

    #[test]
    fn reading_branch_closing_names_no_rows() {
        let live = question(&req(vec!["reading:board-unreadable"], Some(true)));
        assert!(live.contains("act on this reading or tell it to stand down"));
        assert!(!live.contains("these rows"));
        let dead = question(&req(vec!["reading:board-unreadable"], Some(false)));
        assert!(dead.contains("act on this reading or crown a new king"));
    }

    // --- AC5-ERR: refusals are data ---

    #[test]
    fn empty_set_is_refused() {
        let ans = render(&req(vec![], None));
        assert!(!ans.ok);
        let msg = ans.message.expect("refusal names itself");
        assert!(msg.contains("king escalation refused"));
        assert!(msg.contains("empty"));
    }

    #[test]
    fn reading_mixed_with_rows_is_refused() {
        let ans = render(&req(vec!["reading:board-unreadable", "x-1111"], Some(true)));
        assert!(!ans.ok);
        let msg = ans.message.expect("refusal names itself");
        assert!(msg.contains("reading:board-unreadable"));
        assert!(msg.contains("x-1111"));
    }

    #[test]
    fn two_readings_are_refused() {
        let ans = render(&req(
            vec!["reading:board-unreadable", "reading:undelivered:x-1"],
            None,
        ));
        assert!(!ans.ok);
        let msg = ans.message.expect("refusal names itself");
        assert!(msg.contains("multiple readings"));
        assert!(msg.contains("reading:board-unreadable"));
    }

    #[test]
    fn unknown_reading_kind_is_refused() {
        let ans = render(&req(vec!["reading:bogus"], None));
        assert!(!ans.ok);
        let msg = ans.message.expect("refusal names itself");
        assert!(msg.contains("unknown reading id [reading:bogus]"));
    }

    // --- the mail twin ---

    #[test]
    fn mail_names_the_subject_and_reason() {
        let ans = render(&req(vec!["reading:undelivered:x-a792"], Some(true)));
        let mail = ans.mail.expect("ok case carries mail");
        assert!(mail.contains("A crown under yours stopped on a quiet board:"));
        assert!(mail.contains("Reason given: NoProgress"));
    }
}
