//! The `publish` operand: POST the coverage verdict as the two commit
//! statuses GitHub's ruleset judges, and print the receipt line. A port of
//! the Python publisher: it reads with the recompute OFF (a publisher must
//! never spawn the minutes-long producer), maps the verdict to the statuses,
//! attempts both contexts even when the first fails, and never fails its
//! caller for a POST - the durable verdict is the event row, and a failed
//! POST leaves the status absent, which the ruleset reads as not-passing.

use super::*;

pub(crate) fn run(args: &[String]) -> i32 {
    let mut pr: Option<u64> = None;
    let mut head: Option<String> = None;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        if let Some(val) = try_flag_value(&args[i], "--head", args, &mut i) {
            head = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--cwd", args, &mut i) {
            cwd = PathBuf::from(val);
        } else if let Some(val) = try_flag_value(&args[i], "--repo", args, &mut i) {
            // The Python spelling named a directory whose git remote names
            // the repository; it is a cwd here like --cwd.
            cwd = PathBuf::from(val);
        } else if !args[i].starts_with('-') {
            pr = args[i].parse::<u64>().ok();
        }
        i += 1;
    }
    let Some(pr) = pr else {
        eprintln!("review-coverage publish: a PR number is required");
        return 2;
    };
    let (posted, note) = publish(&cwd, pr, head.as_deref());
    // The receipt line keeps the Python publisher's key order and compact
    // separators: readers parse it positionally, and a sorted-key rebuild
    // would fork the contract on the first reader that compares bytes.
    println!(
        "{{\"pr\":{},\"posted\":{},\"note\":{}}}",
        pr,
        posted,
        serde_json::to_string(&note).unwrap_or_else(|_| "\"\"".to_string()),
    );
    if posted {
        0
    } else {
        1
    }
}

/// POST the verdict and answer `(posted, note)`. Never raises.
pub(crate) fn publish(cwd: &Path, pr: u64, head: Option<&str>) -> (bool, String) {
    // One resolved directory for every gh spawn below.
    let facts = match crate::authorized_merge::RealProbes.pr_facts(cwd, Some(pr)) {
        Ok(facts) => facts,
        Err(_) => return (false, "no PR head to publish a status on".to_string()),
    };
    let head = match head.filter(|h| !h.is_empty()) {
        Some(h) => h.to_string(),
        None if !facts.head_sha.is_empty() => facts.head_sha.clone(),
        None => return (false, "no PR head to publish a status on".to_string()),
    };
    let v = super::verdict(&crate::authorized_merge::RealProbes, cwd, pr, false);
    let (required_state, required_description): (&str, String) =
        if v.state == GateState::Covered && v.note.starts_with(OVERRIDE_NOTE_PREFIX) {
            // Override first: a labelled or waived PR publishes success naming
            // the lever and its actor, legible in the PR timeline and the audit.
            // The gate read the label - the publisher does not read it a second
            // time, so the status it stamps and the verdict the merge enforces
            // cannot disagree about the valve.
            let pin_head = if v.covered_head.is_empty() {
                head.clone()
            } else {
                v.covered_head.clone()
            };
            return post_pair(
                cwd,
                &pin_head,
                ("success", v.note[OVERRIDE_NOTE_PREFIX.len()..].to_string()),
                false,
            );
        } else if v.state == GateState::Covered && !v.covered_head.is_empty() {
            let count = best_effort_reviewed_count(cwd, pr);
            let description = if count > 0 {
                format!(
                    "covered: {count} reviewed at {}",
                    short_sha(&v.covered_head)
                )
            } else {
                format!("covered at {}", short_sha(&v.covered_head))
            };
            ("success", description)
        } else if v.state == GateState::Covered && v.note == NO_LANE_NOTE {
            // No review lane configured: the merge gate does not apply. Say so
            // on the status instead of reading as a pass on nothing.
            (
                "success",
                "no review lane configured; merge ungated".to_string(),
            )
        } else if v.state == GateState::Covered {
            // Covered, but the row pinned no head. The verdict still stands; it
            // just cannot name the sha it was computed at.
            let count = best_effort_reviewed_count(cwd, pr);
            let description = if count > 0 {
                format!("covered: {count} reviewed (row pinned no head sha)")
            } else {
                "covered (row pinned no head sha)".to_string()
            };
            ("success", description)
        } else if v.state == GateState::Unanswered {
            let detail = if v.note.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", v.note.trim())
            };
            (
                "pending",
                truncate_description(&format!(
                    "coverage read unavailable at {}; retry the review verb{}",
                    short_sha(&head),
                    detail
                )),
            )
        } else {
            (
                "failure",
                truncate_description(&refusal_line(&v.refusal, &v.note)),
            )
        };
    // POST on the head the row pins, not one the caller guessed at: the
    // verdict describes that sha and no other. The override arm above
    // returns its own pin; every other covered-with-head answer moves the
    // post to the pinned sha, the diagnostic sentence included.
    let post_head = if v.state == GateState::Covered && !v.covered_head.is_empty() {
        v.covered_head.clone()
    } else {
        head
    };
    post_pair(
        cwd,
        &post_head,
        (required_state, required_description),
        true,
    )
}

/// Attempt BOTH status posts even when the first fails: a failed
/// required-context POST must not strand a stale diagnostic on a
/// gate-covered PR. Failures join as `<context>: <why>`. `split_diagnostic`
/// marks the answered-unanswered shape where the diagnostic mirrors the
/// required text instead of the healthy sentence.
fn post_pair(
    cwd: &Path,
    head: &str,
    required: (&str, String),
    split_diagnostic: bool,
) -> (bool, String) {
    let diagnostic_state = if required.0 == "pending" {
        "pending"
    } else {
        "success"
    };
    let diagnostic_description = if split_diagnostic && required.0 == "pending" {
        required.1.clone()
    } else {
        format!("coverage read healthy at {}", short_sha(head))
    };
    let mut failures: Vec<String> = Vec::new();
    for (context, state, description) in [
        (
            super::super::coverage_status::COVERAGE_STATUS_CONTEXT,
            required.0,
            required.1,
        ),
        (
            super::super::coverage_status::COVERAGE_UNAVAILABLE_STATUS_CONTEXT,
            diagnostic_state,
            diagnostic_description,
        ),
    ] {
        let posted = post_with_retries(cwd, head, context, state, &description);
        if let Err(why) = posted {
            failures.push(format!("{context}: {why}"));
        }
    }
    if failures.is_empty() {
        (true, String::new())
    } else {
        (false, failures.join("; "))
    }
}

/// The POST with the Python publisher's retry shape: up to three attempts,
/// five seconds apart. One attempt per call site is the producer's shape;
/// a publisher that races a busy ruleset needs the pauses.
fn post_with_retries(
    cwd: &Path,
    head: &str,
    context: &str,
    state: &str,
    description: &str,
) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let args = vec![
            "api".to_string(),
            "--method".to_string(),
            "POST".to_string(),
            format!("repos/:owner/:repo/statuses/{head}"),
            "-f".to_string(),
            format!("state={state}"),
            "-f".to_string(),
            format!("context={context}"),
            "-f".to_string(),
            format!("description={description}"),
        ];
        match crate::authorized_merge::RealProbes.run_gh(cwd, &args) {
            Ok((true, _)) => return Ok(()),
            Ok((false, output)) => {
                if attempt >= 3 {
                    let why: String = output.lines().last().unwrap_or("gh exited nonzero").into();
                    return Err(why.trim().chars().take(200).collect());
                }
            }
            Err(e) => {
                if attempt >= 3 {
                    return Err(e.chars().take(200).collect());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// ASCII-safe cut to GitHub's 140-character description cap with a trailing
/// ellipsis, so a byte truncate cannot split a character or lie about the
/// cut.
fn truncate_description(text: &str) -> String {
    if text.len() <= 140 {
        return text.to_string();
    }
    let mut cut = 137;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

/// The reviewed count off the raw row; 0 when unreadable. Description text
/// only: the verdict above already gated on the count, so a failed re-read
/// degrades the sentence, never the status state.
fn best_effort_reviewed_count(cwd: &Path, pr: u64) -> i64 {
    let row = read_review_coverage(cwd, pr, None, false, false);
    row.get("reviewed_count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}
