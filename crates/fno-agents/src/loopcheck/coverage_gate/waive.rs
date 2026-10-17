//! The `waive` operand: record the attended, head-pinned operator waiver for
//! one PR head and publish the green marker now, so GitHub's ruleset sees
//! the context the law already answers. The write rides the decide door as
//! a child process whose terminal is the operator credential; this operand
//! is a port of the Python command, and every stderr sentence is verbatim.

use super::*;

pub(crate) fn run(args: &[String]) -> i32 {
    let mut pr: Option<u64> = None;
    let mut reason: Option<String> = None;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        if let Some(val) = try_flag_value(&args[i], "--reason", args, &mut i) {
            reason = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "-R", args, &mut i) {
            reason = Some(merge_reason(reason, val));
        } else if !args[i].starts_with('-') {
            pr = args[i].parse::<u64>().ok();
        }
        i += 1;
    }
    let Some(pr) = pr else {
        eprintln!("review-coverage waive: a PR number is required");
        return 2;
    };
    let text = reason.unwrap_or_default().trim().to_string();
    if text.is_empty() {
        eprintln!("coverage-waive refused: --reason is required and empty");
        return 2;
    }
    let Some(slug) = git_origin_slug(&cwd) else {
        eprintln!(
            "coverage-waive refused: repository slug unreadable (git remote \
get-url origin in this checkout)"
        );
        return 4;
    };
    let facts = match crate::authorized_merge::RealProbes.pr_facts(&cwd, Some(pr)) {
        Ok(facts) => facts,
        Err(_) => {
            eprintln!(
                "coverage-waive refused: pr head fetch failed or is not a \
live 40-hex head"
            );
            return 4;
        }
    };
    let head = facts.head_sha;
    if !crate::verify_evidence::full_sha(&head) {
        eprintln!(
            "coverage-waive refused: pr head fetch failed or is not a live \
40-hex head"
        );
        return 4;
    }
    let subject = super::super::coverage_status::scoped_waiver_subject(&slug, pr as i64, &head);
    // The write rides the native decide door at operator authority: the
    // attended terminal is the credential, and the door refuses everything
    // else (exit 3) exactly as the retired Python engine did.
    let fno_bin = loopcheck_fno_bin();
    if fno_bin.is_empty() {
        eprintln!("coverage-waive failed: the native fno binary was not found");
        return 1;
    }
    let child = std::process::Command::new(&fno_bin)
        .args([
            "inbox".to_string(),
            "decide".to_string(),
            subject.clone(),
            super::super::coverage_status::WAIVER_DECISION.to_string(),
            "--rationale".to_string(),
            text,
            "--authority".to_string(),
            "operator".to_string(),
        ])
        .current_dir(&cwd)
        .output();
    let output = match child {
        Ok(output) => output,
        Err(e) => {
            eprintln!(
                "coverage-waive failed: the decide door refused the write \
(exit {}): {}; do not re-run this command - a recorded waiver would double.",
                127, e
            );
            return 1;
        }
    };
    if output.status.code() == Some(3) {
        eprintln!(
            "coverage-waive refused: {} An operator waiver needs an attended \
operator terminal; a session a harness identifies records nothing.",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return 3;
    }
    if !output.status.success() {
        eprintln!(
            "coverage-waive failed: the decide door refused the write (exit \
{}): {}; do not re-run this command - a recorded waiver would double.",
            output.status.code().unwrap_or(1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return 1;
    }
    // Publish the waiver green NOW, so GitHub's ruleset sees the context the
    // law already answers. Best-effort: the recorded law is the authority, a
    // failed POST is the documented publisher lag, and the next publisher
    // run heals it.
    let (posted, why) = super::publish::publish(&cwd, pr, Some(&head));
    if !posted {
        eprintln!(
            "coverage-waive: status publish failed ({}); the waiver is \
recorded and the next publisher run will post it.",
            why
        );
    }
    println!("coverage waiver recorded: {slug}#{pr}@{}", short_sha(&head));
    0
}

fn merge_reason(existing: Option<String>, next: String) -> String {
    match existing {
        Some(prev) => format!("{prev} {next}"),
        None => next,
    }
}
