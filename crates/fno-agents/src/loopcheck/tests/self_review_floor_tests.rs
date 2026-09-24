use super::super::self_review_floor::{
    classify_payload, harness_can_self_review, payload_is_code, REVIEWER_INVOCATIONS,
};
use super::*;

#[test]
fn reviewer_invocations_cover_the_descriptor_table() {
    // The parity script enforces this against the Python side in CI; this
    // keeps the Rust half self-consistent at unit-test speed.
    for (name, inv, self_cert, _per) in REVIEWER_INVOCATIONS {
        assert!(!inv.is_empty(), "{name} has no invocation");
        assert_eq!(
            reviewer_invocation_for(name, None),
            Some((*inv, *self_cert))
        );
    }
    assert_eq!(reviewer_invocation_for("teleport", None), None);
    // AC5: the ONE self-cert must stay visibly marked on this surface too.
    assert_eq!(
        reviewer_invocation_for("declare", None).map(|(_, sc)| sc),
        Some(true)
    );
    assert_eq!(
        reviewer_invocation_for("sigma", None).map(|(_, sc)| sc),
        Some(false)
    );
}

#[test]
fn reviewer_invocation_resolves_the_author_harness_verb() {
    // The owned lane is the invocation on EVERY harness: an inline lane
    // runs wherever the plugin runs, so a transport-dependent verb here
    // would be the fragile part. sigma is retired and also names the lane
    // (the refusal tells a wedged session what to run instead).
    for harness in ["codex", "claude", "opencode", "agy"] {
        assert_eq!(
            reviewer_invocation_for("code-review", Some(harness)),
            Some(("/fno:review", false)),
            "harness {harness} must name the portable lane"
        );
    }
    assert_eq!(
        reviewer_invocation_for("code-review", None),
        Some(("/fno:review", false))
    );
    assert_eq!(
        reviewer_invocation_for("sigma", Some("codex")),
        Some(("/fno:review", false))
    );
}

#[test]
fn code_payload_classifies_code_and_docs() {
    // Code: source, config, lockfile, script. Docs: markdown, docs/.
    assert!(payload_is_code(&["cli/src/fno/x.py".into()]));
    assert!(payload_is_code(&["crates/fno-agents/src/lib.rs".into()]));
    assert!(payload_is_code(&[".fno/config.toml".into()]));
    assert!(payload_is_code(&["Cargo.lock".into()]));
    assert!(payload_is_code(&["scripts/ci/gate.sh".into()]));
    assert!(!payload_is_code(&["README.md".into()]));
    assert!(!payload_is_code(&[
        "docs/architecture/review-lanes.md".into()
    ]));
    assert!(!payload_is_code(&["docs/preflight.txt".into()]));
}

#[test]
fn code_payload_empty_or_docs_only_diff_is_not_code() {
    // No diff -> no ship -> no gate. Docs-only -> unchanged behavior.
    assert!(!payload_is_code(&[]));
    assert!(!payload_is_code(&[
        "docs/a.md".into(),
        "CHANGELOG.md".into()
    ]));
}

#[test]
fn code_payload_mixed_diff_is_code() {
    // One code file among docs is enough to carry a code payload.
    assert!(payload_is_code(&["docs/a.md".into(), "src/lib.rs".into()]));
}

#[test]
fn self_review_gate_classifies_unreadable_diff_as_code() {
    // AC4-ERR: a git that cannot produce a diff fails CLOSED - code with
    // assumed=true - so a degraded probe cannot wave the obligation away.
    let (is_code, assumed) = classify_payload("definitely-not-a-real-git-binary", Path::new("."));
    assert!(is_code);
    assert!(assumed);
}

#[test]
fn self_review_gate_classifies_docs_via_master_fallback() {
    // A master-default repo: the origin/main probe fails, the origin/master
    // probe answers, and the payload classifies from that diff (docs-only
    // waives the obligation) instead of failing closed as assumed code.
    let dir = tempfile::tempdir().unwrap();
    let script = write_exec(
            dir.path(),
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  *origin/main*) exit 1 ;;\n  *origin/master*) printf 'docs/plan.md\\n' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload(script.to_str().unwrap(), Path::new("."));
    assert!(!is_code);
    assert!(!assumed);
}

#[test]
fn self_review_gate_fails_closed_when_main_resolves_but_diffs_fail() {
    // origin/main RESOLVES but its diff fails (unrelated histories exit
    // 128 "no merge base"): the fallback must not quietly re-base onto a
    // sibling origin/master, which can be a stale pre-migration ancestor
    // and would size the payload from a whole era. Fail closed instead.
    let dir = tempfile::tempdir().unwrap();
    let script = write_exec(
            dir.path(),
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  rev-parse*origin/main*) printf 'sha\\n' ;;\n  *origin/main*) exit 1 ;;\n  rev-parse*origin/master*) printf 'sha\\n' ;;\n  *origin/master*) printf 'src/lib.rs\\n' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload(script.to_str().unwrap(), Path::new("."));
    assert!(is_code);
    assert!(assumed);
}

#[test]
fn self_review_gate_floors_code_review_for_a_code_payload() {
    // AC1 floor: a code payload on a lane-less stock install floors
    // code-review onto the required set.
    assert_eq!(
        floor_self_review(&[], false, true, true),
        Some("code-review".to_string())
    );
}

#[test]
fn floor_payload_classifies_the_pr_when_the_cwd_diff_is_empty() {
    // The merge gate classifies the PR (_pr_payload_is_code); a floor that
    // answers for a directory is more permissive than the merge it arms.
    let dir = tempfile::tempdir().unwrap();
    let (git, gh) = floor_payload_stubs(dir.path(), r#"[{"filename":"src/x.rs"}]"#);
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        None,
    );
    assert!(
        is_code,
        "a code PR must floor even where the cwd diff is empty"
    );
    assert!(!assumed);
}

#[test]
fn floor_payload_keeps_a_docs_pr_unfloored_with_an_empty_cwd_diff() {
    // The mirror control: the PR, not the directory, decides both ways.
    let dir = tempfile::tempdir().unwrap();
    let (git, gh) = floor_payload_stubs(dir.path(), r#"[{"filename":"docs/x.md"}]"#);
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        None,
    );
    assert!(!is_code, "a docs-only PR must not floor");
    assert!(!assumed);
}

#[test]
fn floor_payload_falls_back_to_the_cwd_diff_before_the_pr_exists() {
    // No PR for the branch: the cwd answer stands (pre-PR fire).
    let dir = tempfile::tempdir().unwrap();
    let git = write_exec(
            dir.path(),
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  rev-parse*origin/main*) printf 'sha\\n' ;;\n  *origin/main*) printf '' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let gh = write_exec(
            dir.path(),
            "gh",
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'gh version 2.x' ;;\n  *) echo 'no pull requests found' >&2; exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        None,
    );
    assert!(!is_code);
    assert!(!assumed);
}

#[test]
fn floor_payload_classifies_the_named_pr_for_the_coverage_verb() {
    // The review-coverage verb evaluates a PR it names (--pr), from a
    // checkout that need not sit on its branch: the floor must classify
    // THAT PR, and a named PR that resolves to nothing fails closed
    // rather than falling back to the foreign checkout's empty diff.
    let dir = tempfile::tempdir().unwrap();
    let git = write_exec(
            dir.path(),
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  rev-parse*origin/main*) printf 'sha\\n' ;;\n  *origin/main*) printf '' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let gh = write_exec(
            dir.path(),
            "gh",
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'gh version 2.x' ;;\n  *view*) echo '{\"number\":7}' ;;\n  *files*) printf '[{\"filename\":\"src/x.rs\"}]' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        Some("7"),
    );
    assert!(
        is_code,
        "a named code PR must floor even from a foreign checkout"
    );
    assert!(!assumed);

    // The same gh refusing the named PR (no view answer): fail closed,
    // never the foreign checkout's empty-diff answer.
    let gh = write_exec(
            dir.path(),
            "gh-refuses",
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'gh version 2.x' ;;\n  *) echo 'no pull requests found' >&2; exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        Some("7"),
    );
    assert!(is_code, "an unreadable named PR must fail closed");
    assert!(assumed);
}

#[test]
fn floor_payload_fails_closed_when_the_pr_files_read_fails() {
    // A PR exists but its files cannot be read: degraded probe, code.
    let dir = tempfile::tempdir().unwrap();
    let git = write_exec(
            dir.path(),
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  rev-parse*origin/main*) printf 'sha\\n' ;;\n  *origin/main*) printf '' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let gh = write_exec(
            dir.path(),
            "gh",
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'gh version 2.x' ;;\n  *view*) echo '{\"number\":7}' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        None,
    );
    assert!(is_code, "an unreadable PR files read must fail closed");
    assert!(assumed);
}

#[test]
fn floor_payload_short_circuits_on_a_code_cwd_diff_without_asking_gh() {
    // The common case spends no gh: a code cwd diff floors immediately.
    let dir = tempfile::tempdir().unwrap();
    let git = write_exec(
            dir.path(),
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  rev-parse*origin/main*) printf 'sha\\n' ;;\n  *origin/main*) printf 'src/x.rs\\n' ;;\n  *) exit 1 ;;\nesac\n",
        );
    // A gh that would answer a docs-only PR: reaching it is the failure.
    let gh = write_exec(
            dir.path(),
            "gh",
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'gh version 2.x' ;;\n  *view*) echo '{\"number\":7}' ;;\n  *files*) printf '[{\"filename\":\"docs/x.md\"}]' ;;\n  *) exit 1 ;;\nesac\n",
        );
    let (is_code, assumed) = classify_payload_for_floor(
        gh.to_str().unwrap(),
        git.to_str().unwrap(),
        Path::new("."),
        None,
    );
    assert!(is_code);
    assert!(!assumed);
}

#[test]
fn self_review_gate_floor_respects_opt_out_lanes_docs_and_existing() {
    // AC6-CON: opt-out -> None.
    assert_eq!(floor_self_review(&[], false, true, false), None);
    // A configured lane -> None: the lane already expresses review intent.
    assert_eq!(floor_self_review(&[], true, true, true), None);
    // A docs payload -> None: nothing to review.
    assert_eq!(floor_self_review(&[], false, false, true), None);
    // code-review already named -> None: no double-add.
    assert_eq!(
        floor_self_review(&["code-review".to_string()], false, true, true),
        None
    );
    // A leading slash on an existing entry is still recognized as present.
    assert_eq!(
        floor_self_review(&["/code-review".to_string()], false, true, true),
        None
    );
}

#[test]
fn self_review_gate_held_reason_names_code_review_and_its_verb() {
    // AC1-HP: a code payload that reaches the stop gate with no head-pinned
    // code-review attestation is held, and the reason names the reviewer
    // and the verb served by the ambient harness. Both render branches are
    // pinned here, sequentially (one env write per branch, no thread race):
    // a working bridge substitutes the sized render for the map value's
    // `<level>` placeholder, and a missing binary keeps the placeholder.
    // Without the pin the expectation would depend on whatever fno the
    // host has installed.
    let _env_guard = crate::distress::fno_bin_env_test_lock().lock().unwrap(); // shared: distress.rs races this var too
    let var = "FNO_LOOPCHECK_FNO_BIN";
    let prior = std::env::var(var).ok();
    let mut pr = reviewers_gate_pr();
    pr.unattested_reviewers[0].name = "code-review".to_string();

    let tmp = tempfile::tempdir().unwrap();
    let stub = crate::write_exec_stub(
        tmp.path(),
        "fno-stub",
        "#!/bin/sh\nprintf '/code-review from-stub --comment --fix\\n'\n",
    );

    std::env::set_var(var, stub.to_str().unwrap());
    let sized_reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        sized_reason.contains("`/code-review from-stub --comment --fix`"),
        "got: {sized_reason}"
    );
    assert!(
        !sized_reason.contains("<level>"),
        "the placeholder must not survive a working bridge: {sized_reason}"
    );

    std::env::set_var(var, "/nonexistent-fno-for-this-test");
    let reason = build_block_reason(&pr, "abc", true, true);
    match prior {
        Some(v) => std::env::set_var(var, v),
        None => std::env::remove_var(var),
    }
    let harness = crate::claims::resolve_harness();
    let expected = reviewer_invocation_for("code-review", harness.as_deref())
        .expect("code-review descriptor")
        .0;
    assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
    assert!(reason.contains("code-review"), "got: {reason}");
    assert!(reason.contains(&format!("`{expected}`")), "got: {reason}");
    assert!(
        reason.contains("skills/review/scripts/emit-attestation.sh code-review"),
        "got: {reason}"
    );
    assert!(
        reason.contains("local work to DO, not a wait"),
        "got: {reason}"
    );
}

#[test]
fn self_review_gate_floors_every_harness() {
    // The fno-owned review lane runs as ordinary tool calls wherever the
    // plugin runs, so every harness self-reviews now - no KNOWN harness
    // is verbless any more.
    assert!(harness_can_self_review(Some("claude")));
    assert!(harness_can_self_review(Some("codex")));
    assert!(harness_can_self_review(Some("opencode")));
    assert!(harness_can_self_review(Some("gemini")));
    assert!(harness_can_self_review(Some("agy")));
    assert!(harness_can_self_review(None));
}

#[test]
fn unresolved_harness_floors_the_self_review_gate() {
    // `None` means UNATTRIBUTABLE, not verbless. A claude session
    // started from a codex shell resolves no single family, and reading
    // that ambiguity as "no floor" silently disengaged the only review a
    // stock install demands. Ambiguity is not permission; the explicit
    // `--author-harness none` pin stays the hermetic opt-out.
    assert!(self_review_floor_applies(None, false));
    assert!(!self_review_floor_applies(None, true));
    assert!(self_review_floor_applies(Some("claude"), false));
    assert!(self_review_floor_applies(Some("codex"), true));
    // The owned fno review lane runs wherever the plugin runs, so no
    // KNOWN harness is verbless any more: gemini/agy/opencode floor too.
    assert!(self_review_floor_applies(Some("opencode"), false));
    assert!(self_review_floor_applies(Some("gemini"), false));
    assert!(self_review_floor_applies(Some("agy"), false));
    // An unrecognized spelling is an unattributed run: it floors rather
    // than passing an unknown name through the verb table.
    assert!(self_review_floor_applies(Some("hermes"), false));
}
