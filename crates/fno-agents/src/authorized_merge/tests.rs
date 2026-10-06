use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Default)]
struct Fake {
    facts: Option<PrFacts>,
    facts_error: Option<String>,
    /// Answer for the binding probe alone; `None` (the default) reads Clear.
    node_binding: Option<ProbeOutcome>,
    dispatch_hold: Option<ProbeOutcome>,
    review_hold: Option<ProbeOutcome>,
    lineage: Option<ProbeOutcome>,
    merge_result: Option<ProbeOutcome>,
    ci_base: Option<ProbeOutcome>,
    fresh_ci: Option<bool>,
    ci_base_calls: RefCell<u32>,
    checks: Option<String>,
    /// GitHub's ruleset hold for the PR under decision; `None` (the
    /// default) reads no hold, so every pre-existing test keeps its behavior.
    github_block: Option<String>,
    /// Optional-review answer for the PR under decision; the default
    /// `Some(Some(0))` keeps every pre-existing test passing.
    optional_unresolved: Option<Option<i64>>,
    /// Rerun-recovery answer for the PR under decision.
    rerun_recovered: Option<bool>,
    /// The coverage verb's exit for the PR under decision; None reads 0.
    coverage_exit: Option<i32>,
    /// The plan-fidelity gate's verdict for a test-armable refusal.
    plan_fidelity_refused: bool,
    /// The `fno backlog decisions` exit and stdout the head-grant read
    /// gets. `None` exit reads 0; `None` stdout reads empty (malformed).
    decisions_exit: Option<i32>,
    decisions_stdout: Option<Vec<u8>>,
    /// Every fno-shell argv, so a test can assert the subject the grant
    /// read asked for.
    fno_calls: RefCell<Vec<Vec<String>>>,
    covered_head: Option<String>,
    enabled: bool,
    floor: Option<String>,
    gh_ok: bool,
    gh_output: String,
    /// Answer for the REST recovery call alone, so a test can fail the
    /// `gh pr merge` and let the retry succeed.
    gh_recovery_ok: Option<bool>,
    gh_calls: RefCell<Vec<Vec<String>>>,
    /// Simulated merge-slot claim: `None` is free, `Some(pr)` is held.
    slot: RefCell<Option<u64>>,
    slot_holder_err: bool,
    take_slot_err: bool,
    take_slot_calls: RefCell<Vec<u64>>,
    release_slot_calls: RefCell<Vec<u64>>,
    /// Facts and checks for a PR other than the one under decision, keyed
    /// by PR number - a holder read in the slot logic.
    other_facts: RefCell<HashMap<u64, PrFacts>>,
    other_checks: RefCell<HashMap<u64, String>>,
    /// Dispatch holds keyed by PR, so a test can hold a holder without
    /// holding the PR under decision.
    other_holds: RefCell<HashMap<u64, ProbeOutcome>>,
    /// The main-ci verdict the gate reads; `None` (the default) reads the
    /// `pending` word, so every pre-existing test keeps its behavior.
    main_ci: Option<Result<Value, String>>,
    /// The main-repair exemption answer; `None` (the default) reads exempt.
    main_repair: Option<Option<String>>,
}

fn open_facts() -> PrFacts {
    PrFacts {
        number: 7,
        head_sha: "abc123".to_string(),
        head_ref: "feature/x".to_string(),
        base_ref: "main".to_string(),
        url: "https://github.com/o/r/pull/7".to_string(),
        body: Some("Backlog-Closure: x-aaaa\n".to_string()),
        state: "OPEN".to_string(),
        armed: false,
    }
}

fn clean() -> Fake {
    Fake {
        facts: Some(open_facts()),
        covered_head: Some("abc123".to_string()),
        enabled: true,
        gh_ok: true,
        optional_unresolved: Some(Some(0)),
        ..Default::default()
    }
}

struct ClaimsRootRestore(Option<std::ffi::OsString>);

impl Drop for ClaimsRootRestore {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
            None => std::env::remove_var("FNO_CLAIMS_ROOT"),
        }
    }
}

fn with_claims_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    let _env_lock = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let restore = ClaimsRootRestore(std::env::var_os("FNO_CLAIMS_ROOT"));
    std::env::set_var("FNO_CLAIMS_ROOT", root);
    let result = f();
    drop(restore);
    result
}

#[test]
fn slot_holder_reads_lockfiles_and_refuses_corrupted_claims() {
    let temp = tempfile::TempDir::new().unwrap();
    with_claims_root(temp.path(), || {
        let key = slot_key("main");
        assert!(matches!(
            claims::acquire(
                &key,
                &slot_holder_key(17),
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(MERGE_SLOT_TTL_MS),
                    ..Default::default()
                }
            ),
            claims::AcquireOutcome::Acquired(_)
        ));
        assert_eq!(
            slot_holder_read(Path::new("/repo"), "main").unwrap(),
            Some(17)
        );
        assert!(
            !temp.path().join("graph.db").exists(),
            "merge slots must use the claim lockfiles"
        );

        let corrupt = slot_key("broken");
        let path = claims::claim_path(&corrupt, None).unwrap();
        std::fs::write(path, "not: [valid yaml").unwrap();
        let error = slot_holder_read(Path::new("/repo"), "broken").unwrap_err();
        assert!(error.contains("corrupted"), "{error}");
    });
}

#[test]
fn take_slot_refuses_a_live_legacy_lockfile_without_creating_a_second_slot() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(init.success());

    let current_root = temp.path().join("current");
    with_claims_root(&current_root, || {
        let key = slot_key("main");
        assert!(matches!(
            claims::acquire(
                &key,
                &slot_holder_key(8),
                claims::AcquireOpts {
                    root: Some(repo.clone()),
                    pid_unavailable: true,
                    ttl_ms: Some(MERGE_SLOT_TTL_MS),
                    ..Default::default()
                }
            ),
            claims::AcquireOutcome::Acquired(_)
        ));

        let error = RealProbes.take_slot(&repo, "main", 9).unwrap_err();
        assert!(error.contains("pr:8"), "{error}");
        assert!(!claims::claim_path(&key, None).unwrap().exists());
    });
}

impl Probes for Fake {
    fn pr_facts(&self, _cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String> {
        if let Some(error) = &self.facts_error {
            return Err(error.clone());
        }
        let main = self.facts.clone().ok_or_else(|| "no facts".to_string())?;
        match pr {
            Some(n) if n != main.number => self
                .other_facts
                .borrow()
                .get(&n)
                .cloned()
                .ok_or_else(|| format!("no facts for pr {n}")),
            _ => Ok(main),
        }
    }
    fn node_binding(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
        self.node_binding.clone().unwrap_or(ProbeOutcome::Clear)
    }
    fn dispatch_hold(&self, _cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
        if let Some(hold) = self.other_holds.borrow().get(&facts.number) {
            return hold.clone();
        }
        self.dispatch_hold.clone().unwrap_or(ProbeOutcome::Clear)
    }
    fn review_hold(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
        self.review_hold.clone().unwrap_or(ProbeOutcome::Clear)
    }
    fn base_lineage(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
        self.lineage.clone().unwrap_or(ProbeOutcome::Clear)
    }
    fn merge_result(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
        self.merge_result.clone().unwrap_or(ProbeOutcome::Clear)
    }
    fn ci_base(&self, _cwd: &Path, _facts: &PrFacts) -> ProbeOutcome {
        *self.ci_base_calls.borrow_mut() += 1;
        self.ci_base.clone().unwrap_or(ProbeOutcome::Clear)
    }
    fn require_fresh_ci(&self, _cwd: &Path) -> bool {
        self.fresh_ci.unwrap_or(true)
    }
    fn slot_holder(&self, _cwd: &Path, _base_ref: &str) -> Result<Option<u64>, String> {
        if self.slot_holder_err {
            return Err("merge slot claim corrupted".to_string());
        }
        Ok(*self.slot.borrow())
    }
    fn take_slot(&self, _cwd: &Path, _base_ref: &str, pr: u64) -> Result<(), String> {
        self.take_slot_calls.borrow_mut().push(pr);
        if self.take_slot_err {
            return Err("merge slot already held by pr:99".to_string());
        }
        *self.slot.borrow_mut() = Some(pr);
        Ok(())
    }
    fn release_slot(&self, _cwd: &Path, _base_ref: &str, pr: u64) {
        self.release_slot_calls.borrow_mut().push(pr);
        let mut held = self.slot.borrow_mut();
        if *held == Some(pr) {
            *held = None;
        }
    }
    fn checks_read(&self, _cwd: &Path, pr: u64) -> ChecksRead {
        let mine = self.facts.as_ref().map(|f| f.number) == Some(pr);
        let verdict = if mine {
            self.checks.clone().unwrap_or_else(|| "green".to_string())
        } else {
            self.other_checks
                .borrow()
                .get(&pr)
                .cloned()
                .unwrap_or_else(|| "green".to_string())
        };
        ChecksRead {
            verdict,
            github_block: if mine {
                self.github_block.clone()
            } else {
                None
            },
            optional_unresolved: if mine {
                self.optional_unresolved
            } else {
                Some(Some(0))
            },
            rerun_recovered: if mine { self.rerun_recovered } else { None },
            rerun_failures: None,
        }
    }
    fn covered_head(&self, _cwd: &Path) -> Option<String> {
        self.covered_head.clone()
    }
    fn auto_merge_enabled(&self, _cwd: &Path) -> bool {
        self.enabled
    }
    fn posture_floor_block(&self, _cwd: &Path) -> Option<String> {
        self.floor.clone()
    }
    fn strategy(&self, _cwd: &Path) -> String {
        "squash".to_string()
    }
    fn main_ci_token(&self, _cwd: &Path) -> Result<Value, String> {
        self.main_ci
            .clone()
            .unwrap_or_else(|| Ok(Value::String("pending".to_string())))
    }
    fn main_repair_hold(&self, _cwd: &Path, _facts: &PrFacts) -> Option<String> {
        self.main_repair.clone().flatten()
    }
    fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String), String> {
        self.gh_calls.borrow_mut().push(args.to_vec());
        if args.first().map(String::as_str) == Some("api") {
            if let Some(ok) = self.gh_recovery_ok {
                return Ok((ok, String::new()));
            }
        }
        Ok((self.gh_ok, self.gh_output.clone()))
    }
    fn fno_shell(
        &self,
        _cwd: &Path,
        args: &[String],
    ) -> Result<(Option<i32>, Vec<u8>, Vec<u8>), String> {
        self.fno_calls.borrow_mut().push(args.to_vec());
        // The head-grant ask: `backlog decisions <subject> --lane law
        // --state live --json`.
        if args.len() >= 2 && args[0] == "backlog" && args[1] == "decisions" {
            return Ok((
                self.decisions_exit.or(Some(0)),
                self.decisions_stdout.clone().unwrap_or_default(),
                Vec::new(),
            ));
        }
        let covered = if self.plan_fidelity_refused {
            br#"{"refused": true, "reason": "test"}"#.to_vec()
        } else {
            br#"{"refused": false}"#.to_vec()
        };
        // The coverage ask is the `do pr coverage-check` shell; any other
        // fno-shell call is a fidelity ask in these tests.
        let is_coverage = args.len() >= 3 && args[2] == "coverage-check";
        Ok((
            if is_coverage {
                self.coverage_exit.or(Some(0))
            } else {
                Some(0)
            },
            if is_coverage { Vec::new() } else { covered },
            Vec::new(),
        ))
    }
}

fn request(effect: Effect) -> Request {
    Request {
        cwd: PathBuf::from("/tmp"),
        pr: Some(7),
        effect,
        approved: Some(true),
        auto_merge_source: Some("config".to_string()),
        require_checks: false,
        covered_head: None,
        decide_only: false,
        authority: None,
        accept_flake: false,
        supplied_verdict: None,
        supplied_counts: None,
        supplied_rerun_recovered: None,
        supplied_optional_unresolved: None,
        supplied_github_blockers: None,
        supplied_dispatch_hold: None,
        supplied_review_hold: None,
        supplied_facts: None,
    }
}

fn granted_decisions() -> Vec<u8> {
    br#"{"decisions":[{"authority_source":"operator","decision":"merge authorized for this head"}]}"#
        .to_vec()
}

// ── the head-scoped operator merge grant ──────────────────────────────

#[test]
fn a_head_grant_clears_the_per_run_no_merge_and_the_receipt_names_it() {
    // AC2-HP.
    let mut req = request(Effect::Merge);
    req.approved = Some(false);
    let fake = Fake {
        decisions_stdout: Some(granted_decisions()),
        ..clean()
    };
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "merged", "{}", outcome.detail());
    let receipt = outcome.to_json();
    assert_eq!(
        receipt["merge_grant"],
        json!("operator head grant abc123"),
        "{}",
        receipt
    );
}

#[test]
fn the_grant_read_is_scoped_to_this_pr_and_head() {
    // AC2-EDGE: the subject carries the PR's current head, so a grant
    // recorded for an earlier head can never answer this read. The test
    // cwd is not a git repo, so the slug leg is empty.
    let mut req = request(Effect::Merge);
    req.approved = Some(false);
    let fake = Fake {
        decisions_stdout: Some(granted_decisions()),
        ..clean()
    };
    let _ = run(&fake, &req);
    let calls = fake.fno_calls.borrow();
    let decisions = calls
        .iter()
        .find(|args| args.len() >= 2 && args[0] == "backlog" && args[1] == "decisions")
        .expect("the grant read ran");
    assert_eq!(decisions[2], "merge-grant:#7@abc123");
    assert_eq!(decisions[3], "--lane");
    assert_eq!(decisions[7], "--json");
}

#[test]
fn an_absent_grant_refuses_and_names_only_the_attended_command() {
    // AC2-ERR: absence refuses, the remedy is the one sanctioned grant
    // command, and the out-of-band escape is gone from the refusal.
    let mut req = request(Effect::Merge);
    req.approved = Some(false);
    req.auto_merge_source = Some("flag-no-merge".to_string());
    let fake = Fake {
        decisions_stdout: Some(br#"{"decisions":[]}"#.to_vec()),
        ..clean()
    };
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "refused");
    let detail = outcome.detail();
    assert!(detail.contains("absent"), "{detail}");
    assert!(detail.contains("flag-no-merge"), "{detail}");
    assert!(detail.contains("'merge-grant:#7@abc123'"), "{detail}");
    assert!(
        detail.contains("'merge authorized for this head'"),
        "{detail}"
    );
    assert!(detail.ends_with("--authority operator"), "{detail}");
    assert!(!detail.contains("out-of-band"), "{detail}");
    assert!(fake.gh_calls.borrow().is_empty());
}

#[test]
fn a_conflicting_grant_refuses_and_names_conflicting() {
    // AC2-ERR: disagreeing operator rows never grant.
    let mut req = request(Effect::Merge);
    req.approved = Some(false);
    let fake = Fake {
        decisions_stdout: Some(
            br#"{"decisions":[{"authority_source":"operator","decision":"merge authorized for this head"},{"authority_source":"operator","decision":"hold"}]}"#
                .to_vec(),
        ),
        ..clean()
    };
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "refused");
    assert!(
        outcome.detail().contains("conflicting"),
        "{}",
        outcome.detail()
    );
}

#[test]
fn an_unreadable_decisions_read_is_never_a_grant() {
    // AC2-ERR: a nonzero exit or a malformed payload fails closed as
    // unreadable; neither becomes a grant. One contract, two transports.
    let mut req = request(Effect::Merge);
    req.approved = Some(false);
    for fake in [
        Fake {
            decisions_exit: Some(1),
            ..clean()
        },
        Fake {
            decisions_stdout: Some(br#"{"error":"damaged"}"#.to_vec()),
            ..clean()
        },
    ] {
        let outcome = run(&fake, &req);
        assert_eq!(outcome.word(), "refused");
        assert!(
            outcome.detail().contains("unreadable"),
            "{}",
            outcome.detail()
        );
    }
}

#[test]
fn a_preview_no_merge_reports_the_per_run_code_the_owner_returned() {
    // AC3-HP: the preview's blocker code comes from authority_refusal
    // itself, not a re-derived guess.
    let mut req = request(Effect::Preview);
    req.approved = Some(false);
    let fake = Fake {
        decisions_stdout: Some(br#"{"decisions":[]}"#.to_vec()),
        ..clean()
    };
    let facts = fake.pr_facts(Path::new("/tmp"), Some(7)).unwrap();
    match preview_walk(&fake, &req, &facts) {
        PreviewVerdict::Blocked(blockers) => {
            let blocker = blockers
                .iter()
                .find(|b| b.code == "per_run_no_merge")
                .expect("per_run_no_merge blocker");
            assert!(blocker.detail.contains("absent"), "{}", blocker.detail);
        }
        _ => panic!("expected a blocked preview"),
    }
}

#[test]
fn a_granted_no_merge_preview_under_a_dead_switch_reads_auto_merge_disabled() {
    // AC3-ERR: the grant supersedes only the per-run layer; the live
    // config still refuses, and under its own code.
    let mut req = request(Effect::Preview);
    req.approved = Some(false);
    let fake = Fake {
        enabled: false,
        decisions_stdout: Some(granted_decisions()),
        ..clean()
    };
    let facts = fake.pr_facts(Path::new("/tmp"), Some(7)).unwrap();
    match preview_walk(&fake, &req, &facts) {
        PreviewVerdict::Blocked(blockers) => {
            assert!(
                blockers.iter().any(|b| b.code == "auto_merge_disabled"),
                "{blockers:?}"
            );
            assert!(
                !blockers.iter().any(|b| b.code == "per_run_no_merge"),
                "{blockers:?}"
            );
        }
        _ => panic!("expected a blocked preview"),
    }
}

#[test]
fn a_live_review_hold_holds_both_effects_and_calls_no_gh() {
    // AC1-HP. The arm path never read this hold before, so a queue armed at
    // the terminal shipped the code a review was still fixing.
    for effect in [Effect::Merge, Effect::Arm] {
        let fake = Fake {
            review_hold: Some(ProbeOutcome::Refused(
                "review_in_flight: held by tgt-x at abc123".to_string(),
            )),
            ..clean()
        };
        let outcome = run(&fake, &request(effect));
        assert_eq!(outcome.word(), "held", "{effect:?}");
        assert!(outcome.detail().contains("review_in_flight"));
        assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
    }
}

#[test]
fn a_stale_pr_takes_the_free_slot_and_holds_a_second_stale_pr_behind_it() {
    // AC1-HP.
    let mut fake = Fake {
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 3 behind".to_string())),
        ..clean()
    };
    let req7 = Request {
        require_checks: true,
        pr: Some(7),
        ..request(Effect::Merge)
    };
    let outcome7 = run(&fake, &req7);
    assert_eq!(outcome7.word(), "held");
    assert!(outcome7.detail().contains("PR 7 now holds the merge slot"));
    assert_eq!(*fake.slot.borrow(), Some(7));

    fake.facts = Some(PrFacts {
        number: 8,
        head_sha: "def456".to_string(),
        ..open_facts()
    });
    fake.covered_head = Some("def456".to_string());
    let req8 = Request {
        require_checks: true,
        pr: Some(8),
        ..request(Effect::Merge)
    };
    let outcome8 = run(&fake, &req8);
    assert_eq!(outcome8.word(), "held");
    let detail8 = outcome8.detail();
    assert!(detail8.contains("merge_slot_held"));
    assert!(detail8.contains("PR 7"));
    assert_eq!(
        *fake.slot.borrow(),
        Some(7),
        "the slot must stay with the first holder"
    );
}

#[test]
fn a_holder_with_fresh_ci_clears_and_releases_the_slot_on_merge_while_a_racer_waits() {
    // AC2-HP.
    let mut fake = Fake {
        slot: RefCell::new(Some(7)),
        facts: Some(PrFacts {
            number: 9,
            head_sha: "nine".to_string(),
            ..open_facts()
        }),
        covered_head: Some("nine".to_string()),
        ..clean()
    };
    let req9 = Request {
        require_checks: true,
        pr: Some(9),
        ..request(Effect::Merge)
    };
    let outcome9 = run(&fake, &req9);
    assert_eq!(outcome9.word(), "held");
    let detail9 = outcome9.detail();
    assert!(detail9.contains("merge_slot_held"));
    assert!(detail9.contains("PR 7"));
    assert_eq!(
        *fake.slot.borrow(),
        Some(7),
        "the waiting PR must not take the slot"
    );

    fake.facts = Some(PrFacts {
        number: 7,
        head_sha: "abc123".to_string(),
        ..open_facts()
    });
    fake.covered_head = Some("abc123".to_string());
    let req7 = Request {
        require_checks: true,
        pr: Some(7),
        ..request(Effect::Merge)
    };
    let outcome7 = run(&fake, &req7);
    assert_eq!(
        outcome7.word(),
        "merged",
        "the slot holder with fresh CI clears and merges"
    );
    assert_eq!(
        *fake.slot.borrow(),
        None,
        "the slot releases once the Merged outcome lands"
    );
}

#[test]
fn a_holder_that_went_red_releases_its_slot_to_a_waiting_stale_pr() {
    // AC3-EDGE.
    let fake = Fake {
        slot: RefCell::new(Some(7)),
        facts: Some(PrFacts {
            number: 8,
            head_sha: "eight".to_string(),
            ..open_facts()
        }),
        covered_head: Some("eight".to_string()),
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 4 behind".to_string())),
        ..clean()
    };
    fake.other_facts.borrow_mut().insert(
        7,
        PrFacts {
            number: 7,
            state: "OPEN".to_string(),
            ..open_facts()
        },
    );
    fake.other_checks.borrow_mut().insert(7, "red".to_string());

    let req8 = Request {
        require_checks: true,
        pr: Some(8),
        ..request(Effect::Merge)
    };
    let outcome8 = run(&fake, &req8);
    assert_eq!(outcome8.word(), "held");
    assert!(outcome8.detail().contains("PR 8 now holds the merge slot"));
    assert_eq!(*fake.slot.borrow(), Some(8));
    assert_eq!(*fake.release_slot_calls.borrow(), vec![7]);
}

#[test]
fn a_holder_under_a_dispatch_hold_releases_its_slot_to_a_waiting_stale_pr() {
    // A held holder is neither terminal nor red, so only the hold read
    // frees it. The hold answers per PR: if the fake keyed it globally,
    // PR 8 would refuse at the hold gate before reaching the slot logic.
    let fake = Fake {
        slot: RefCell::new(Some(7)),
        facts: Some(PrFacts {
            number: 8,
            head_sha: "eight".to_string(),
            ..open_facts()
        }),
        covered_head: Some("eight".to_string()),
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 4 behind".to_string())),
        ..clean()
    };
    fake.other_facts.borrow_mut().insert(
        7,
        PrFacts {
            number: 7,
            state: "OPEN".to_string(),
            ..open_facts()
        },
    );
    fake.other_checks
        .borrow_mut()
        .insert(7, "green".to_string());
    fake.other_holds.borrow_mut().insert(
        7,
        ProbeOutcome::Refused("dispatch_hold: held by the team for a queued node".to_string()),
    );

    let req8 = Request {
        require_checks: true,
        pr: Some(8),
        ..request(Effect::Merge)
    };
    let outcome8 = run(&fake, &req8);
    assert_eq!(outcome8.word(), "held");
    assert!(outcome8.detail().contains("PR 8 now holds the merge slot"));
    assert_eq!(*fake.slot.borrow(), Some(8));
    assert_eq!(*fake.release_slot_calls.borrow(), vec![7]);
}

#[test]
fn an_inconclusive_hold_read_keeps_the_slot_with_its_holder() {
    // The eviction hold read is fail_open: an unreadable hold must not
    // evict, so the lease stays the bound.
    let fake = Fake {
        slot: RefCell::new(Some(7)),
        facts: Some(PrFacts {
            number: 8,
            head_sha: "eight".to_string(),
            ..open_facts()
        }),
        covered_head: Some("eight".to_string()),
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 4 behind".to_string())),
        ..clean()
    };
    fake.other_facts.borrow_mut().insert(
        7,
        PrFacts {
            number: 7,
            state: "OPEN".to_string(),
            ..open_facts()
        },
    );
    fake.other_checks
        .borrow_mut()
        .insert(7, "green".to_string());
    fake.other_holds.borrow_mut().insert(
        7,
        ProbeOutcome::Inconclusive("hold-check could not run".to_string()),
    );

    let req8 = Request {
        require_checks: true,
        pr: Some(8),
        ..request(Effect::Merge)
    };
    let outcome8 = run(&fake, &req8);
    assert_eq!(outcome8.word(), "held");
    assert!(outcome8.detail().contains("merge_slot_held"));
    assert_eq!(*fake.slot.borrow(), Some(7));
    assert!(fake.release_slot_calls.borrow().is_empty());
}

#[test]
fn an_arm_never_releases_the_slot() {
    // decide() takes the slot only under Effect::Merge, so the release in
    // run() belongs to the same effect: an armed holder keeps the slot
    // until its merge lands, which the eviction then reads as terminal.
    let fake = Fake {
        slot: RefCell::new(Some(7)),
        ..clean()
    };
    let req = Request {
        require_checks: true,
        pr: Some(7),
        ..request(Effect::Arm)
    };
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "armed");
    assert!(fake.release_slot_calls.borrow().is_empty());
    assert_eq!(*fake.slot.borrow(), Some(7));
}

#[test]
fn a_holder_whose_merge_attempt_fails_releases_its_own_slot() {
    // run() releases unconditionally once effect() has run (Merged,
    // Failed, HeadChanged, or Unknown alike): the slot's protective job
    // is done the moment decide() clears, so nothing after that should
    // starve the queue for the rest of the 60m lease. Failed exercises
    // it here; the release call itself no longer branches on outcome.
    let fake = Fake {
        slot: RefCell::new(Some(7)),
        gh_ok: false,
        gh_output: "not mergeable (conflicts or base changed)".to_string(),
        ..clean()
    };
    let req = Request {
        require_checks: true,
        pr: Some(7),
        ..request(Effect::Merge)
    };
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "failed");
    assert_eq!(
        *fake.slot.borrow(),
        None,
        "a durable merge failure must free the slot rather than starve the queue"
    );
}

#[test]
fn slot_holder_error_fails_open_to_the_stale_hold_without_taking_a_slot() {
    // AC4-ERR.
    let fake = Fake {
        slot_holder_err: true,
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 5 behind".to_string())),
        ..clean()
    };
    let req7 = Request {
        require_checks: true,
        pr: Some(7),
        ..request(Effect::Merge)
    };
    let outcome = run(&fake, &req7);
    assert_eq!(outcome.word(), "held");
    let detail = outcome.detail();
    assert!(detail.contains("ci_base_stale"));
    assert!(
        !detail.contains("merge slot"),
        "a fail-open read must not mention the slot"
    );
    assert!(fake.take_slot_calls.borrow().is_empty());
}

#[test]
fn ci_base_verdict_reads_each_branch() {
    // Ancestry is the whole verdict now: behind_by 0 clears, anything
    // behind refuses, because no run at a head that lacks the base tip
    // can have tested the merged tree.
    assert_eq!(ci_base_verdict(0), ProbeOutcome::Clear);
    let outcome = ci_base_verdict(3);
    assert!(matches!(outcome, ProbeOutcome::Refused(reason) if reason.contains("ci_base_stale")));
}

#[test]
fn a_green_pr_whose_ci_predates_main_merges_main_in_and_waits_for_the_retest() {
    // The replay: one PR lands, then a second PR shares no file with it
    // and is green on a run created before that landing. It holds, takes
    // the slot, and has main merged into its branch; once the rerun is
    // green against the current main it merges.
    let mut fake = Fake {
        ci_base: Some(ProbeOutcome::Refused(
            "ci_base_stale: run predates base".to_string(),
        )),
        ..clean()
    };
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "held");
    assert!(outcome.detail().contains("ci_base_stale"));
    assert!(outcome.detail().contains("merged main into PR 7's branch"));
    assert_eq!(*fake.slot.borrow(), Some(7));
    assert_eq!(
        *fake.gh_calls.borrow(),
        vec![vec![
            "api".to_string(),
            "-X".to_string(),
            "PUT".to_string(),
            "repos/{owner}/{repo}/pulls/7/update-branch".to_string(),
            "-f".to_string(),
            "expected_head_sha=abc123".to_string(),
        ]]
    );

    fake.gh_calls.borrow_mut().clear();
    fake.gh_recovery_ok = Some(false);
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "held");
    assert!(outcome.detail().contains("update-branch failed"));
    assert!(outcome
        .detail()
        .contains("merge origin/main into the branch"));

    fake.checks = Some("pending".to_string());
    fake.ci_base = Some(ProbeOutcome::Clear);
    assert_eq!(run(&fake, &req).word(), "held");
    fake.checks = None;
    assert_eq!(run(&fake, &req).word(), "merged");
}

#[test]
fn an_unchecked_merge_skips_ci_base_freshness_and_an_arm_does_not() {
    for (effect, require_checks) in [(Effect::Arm, true), (Effect::Merge, false)] {
        let fake = Fake {
            ci_base: Some(ProbeOutcome::Refused("ci_base_stale: old".to_string())),
            ..clean()
        };
        let mut req = request(effect);
        req.require_checks = require_checks;
        let armed = effect == Effect::Arm;
        assert_eq!(
            run(&fake, &req).word(),
            if armed { "held" } else { "merged" },
            "{effect:?} require_checks={require_checks}"
        );
        assert_eq!(*fake.ci_base_calls.borrow(), u32::from(armed));
        if armed {
            assert!(!fake.gh_calls.borrow().iter().any(|c| c[0] == "pr"));
        }
    }
}

#[test]
fn ci_base_freshness_gates_the_chain() {
    let fake = Fake {
        ci_base: Some(ProbeOutcome::Inconclusive("gh unavailable".to_string())),
        ..clean()
    };
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    assert_eq!(run(&fake, &req).word(), "unknown");
    assert!(fake.gh_calls.borrow().is_empty());
    let fake = Fake {
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: old".to_string())),
        fresh_ci: Some(false),
        ..clean()
    };
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    assert_eq!(run(&fake, &req).word(), "merged");
    assert_eq!(*fake.ci_base_calls.borrow(), 0);
}

#[test]
fn an_unbound_pr_refuses_both_effects_and_calls_no_gh() {
    // The merge the graph cannot see is refused before any hold or check
    // read: retrying without binding changes nothing, so Held would name
    // the wrong remedy.
    for effect in [Effect::Merge, Effect::Arm] {
        let fake = Fake {
            node_binding: Some(ProbeOutcome::Refused(
                "PR 7 is unbound: branch names no node; no node carries this PR; \
                 body carries no closure line. A merge the graph cannot \
                 see is refused. Bind it: pick or file the node (fno backlog idea \
                 \"...\"), run fno do pr closure-trailer <id>, append the printed \
                 line to the PR body, then retry."
                    .to_string(),
            )),
            ..clean()
        };
        let outcome = run(&fake, &request(effect));
        assert_eq!(outcome.word(), "refused", "{effect:?}");
        assert!(outcome.detail().contains("PR 7 is unbound"), "{effect:?}");
        assert!(outcome.detail().contains("closure-trailer"), "{effect:?}");
        assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
    }
}

#[test]
fn an_unreadable_binding_reads_unknown_not_bound() {
    // AC3-ERR: an instrument that cannot answer never becomes a verdict.
    // Bound still flows: every other test runs the default Clear fake,
    // which proves the gate reads the binding and refuses nothing else.
    for effect in [Effect::Merge, Effect::Arm] {
        let fake = Fake {
            node_binding: Some(ProbeOutcome::Inconclusive(
                "graph unreadable; refusing to assume bound".to_string(),
            )),
            ..clean()
        };
        let outcome = run(&fake, &request(effect));
        assert_eq!(outcome.word(), "unknown", "{effect:?}");
        assert!(outcome.detail().contains("refusing to assume bound"));
        assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
    }
}

#[test]
fn a_repo_with_no_node_under_it_is_never_refused() {
    // AC4-HP: the scope. A graph whose nodes all live in other repos has
    // nothing this PR could bind to, so the gate is silent there.
    let entries = vec![json!({"id": "x-bbbb", "cwd": "/other/repo", "project": "other"})];
    let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &open_facts());
    assert_eq!(outcome, ProbeOutcome::Clear);
}

#[test]
fn an_unreadable_body_or_graph_refuses_to_assume_bound() {
    // AC4-ERR: a missing body key is an out-of-date deployed fno, never a
    // bound PR; the remedy names the update.
    let entries = vec![json!({"id": "x-bbbb", "cwd": "/this/repo", "project": "fno"})];
    let facts = PrFacts {
        body: None,
        ..open_facts()
    };
    let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &facts);
    let ProbeOutcome::Inconclusive(reason) = outcome else {
        unreachable!("a missing body is Inconclusive, never a verdict")
    };
    assert!(reason.contains("fno doctor update"));

    // A repo-scoped PR with no binding key at all is the refusal, with
    // the bind remedy.
    let facts = PrFacts {
        head_ref: "docs/team-succeed-faq".to_string(),
        body: Some(String::new()),
        ..open_facts()
    };
    let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &facts);
    let ProbeOutcome::Refused(reason) = outcome else {
        unreachable!("three missing keys refuse")
    };
    assert!(reason.contains("closure-trailer"));
    assert!(reason.contains("no flag bypasses this gate"));

    // No comparable url leaves the backref key unevaluable: Unknown, not
    // a refusal on a PR that may be bound through the back-pointer.
    let facts = PrFacts {
        head_ref: "docs/team-succeed-faq".to_string(),
        url: String::new(),
        body: Some(String::new()),
        ..open_facts()
    };
    let outcome = node_binding_from_entries(Path::new("/this/repo"), &entries, &facts);
    let ProbeOutcome::Inconclusive(reason) = outcome else {
        unreachable!("an unscopeable backref key is Inconclusive")
    };
    assert!(reason.contains("could not be scoped"));

    // AC4-ERR, retarget: the body hands the branch's node to another
    // node, but the graph still points the old node at this PR; the
    // refusal names both fno backlog update commands.
    let url = "https://github.com/o/r/pull/7";
    let body = "Fixes x-bbbb\nRetarget x-aaaa x-bbbb msg-447f8f".to_string();
    let unmoved = vec![
        json!({"id": "x-aaaa", "cwd": "/this/repo", "project": "fno",
               "pr_number": 7, "pr_url": url}),
        json!({"id": "x-bbbb", "cwd": "/this/repo", "project": "fno"}),
    ];
    let facts = PrFacts {
        head_ref: "feature/x-aaaa".to_string(),
        body: Some(body.clone()),
        ..open_facts()
    };
    let outcome = node_binding_from_entries(Path::new("/this/repo"), &unmoved, &facts);
    let ProbeOutcome::Refused(reason) = outcome else {
        unreachable!("an unmoved retarget binding refuses")
    };
    assert!(reason.contains("msg-447f8f"), "{reason}");
    assert!(
        reason.contains("fno backlog update x-aaaa --pr-number null --pr-url null"),
        "{reason}"
    );
    assert!(
        reason.contains("fno backlog update x-bbbb --pr-number 7 --pr-url"),
        "{reason}"
    );

    // AC4-HP: the graph moved (the old node holds no ref, the new one
    // carries this PR), so the probe is Clear.
    let moved = vec![
        json!({"id": "x-aaaa", "cwd": "/this/repo", "project": "fno"}),
        json!({"id": "x-bbbb", "cwd": "/this/repo", "project": "fno",
               "pr_number": 7, "pr_url": url}),
    ];
    let outcome = node_binding_from_entries(Path::new("/this/repo"), &moved, &facts);
    assert_eq!(outcome, ProbeOutcome::Clear);
}

#[test]
fn a_red_merge_result_holds_both_effects_and_calls_no_gh() {
    // 3334b826a133: two green parents merged into a red main. Held, not
    // refused - the remedy is rebase, fix, push, retry.
    for effect in [Effect::Merge, Effect::Arm] {
        let fake = Fake {
            merge_result: Some(ProbeOutcome::Refused(
                "merge-result: REFUSED - cli/src/fno/graph/store.py:525:21: F821 Undefined name `_TAG_SHUTDOWN`"
                    .to_string(),
            )),
            ..clean()
        };
        let outcome = run(&fake, &request(effect));
        assert_eq!(outcome.word(), "held", "{effect:?}");
        assert!(outcome.detail().contains("red merge result"), "{effect:?}");
        assert!(outcome.detail().contains("F821"), "{effect:?}");
        assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
    }
}

#[test]
fn an_inconclusive_merge_result_proceeds_with_a_breadcrumb() {
    // Fail-open, like the lineage probe: a deployed fno lacking the verb
    // or a gh hiccup must not make auto-merge silently never work.
    let fake = Fake {
        merge_result: Some(ProbeOutcome::Inconclusive(
            "exit 4: probe failed".to_string(),
        )),
        ..clean()
    };
    assert_eq!(run(&fake, &request(Effect::Merge)).word(), "merged");
}

#[test]
fn an_unreadable_review_probe_holds_rather_than_assuming_none_runs() {
    let fake = Fake {
        review_hold: Some(ProbeOutcome::Inconclusive(
            "review-activity-unreadable (exit Some(4))".to_string(),
        )),
        ..clean()
    };
    assert_eq!(run(&fake, &request(Effect::Arm)).word(), "held");
    assert!(fake.gh_calls.borrow().is_empty());
}

#[test]
fn a_per_run_refusal_outranks_a_standing_grant_and_names_its_source() {
    // AC1-EDGE.
    let fake = clean();
    let mut req = request(Effect::Merge);
    req.approved = Some(false);
    req.auto_merge_source = Some("flag-no-merge".to_string());
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "refused");
    assert!(
        outcome.detail().contains("flag-no-merge"),
        "{}",
        outcome.detail()
    );
    assert!(fake.gh_calls.borrow().is_empty());
}

#[test]
fn a_disabled_standing_switch_refuses_and_names_the_operator_levers() {
    // AC1-EDGE: the authority evidence is specific, never "not allowed".
    let fake = Fake {
        enabled: false,
        ..clean()
    };
    let outcome = run(&fake, &request(Effect::Arm));
    assert_eq!(outcome.word(), "refused");
    assert!(outcome.detail().contains("auto_merge.enabled"));
    assert!(outcome.detail().contains("TARGET_AUTO_MERGE=1"));
}

#[test]
fn an_env_grant_satisfies_the_arm_without_the_standing_switch() {
    let fake = Fake {
        enabled: false,
        ..clean()
    };
    let mut req = request(Effect::Arm);
    req.auto_merge_source = Some("env-target-auto-merge".to_string());
    assert_eq!(run(&fake, &req).word(), "armed");
}

#[test]
fn an_unknown_authority_read_refuses_rather_than_arming() {
    // A manifest that says nothing falls to the live config, which is off.
    let fake = Fake {
        enabled: false,
        ..clean()
    };
    let mut req = request(Effect::Arm);
    req.approved = None;
    req.auto_merge_source = None;
    assert_eq!(run(&fake, &req).word(), "refused");
}

#[test]
fn the_posture_floor_refuses_both_effects() {
    for effect in [Effect::Merge, Effect::Arm] {
        let fake = Fake {
            floor: Some("review posture no_review is below the automerge floor".to_string()),
            ..clean()
        };
        let outcome = run(&fake, &request(effect));
        assert_eq!(outcome.word(), "refused");
        assert!(fake.gh_calls.borrow().is_empty());
    }
}

#[test]
fn an_unreadable_covered_head_never_produces_an_unpinned_request() {
    // AC2-HP. This is the finalize defect: the old arm dropped
    // --match-head-commit when the journal could not be read.
    for effect in [Effect::Merge, Effect::Arm] {
        let fake = Fake {
            covered_head: None,
            ..clean()
        };
        let outcome = run(&fake, &request(effect));
        assert_eq!(outcome.word(), "unknown", "{effect:?}");
        assert!(outcome.detail().contains("unpinned"));
        assert!(fake.gh_calls.borrow().is_empty(), "{effect:?} ran gh");
    }
}

#[test]
fn a_push_between_validation_and_effect_reads_as_head_changed() {
    // AC2-HP.
    let fake = Fake {
        facts: Some(PrFacts {
            head_sha: "def456".to_string(),
            ..open_facts()
        }),
        ..clean()
    };
    let outcome = run(&fake, &request(Effect::Merge));
    assert_eq!(outcome.word(), "head_changed");
    assert!(fake.gh_calls.borrow().is_empty());
}

#[test]
fn an_arm_receipt_names_the_head_and_is_never_merged() {
    // AC2-EDGE. A queue entry is not a landed merge.
    let journal = crate::merge_provenance::TestJournal::opt_in();
    let fake = clean();
    let outcome = run(&fake, &request(Effect::Arm));
    assert_eq!(
        outcome,
        Outcome::Armed {
            head: "abc123".to_string(),
            merge_grant: None,
        }
    );
    let calls = fake.gh_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(&"--auto".to_string()));
    assert!(calls[0].contains(&"--match-head-commit".to_string()));
    assert!(calls[0].contains(&"abc123".to_string()));
    // An arm records merge_armed, never a landed merge.
    let journal_rows = journal.rows();
    let armed: Vec<_> = journal_rows
        .iter()
        .filter(|r| r["data"]["span_kind"] == "merge_armed")
        .collect();
    assert_eq!(armed.len(), 1, "{journal_rows:?}");
    assert_eq!(armed[0]["data"]["pr"], 7);
    let landed: Vec<_> = journal_rows
        .iter()
        .filter(|r| r["data"]["span_kind"] == "merge_landed")
        .collect();
    assert!(landed.is_empty(), "{journal_rows:?}");
}

#[test]
fn an_immediate_merge_never_passes_auto() {
    let journal = crate::merge_provenance::TestJournal::opt_in();
    let fake = clean();
    let outcome = run(&fake, &request(Effect::Merge));
    assert_eq!(
        outcome,
        Outcome::Merged {
            head: "abc123".to_string(),
            note: None,
            cleanup_failure: None,
            merge_grant: None,
        }
    );
    let calls = fake.gh_calls.borrow();
    assert!(!calls[0].contains(&"--auto".to_string()));
    assert!(calls[0].contains(&"--match-head-commit".to_string()));
    // The owner's record: one merge_landed span for the run, naming the
    // verb lane and the merged PR.
    let spans: Vec<_> = journal
        .rows()
        .into_iter()
        .filter(|r| r["data"]["span_kind"] == "merge_landed")
        .collect();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["data"]["path"], "pr_merge");
    assert_eq!(spans[0]["data"]["pr"], 7);
    assert_eq!(spans[0]["data"]["repo"], "o/r");
    assert_eq!(spans[0]["data"]["head"], "abc123");
}

#[test]
fn an_already_armed_pr_stands_down_on_merge_and_no_ops_on_arm() {
    let armed = PrFacts {
        armed: true,
        ..open_facts()
    };
    let merge_fake = Fake {
        facts: Some(armed.clone()),
        ..clean()
    };
    assert_eq!(run(&merge_fake, &request(Effect::Merge)).word(), "held");
    assert!(merge_fake.gh_calls.borrow().is_empty());

    let arm_fake = Fake {
        facts: Some(armed),
        ..clean()
    };
    assert_eq!(run(&arm_fake, &request(Effect::Arm)).word(), "armed");
    assert!(arm_fake.gh_calls.borrow().is_empty());
}

#[test]
fn a_merged_pr_holds_before_every_other_guard() {
    let fake = Fake {
        facts: Some(PrFacts {
            state: "MERGED".to_string(),
            ..open_facts()
        }),
        enabled: false,
        ..clean()
    };
    let outcome = run(&fake, &request(Effect::Merge));
    assert_eq!(outcome.word(), "held");
    assert!(outcome.detail().contains("already merged"));
}

#[test]
fn an_unreadable_pr_fetch_is_unknown_never_a_verdict() {
    let fake = Fake {
        facts_error: Some("gh api pulls/7 failed".to_string()),
        ..clean()
    };
    assert_eq!(run(&fake, &request(Effect::Arm)).word(), "unknown");
}

#[test]
fn require_checks_holds_on_pending_and_fails_on_red() {
    let mut req = request(Effect::Merge);
    req.require_checks = true;

    let pending = Fake {
        checks: Some("pending".to_string()),
        ..clean()
    };
    assert_eq!(run(&pending, &req).word(), "held");
    assert!(pending.gh_calls.borrow().is_empty());

    let red = Fake {
        checks: Some("red".to_string()),
        ..clean()
    };
    assert_eq!(run(&red, &req).word(), "failed");
    assert!(red.gh_calls.borrow().is_empty());
}

#[test]
fn a_red_verdict_holds_on_the_durable_grant_lane_and_fails_interactive() {
    // Red CI is the state a working session is in while it pushes fixes.
    // Spending a retry on it parked six open PRs in one afternoon.
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    req.authority = Some("durable_grant".to_string());
    let red = Fake {
        checks: Some("red".to_string()),
        ..clean()
    };
    let mut observed_head = None;
    let held = run_observed(&red, &req, &mut observed_head);
    assert_eq!(observed_head.as_deref(), Some("abc123"));
    assert_eq!(held.word(), "held");
    assert!(held
        .detail()
        .contains("the healer or the worker owns the next push"));
    assert!(red.gh_calls.borrow().is_empty());

    // The interactive lane keeps `failed`: the merge_status=failed stamp is
    // the worker's signal, and the existing tests above depend on it.
    let mut interactive = request(Effect::Merge);
    interactive.require_checks = true;
    interactive.authority = Some("manifest".to_string());
    assert_eq!(run(&red, &interactive).word(), "failed");
}

#[test]
fn a_gh_failure_over_a_merge_that_landed_reads_as_merged() {
    // The local post-merge step can fail after the server-side merge landed.
    let fake = Fake {
        gh_ok: false,
        gh_output: "failed to delete local branch".to_string(),
        facts: Some(PrFacts {
            state: "MERGED".to_string(),
            ..open_facts()
        }),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "squash".to_string(),
        merge_grant: None,
    };
    let outcome = effect(&fake, &request(Effect::Merge), &authorized);
    assert_eq!(outcome.word(), "merged");
    // The cleanup failure rides its OWN field. A bare success would lose
    // it, and folding it into `note` would report the merge as partial.
    let Outcome::Merged {
        note,
        cleanup_failure,
        ..
    } = outcome
    else {
        unreachable!()
    };
    assert_eq!(note.as_deref(), Some("merged server-side"));
    assert!(cleanup_failure
        .expect("a cleanup failure")
        .contains("failed to delete local branch"));
}

#[test]
fn a_worktree_recovery_that_worked_carries_no_cleanup_failure() {
    // The regression this pins: the recovery is how the merge landed, not
    // trouble around it. Rendered as a cleanup failure, every worktree-held
    // merge - which is every worktree-first run - reported partial.
    let fake = Fake {
        gh_ok: false,
        gh_output: "fatal: 'x' is already used by worktree at '/w'".to_string(),
        gh_recovery_ok: Some(true),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "merge".to_string(),
        merge_grant: None,
    };
    let outcome = effect(&fake, &request(Effect::Merge), &authorized);
    assert_eq!(
        outcome,
        Outcome::Merged {
            head: "abc123".to_string(),
            note: Some("merged server-side (worktree fallback)".to_string()),
            cleanup_failure: None,
            merge_grant: None,
        }
    );
}

#[test]
fn a_checks_read_that_could_not_run_holds_instead_of_failing() {
    // `fno do pr status` answers `error` when the fetch is rate-limited or
    // the network is down. Failing on it stamps the node merge status
    // failed for a read that never described the checks at all.
    for verdict in ["error", "pending", "unknown"] {
        let fake = Fake {
            checks: Some(verdict.to_string()),
            ..clean()
        };
        let mut req = request(Effect::Merge);
        req.require_checks = true;
        assert_eq!(run(&fake, &req).word(), "held", "verdict {verdict}");
    }
    let red = Fake {
        checks: Some("red".to_string()),
        ..clean()
    };
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    assert_eq!(run(&red, &req).word(), "failed");
}

#[test]
fn a_ruleset_hold_holds() {
    // The door fetched the hold's own name moments before `gh pr merge`
    // and used to spend a failure on it. Held, not Failed: a required
    // check that is merely pending still arrives.
    let fake = Fake {
        checks: Some("green".to_string()),
        github_block: Some("smoke".to_string()),
        ..clean()
    };
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "held");
    assert!(outcome.detail().contains("smoke"));
    assert!(fake.gh_calls.borrow().is_empty());
    // A ruleset hold is not a question about CI greenness. A door gated on
    // the flag would attempt the bypass its own reader just refused.
    let fake = Fake {
        github_block: Some("stacked-base-guard".to_string()),
        ..clean()
    };
    let req = request(Effect::Merge);
    assert_eq!(req.require_checks, false);
    let outcome = run(&fake, &req);
    assert_eq!(outcome.word(), "held");
    assert!(outcome.detail().contains("stacked-base-guard"));
}

#[test]
fn an_arm_effect_proceeds_past_a_ruleset_hold_to_the_queue() {
    // Arm hands the PR to GitHub's own queue, which waits out a missing
    // requirement by design; the hold must not stand in its way.
    let fake = Fake {
        github_block: Some("smoke".to_string()),
        ..clean()
    };
    let mut req = request(Effect::Arm);
    req.decide_only = true;
    assert_eq!(
        run(&fake, &req),
        Outcome::Authorized {
            head: "abc123".to_string()
        }
    );
}

#[test]
fn a_green_read_without_a_github_block_authorizes_as_before() {
    let fake = clean();
    let mut req = request(Effect::Merge);
    req.require_checks = true;
    req.decide_only = true;
    assert_eq!(
        run(&fake, &req),
        Outcome::Authorized {
            head: "abc123".to_string()
        }
    );
}

#[test]
fn an_unparseable_status_read_claims_no_github_block() {
    let read = parse_checks_read(b"error: rate limited");
    assert_eq!(read.verdict, "unknown");
    assert_eq!(read.github_block, None);
}

#[test]
fn a_github_block_with_null_missing_falls_back_to_the_source() {
    // merge_blocker answers `missing_required_checks: null` with a source
    // line saying the block stands, so the reason must carry that line.
    let payload = r#"{"verdict": "green", "ready_blockers": ["github_blocked"], "github_merge_state": {"state": "blocked", "blockers": ["github_blocked"], "missing_required_checks": null, "source": "the rules read failed or names no unsatisfied rule; the block stands"}}"#;
    let read = parse_checks_read(payload.as_bytes());
    assert_eq!(read.verdict, "green");
    assert_eq!(
        read.github_block.as_deref(),
        Some("the rules read failed or names no unsatisfied rule; the block stands")
    );
}

#[test]
fn the_parser_consumes_a_real_status_payload_verbatim() {
    // Captured verbatim from a live `fno do pr status` read, 2026-09-19.
    let payload = r#"{"pr": "2251", "head": "d38a744c36b97c65df67df7f4245f6704adfa494", "verdict": "green", "settled": true, "green": true, "pr_state": "MERGED", "mergeable": "UNKNOWN", "github_merge_state": null, "checks": {"total": 37, "check_runs": 36, "statuses": 1, "fail_check_runs": 0, "fail_statuses": 0, "pass": 37, "fail": 0, "pending": 0, "unsettled": 0, "unsettled_fail": 0}, "optional_reviews": [], "optional_reviews_unresolved": 0, "optional_reviews_resolved_unchanged": 0, "review_coverage": {"coverage": "not_asked", "reviewed_count": 0, "self_attested_count": 0, "head_sha": null, "stale_verdicts": [], "note": "not asked: PR is terminal (merged or closed); this says nothing about coverage at merge time"}, "review_posture": null, "merge_authority": {"auto_merge_enabled": true, "grant": "dispatch", "mergeable_autonomously": true}, "merge_execution": null, "rounds_used": null, "max_rounds": null, "rounds_exhausted": null, "rounds_note": "no review_coverage row at this head; run fno-agents review-coverage", "review_activity": {"blocker": "", "detail": "", "hold": null, "worktree": {"probed": false, "path": null, "dirty": null, "head": null, "note": "not asked: PR is terminal"}}, "dispatch_hold": null, "ready": true, "ready_blockers": []}"#;
    let read = parse_checks_read(payload.as_bytes());
    assert_eq!(read.verdict, "green");
    assert_eq!(read.github_block, None);
}

#[test]
fn a_multibyte_error_line_is_truncated_without_panicking() {
    // A byte slice at 200 panics when the cut lands inside a character.
    let line = "e".repeat(198) + &"é".repeat(20);
    let cut = first_line(&line);
    assert_eq!(cut.chars().count(), 200);
    // The fallback table row: an stderr of only config warnings still
    // reports its first line instead of collapsing to "no error output".
    let out = "fno config: guards.preset is not a modeled config key; ignored\n";
    assert_eq!(
        first_line(out),
        "fno config: guards.preset is not a modeled config key; ignored"
    );
}

#[test]
fn a_secondary_rate_limit_behind_a_config_warning_reads_retryable_not_merge_method() {
    // Specimen 2026-09-29: the fno gh proxy printed its config warning on
    // stderr first, so the real gh error - a secondary rate limit - read
    // as a merge-method fault and the worker burned three tries on it.
    let fake = Fake {
        gh_ok: false,
        gh_output: "fno config: guards.preset is not a modeled config key; ignored\n\
                    gh: You have exceeded a secondary rate limit. Please wait a bit \
                    before you try again."
            .to_string(),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "squash".to_string(),
        merge_grant: None,
    };
    let outcome = effect(&fake, &request(Effect::Merge), &authorized);
    assert_eq!(outcome.word(), "held");
    let detail = outcome.detail();
    assert!(detail.contains("secondary rate limit"), "{detail}");
    assert!(!detail.contains("merge method"), "{detail}");
    assert!(!detail.contains("guards.preset"), "{detail}");
}

#[test]
fn the_failure_reason_names_the_real_gh_error_past_a_config_warning() {
    let fake = Fake {
        gh_ok: false,
        gh_output: "fno config: guards.preset is not a modeled config key; ignored\n\
                    gh: unknown flag: --squash"
            .to_string(),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "squash".to_string(),
        merge_grant: None,
    };
    let outcome = effect(&fake, &request(Effect::Merge), &authorized);
    assert_eq!(outcome.word(), "failed");
    assert!(outcome.detail().contains("unknown flag: --squash"));
    assert!(!outcome.detail().contains("guards.preset"));
}

#[test]
fn a_decide_only_pass_authorizes_without_touching_gh() {
    let fake = clean();
    let mut req = request(Effect::Merge);
    req.decide_only = true;
    assert_eq!(
        run(&fake, &req),
        Outcome::Authorized {
            head: "abc123".to_string()
        }
    );
    assert!(fake.gh_calls.borrow().is_empty());
}

#[test]
fn a_callers_covered_head_outranks_the_journal_read() {
    // The caller's coverage gate and the pin must describe one commit.
    let fake = Fake {
        covered_head: Some("stale999".to_string()),
        ..clean()
    };
    let mut req = request(Effect::Merge);
    req.covered_head = Some("abc123".to_string());
    assert_eq!(run(&fake, &req).word(), "merged");
    assert!(fake.gh_calls.borrow()[0].contains(&"abc123".to_string()));
}

#[test]
fn a_worktree_held_branch_recovers_through_the_rest_endpoint_with_the_pin() {
    let fake = Fake {
        gh_ok: false,
        gh_output: "fatal: 'feature/x' is already used by worktree at /w".to_string(),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "squash".to_string(),
        merge_grant: None,
    };
    // run_gh answers the same failure for both calls, so the recovery here
    // is the argv, not the outcome: the retry must carry sha=<pinned head>.
    let _ = effect(&fake, &request(Effect::Merge), &authorized);
    let calls = fake.gh_calls.borrow();
    assert_eq!(
        calls.len(),
        2,
        "the checkout refusal must retry through REST"
    );
    assert!(
        calls[1].contains(&"sha=abc123".to_string()),
        "{:?}",
        calls[1]
    );
    assert!(calls[1].contains(&"merge_method=squash".to_string()));
}

#[test]
fn an_arm_never_takes_the_worktree_recovery() {
    // A queue arm is not a merge, so a checkout refusal has nothing to
    // recover: PUT .../merge would merge NOW, past the queue's own wait.
    let fake = Fake {
        gh_ok: false,
        gh_output: "fatal: 'feature/x' is already used by worktree at /w".to_string(),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "squash".to_string(),
        merge_grant: None,
    };
    let _ = effect(&fake, &request(Effect::Arm), &authorized);
    assert_eq!(fake.gh_calls.borrow().len(), 1);
}

#[test]
fn a_dead_state_read_after_a_failed_effect_is_unknown() {
    let fake = Fake {
        gh_ok: false,
        gh_output: "boom".to_string(),
        facts_error: Some("gh api pulls/7 failed".to_string()),
        ..clean()
    };
    let authorized = Authorized {
        facts: open_facts(),
        head: "abc123".to_string(),
        strategy: "squash".to_string(),
        merge_grant: None,
    };
    assert_eq!(
        effect(&fake, &request(Effect::Merge), &authorized).word(),
        "unknown"
    );
}

#[test]
fn hold_probe_fails_closed_on_every_non_success() {
    assert_eq!(
        classify_hold_probe(true, b"unheld", b""),
        ProbeOutcome::Clear
    );
    assert_eq!(
        classify_hold_probe(false, b"", b"dispatch-hold:x-owner"),
        ProbeOutcome::Refused("dispatch-hold:x-owner".to_string())
    );
    assert_eq!(
        classify_hold_probe(false, b"", b""),
        ProbeOutcome::Refused(
            "dispatch hold state unreadable; refusing to assume unheld".to_string()
        )
    );
}

#[test]
fn hold_probe_strips_the_move_teaching_line() {
    assert_eq!(
        classify_hold_probe(
            false,
            b"",
            b"fno pr hold-check is now fno do pr hold-check\ndispatch-hold:x-owner\n"
        ),
        ProbeOutcome::Refused("dispatch-hold:x-owner".to_string())
    );
    assert_eq!(
        classify_hold_probe(
            false,
            b"",
            b"fno pr hold-check is now fno do pr hold-check\n"
        ),
        ProbeOutcome::Refused(
            "dispatch hold state unreadable; refusing to assume unheld".to_string()
        )
    );
}

#[test]
fn probe_outcome_projections_state_their_inconclusive_policy() {
    let refused = ProbeOutcome::Refused("stale base".to_string());
    assert_eq!(refused.clone().fail_closed().as_deref(), Some("stale base"));
    assert_eq!(refused.fail_open().as_deref(), Some("stale base"));
    assert_eq!(ProbeOutcome::Clear.fail_closed(), None);
    assert_eq!(ProbeOutcome::Clear.fail_open(), None);
    let inconclusive = ProbeOutcome::Inconclusive("exit 4: unknown".to_string());
    assert_eq!(
        inconclusive.clone().fail_closed().as_deref(),
        Some("exit 4: unknown"),
        "the hold probes refuse on an unevaluated read"
    );
    assert_eq!(
        inconclusive.fail_open(),
        None,
        "the lineage probe proceeds on an unevaluated read"
    );
}

#[test]
fn pr_facts_reads_the_armed_flag_from_the_same_payload_as_the_head() {
    let payload = json!({
        "pr": 7,
        "head_sha": "abc123",
        "head_ref": "feature/x",
        "base_ref": "main",
        "state": "OPEN",
        "auto_merge": {"enabled_by": {"login": "someone"}}
    });
    let facts = parse_pr_facts(&payload).expect("facts parse");
    assert!(facts.armed);
    assert_eq!(facts.head_sha, "abc123");

    let unarmed = json!({"pr": 7, "head_sha": "abc123", "auto_merge": Value::Null});
    assert!(!parse_pr_facts(&unarmed).expect("facts parse").armed);
}

#[test]
fn the_verb_refuses_a_payload_that_names_no_effect() {
    assert!(parse_request(&json!({"cwd": "/tmp"})).is_err());
    assert!(parse_request(&json!({"effect": "arm"})).is_err());
    assert!(parse_request(&json!({"cwd": "/tmp", "effect": "sideways"})).is_err());
    let ok = parse_request(&json!({"cwd": "/tmp", "effect": "arm", "pr": 7}))
        .expect("a well-formed payload parses");
    assert_eq!(ok.effect, Effect::Arm);
    assert_eq!(ok.pr, Some(7));
}

fn preview_request(pr: u64) -> Request {
    Request {
        pr: Some(pr),
        effect: Effect::Preview,
        require_checks: true,
        ..request(Effect::Preview)
    }
}

fn preview_blockers(fake: &Fake, req: &Request) -> Vec<Blocker> {
    let facts = fake.facts.clone().unwrap_or_else(open_facts);
    match preview_walk(fake, req, &facts) {
        PreviewVerdict::Go { .. } => Vec::new(),
        PreviewVerdict::Blocked(rows) => rows,
    }
}

#[test]
fn a_supplied_dispatch_hold_answer_rides_the_preview_without_a_probe() {
    // Held: the supplied reason becomes the blocker, no probe runs.
    let held = Request {
        supplied_dispatch_hold: Some(Some("held by the team".to_string())),
        ..preview_request(8)
    };
    let codes: Vec<String> = preview_blockers(&clean(), &held)
        .into_iter()
        .map(|b| b.code.to_string())
        .collect();
    assert!(
        codes.iter().any(|c| c == "dispatch_hold"),
        "the supplied held answer must block"
    );
    // Clear: no dispatch_hold blocker either way.
    let clear = Request {
        supplied_dispatch_hold: Some(None),
        ..preview_request(8)
    };
    assert!(!preview_blockers(&clean(), &clear)
        .iter()
        .any(|b| b.code == "dispatch_hold"));
}

#[test]
fn a_stale_pr_behind_a_held_slot_previews_both_blockers_without_taking_the_slot() {
    // AC1.
    let fake = Fake {
        ci_base: Some(ProbeOutcome::Refused("ci_base_stale: 3 behind".to_string())),
        slot: RefCell::new(Some(7)),
        other_facts: RefCell::new({
            let mut map = HashMap::new();
            map.insert(
                7,
                PrFacts {
                    number: 7,
                    state: "OPEN".to_string(),
                    ..open_facts()
                },
            );
            map
        }),
        ..clean()
    };
    let req = Request {
        pr: Some(8),
        covered_head: Some("abc123".to_string()),
        ..preview_request(8)
    };
    // The Fake pins covered_head to its own facts head, so pr 8 with the
    // default facts reads a moved head; give pr 8 its own facts.
    let mut fake = fake;
    fake.facts = Some(PrFacts {
        number: 8,
        head_sha: "def456".to_string(),
        ..open_facts()
    });
    fake.covered_head = Some("def456".to_string());
    let blockers = preview_blockers(&fake, &req);
    let codes: Vec<&str> = blockers.iter().map(|b| b.code.as_str()).collect();
    assert!(codes.contains(&"ci_base_stale"), "{codes:?}");
    assert!(codes.contains(&"merge_slot_held"), "{codes:?}");
    let slot_blocker = blockers
        .iter()
        .find(|b| b.code == "merge_slot_held")
        .expect("slot blocker");
    assert!(slot_blocker.detail.contains("PR 7"), "{slot_blocker:?}");
    assert!(fake.take_slot_calls.borrow().is_empty());
    assert!(fake.release_slot_calls.borrow().is_empty());
}

#[test]
fn a_terminal_pr_previews_exactly_pr_terminal() {
    // AC3.
    let fake = Fake {
        facts: Some(PrFacts {
            state: "MERGED".to_string(),
            ..open_facts()
        }),
        ..clean()
    };
    let blockers = preview_blockers(&fake, &preview_request(7));
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert_eq!(blockers[0].code, "pr_terminal");
}

#[test]
fn merge_and_preview_name_the_same_first_blocker_for_every_gate() {
    // AC2: for each gate made to refuse in turn, the Merge outcome's
    // detail and the Preview's first blocker are the SAME answer - one
    // decision, two collectors.
    let base = || Fake { ..clean() };
    let scenarios: Vec<(&str, Fake)> = vec![
        (
            "pr_terminal",
            Fake {
                facts: Some(PrFacts {
                    state: "CLOSED".to_string(),
                    ..open_facts()
                }),
                ..base()
            },
        ),
        (
            "node_unbound",
            Fake {
                node_binding: Some(ProbeOutcome::Refused(
                    "unbound: no node names this PR".to_string(),
                )),
                ..base()
            },
        ),
        (
            "main_red",
            Fake {
                main_ci: Some(Ok(red_main())),
                main_repair: Some(Some("the declared repair lane is x-fix".to_string())),
                ..base()
            },
        ),
        (
            "dispatch_hold",
            Fake {
                dispatch_hold: Some(ProbeOutcome::Refused(
                    "dispatch_hold: held by tgt-x".to_string(),
                )),
                ..base()
            },
        ),
        (
            "review_in_flight",
            Fake {
                review_hold: Some(ProbeOutcome::Refused(
                    "review_in_flight: held by tgt-x at abc123".to_string(),
                )),
                ..base()
            },
        ),
        (
            "red_merge_result",
            Fake {
                merge_result: Some(ProbeOutcome::Refused("F821 in the merged tree".to_string())),
                ..base()
            },
        ),
        (
            "stacked_base",
            Fake {
                lineage: Some(ProbeOutcome::Refused(
                    "base no longer reaches the default branch".to_string(),
                )),
                ..base()
            },
        ),
        (
            "github_blocked",
            Fake {
                github_block: Some("smoke".to_string()),
                ..base()
            },
        ),
        (
            "review_coverage_uncovered",
            Fake {
                coverage_exit: Some(3),
                ..base()
            },
        ),
        (
            "optional_reviews_unresolved",
            Fake {
                optional_unresolved: Some(Some(2)),
                ..base()
            },
        ),
        (
            "ci_pending",
            Fake {
                checks: Some("pending".to_string()),
                ..base()
            },
        ),
    ];
    for (code, fake) in scenarios {
        let req_merge = Request {
            require_checks: true,
            ..request(Effect::Merge)
        };
        let outcome = run(&fake, &req_merge);
        let blockers = preview_blockers(&fake, &preview_request(7));
        assert!(
            !blockers.is_empty(),
            "{code}: preview cleared while merge refused ({outcome:?})"
        );
        let first = blockers[0].code.clone();
        assert_eq!(
            outcome.detail(),
            blockers[0].detail,
            "{code}: merge detail and the preview's first blocker ({first}) disagree"
        );
    }
}

#[test]
fn a_cleared_preview_runs_no_effect_and_takes_no_slot() {
    let fake = Fake {
        fresh_ci: Some(false),
        ..clean()
    };
    let outcome = run(&fake, &preview_request(7));
    assert_eq!(outcome.word(), "authorized");
    assert!(fake.take_slot_calls.borrow().is_empty());
    assert!(fake.release_slot_calls.borrow().is_empty());
    assert!(fake.gh_calls.borrow().is_empty());
}

/// The lead check-in's red token, stubbed: the run name and the sha it
/// failed on. A stub verdict, never a live merge.
fn red_main() -> Value {
    serde_json::json!({
        "verdict": "red",
        "workflow": "cli-ci",
        "sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    })
}

#[test]
fn the_main_red_gate_refuses_red_and_exempts_only_the_declared_repair() {
    // Reproduction of 2026-09-30 (four merges landed on a red main): the
    // merge preview walks a stubbed red main verdict, never a live merge.
    let lane = "no node is declared the repair lane; tag the repair node \
                (fno backlog update <node> --tag main-repair)";
    let refusing = Fake {
        main_ci: Some(Ok(red_main())),
        main_repair: Some(Some(lane.to_string())),
        ..clean()
    };
    let codes: Vec<String> = preview_blockers(&refusing, &preview_request(7))
        .into_iter()
        .map(|b| b.code.to_string())
        .collect();
    assert!(
        codes.contains(&"main_red".to_string()),
        "the preview walked a red main with no main_red blocker: {codes:?}"
    );
    let err = decide(&refusing, &request(Effect::Merge)).unwrap_err();
    assert_eq!(err.word(), "refused", "{}", err.detail());
    let detail = err.detail();
    assert!(detail.contains("cli-ci"), "{detail}");
    assert!(detail.contains("deadbeef"), "{detail}");
    assert!(detail.contains("repair lane"), "{detail}");

    // The exemption: a PR whose node carries the tag is the declared
    // repair, and it lands through the red.
    let repair = Fake {
        main_ci: Some(Ok(red_main())),
        main_repair: Some(None),
        ..clean()
    };
    assert!(decide(&repair, &request(Effect::Merge)).is_ok());

    // A pending main is not red, and an unreadable verdict never
    // manufactures one; the gate names main, not every base.
    for clearing in [
        Fake {
            main_ci: Some(Ok(Value::String("pending".into()))),
            main_repair: Some(Some(lane.to_string())),
            ..clean()
        },
        Fake {
            main_ci: Some(Err("gh api failed: rate limited".to_string())),
            main_repair: Some(Some(lane.to_string())),
            ..clean()
        },
        Fake {
            facts: Some(PrFacts {
                base_ref: "release".to_string(),
                ..open_facts()
            }),
            main_ci: Some(Ok(red_main())),
            ..clean()
        },
    ] {
        assert!(
            decide(&clearing, &request(Effect::Merge)).is_ok(),
            "{}",
            err_words(&clearing)
        );
    }
}

/// The repair-lane sentence never names a done node: its repair landed,
/// so a pointer still naming one is stale and the refusal falls back to
/// the no-lane sentence.
#[test]
fn the_repair_lane_sentence_never_names_a_done_node() {
    let entries = |lane_status: &str| {
        vec![
            serde_json::json!({
                "id": "x-feat", "status": "in_progress", "project": "fno",
                "tags": [],
            }),
            serde_json::json!({
                "id": "x-fix", "status": lane_status, "project": "fno",
                "tags": ["main-repair"],
            }),
        ]
    };
    let facts = PrFacts {
        body: Some("Backlog-Closure: x-feat\n".to_string()),
        ..open_facts()
    };
    let live = main_repair_hold_from_entries(&entries("in_progress"), &facts);
    assert_eq!(live, Some("the declared repair lane is x-fix".to_string()));
    let done = main_repair_hold_from_entries(&entries("done"), &facts);
    assert_eq!(
        done.as_deref(),
        Some("no node is declared the repair lane; tag the repair node: fno backlog update <node> --tag main-repair")
    );
}

/// The refused word of a decide on `fake`, for a failure message.
fn err_words(fake: &Fake) -> String {
    match decide(fake, &request(Effect::Merge)) {
        Ok(_) => "authorized".to_string(),
        Err(outcome) => format!("{}: {}", outcome.word(), outcome.detail()),
    }
}

#[test]
fn the_red_kind_word_splits_by_the_counts() {
    let counts = |uf: i64, f: i64, fs: i64| serde_json::json!({"unsettled_fail": uf, "fail": f, "fail_statuses": fs});
    assert_eq!(
        ci_blocker_word("red", Some(&counts(1, 1, 0))),
        "ci_cancelled_retrigger"
    );
    assert_eq!(
        ci_blocker_word("red", Some(&counts(0, 2, 2))),
        "commit_status_red"
    );
    assert_eq!(ci_blocker_word("red", Some(&counts(1, 3, 1))), "ci_red");
    assert_eq!(ci_blocker_word("red", None), "ci_red");
    assert_eq!(ci_blocker_word("pending", None), "ci_pending");
}

#[test]
fn a_preview_payload_defaults_its_ci_gate_on() {
    let payload: Value =
        serde_json::from_str(r#"{"cwd": "/tmp", "effect": "preview", "pr": 7}"#).unwrap();
    let request = parse_request(&payload).unwrap();
    assert_eq!(request.effect, Effect::Preview);
    assert!(request.require_checks);
    let merge_payload: Value =
        serde_json::from_str(r#"{"cwd": "/tmp", "effect": "merge", "pr": 7}"#).unwrap();
    assert!(!parse_request(&merge_payload).unwrap().require_checks);
}

#[test]
fn live_lanes_counts_live_lane_claims_at_the_canonical_root() {
    // The overlap hold arms on this count; the trait default 0 would
    // leave it dead in production. The claim is planted through the same
    // acquire API the lane runtime uses, in a fresh git repo, so the
    // canonical-root resolution and the claims scan both run for real.
    let base = std::env::temp_dir().join(format!("x53c5-lanes-{}", std::process::id()));
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "git init failed for the lane-count fixture"
    );
    let opts = crate::claims::AcquireOpts {
        pid: Some(std::process::id()),
        ttl_ms: Some(60_000),
        root: Some(repo.clone()),
        ..Default::default()
    };
    let claimed = matches!(
        crate::claims::acquire("lane-slot:0", "parallel-lane:live-lanes-test", opts),
        crate::claims::AcquireOutcome::Acquired(_)
    );
    assert!(claimed, "the planted lane claim must acquire");
    assert_eq!(
        RealProbes.live_lanes(&repo),
        1,
        "the live lane claim must count"
    );
    let _ = crate::claims::release(
        "lane-slot:0",
        "parallel-lane:live-lanes-test",
        Some(repo.as_path()),
        None,
    );
    assert_eq!(
        RealProbes.live_lanes(&repo),
        0,
        "released lane must not count"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn covered_head_reads_a_store_committed_coverage_row() {
    // AC5-HP: a covered row at HEAD committed to the store only.
    let _root = crate::paths::DeclaredRoot::declare("am_covered_head_store");
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output();
    // No git repo in the temp dir: HEAD is empty, so the scan accepts any
    // head. A store-only covered row still answers.
    let _ = head;
    let line = serde_json::json!({
        "ts": "2026-09-17T12:00:00Z", "type": "review_coverage", "source": "target",
        "data": {"pr": 124, "verdicts": [], "head_sha": "aaaaaaaaaa", "coverage": "covered", "reviewed_count": 2}
    })
    .to_string();
    let events = crate::paths::events_path(cwd);
    crate::event_store::append_envelope(&events, &line, None).unwrap();
    let covered = covered_head_from_event(cwd);
    assert_eq!(covered.as_deref(), Some("aaaaaaaaaa"), "{covered:?}");
}
#[test]
fn a_paint_pr_holds_until_an_answered_page_names_it() {
    let tmp = std::env::temp_dir().join(format!("xc129-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".fno")).unwrap();
    // questions_dir falls back to the space dir, and the hermetic guard
    // refuses a HOME-derived one under test; point it at a tempdir. The
    // graph store reads the state root too (the bound-node lookup), so
    // FNO_HOME is declared as well.
    struct RestoreEnv(Option<std::ffi::OsString>, Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
                None => std::env::remove_var("FNO_SPACES_DIR"),
            }
            match self.1.take() {
                Some(v) => std::env::set_var("FNO_HOME", v),
                None => std::env::remove_var("FNO_HOME"),
            }
        }
    }
    let spaces = std::env::temp_dir().join(format!("xc129-spaces-{}", std::process::id()));
    std::fs::create_dir_all(&spaces).unwrap();
    let _env_lock = crate::pr_status::cache_env_lock();
    // The bound-node lookup transitively resolves the global claims root.
    crate::paths::pin_test_claims_root(&spaces);
    let _env = {
        let prior = std::env::var_os("FNO_SPACES_DIR");
        std::env::set_var("FNO_SPACES_DIR", &spaces);
        let prior_home = std::env::var_os("FNO_HOME");
        std::env::set_var("FNO_HOME", &tmp);
        RestoreEnv(prior, prior_home)
    };
    std::fs::write(
        tmp.join(".fno/config.toml"),
        "merge.visual_paint_paths = [\"crates/fno/src/client/**\"]\n",
    )
    .unwrap();
    let mut fake = clean();
    fake.gh_output = "crates/fno/src/client/theme.rs\nREADME.md\n".to_string();
    let head: String = "a".repeat(40);
    let held = crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).expect("held");
    assert_eq!(held.code, "visual_approval");
    assert!(
        held.detail.contains("crates/fno/src/client/theme.rs"),
        "{}",
        held.detail
    );
    // An OPEN page naming the PR never clears the hold.
    let qdir = crate::escalation::questions_dir(&tmp);
    std::fs::create_dir_all(&qdir).unwrap();
    let page = qdir.join("q-xc129test.md");
    std::fs::write(
        &page,
        "---\nquestion_id: q-xc129\nstatus: open\ntitle: May PR 7 merge?\n---\nbody\n",
    )
    .unwrap();
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_some());
    // The page answers, the hold clears.
    std::fs::write(
        &page,
        "---\nquestion_id: q-xc129\nstatus: answered\ntitle: May PR 7 merge?\n---\nbody\n",
    )
    .unwrap();
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_none());
    // The page archives into done/; the approval must not lapse.
    let donedir = qdir.join("done");
    std::fs::create_dir_all(&donedir).unwrap();
    std::fs::rename(&page, donedir.join("q-xc129test.md")).unwrap();
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_none());
    // No paint paths in any candidate config: the gate is disarmed and
    // spends no gh read.
    let bare = std::env::temp_dir().join(format!("xc129-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bare);
    std::fs::create_dir_all(&bare).unwrap();
    let mut off = clean();
    off.gh_output = "crates/fno/src/client/theme.rs\n".to_string();
    assert!(crate::merge_gates::visual_approval_blocker(&off, &bare, 7, &head).is_none());
    assert!(off.gh_calls.borrow().is_empty());
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::remove_dir_all(&bare).ok();
}

#[test]
fn a_crown_chat_approval_clears_the_paint_hold_only_at_its_head() {
    let tmp = std::env::temp_dir().join(format!("xc39f-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".fno")).unwrap();
    struct RestoreEnv(Option<std::ffi::OsString>, Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
                None => std::env::remove_var("FNO_SPACES_DIR"),
            }
            match self.1.take() {
                Some(v) => std::env::set_var("FNO_HOME", v),
                None => std::env::remove_var("FNO_HOME"),
            }
        }
    }
    let spaces = std::env::temp_dir().join(format!("xc39f-spaces-{}", std::process::id()));
    std::fs::create_dir_all(&spaces).unwrap();
    let _env_lock = crate::pr_status::cache_env_lock();
    // The bound-node lookup transitively resolves the global claims root.
    crate::paths::pin_test_claims_root(&spaces);
    let _env = {
        let prior = std::env::var_os("FNO_SPACES_DIR");
        std::env::set_var("FNO_SPACES_DIR", &spaces);
        let prior_home = std::env::var_os("FNO_HOME");
        std::env::set_var("FNO_HOME", &tmp);
        RestoreEnv(prior, prior_home)
    };
    std::fs::write(
        tmp.join(".fno/config.toml"),
        "merge.visual_paint_paths = [\"crates/fno/src/client/**\"]\n",
    )
    .unwrap();
    let head: String = "a".repeat(40);
    let pushed: String = "b".repeat(40);
    let row = |decision: &str, rationale: &str, authority: &str, lifecycle: &str| {
        format!(
            r#"{{"decisions":[{{"decision_id":"d-xc39f","authority_source":"{authority}","lifecycle":"{lifecycle}","decision":"{decision}","rationale":"{rationale}"}}]}}"#
        )
    };
    let mut fake = clean();
    fake.gh_output = "crates/fno/src/client/theme.rs\n".to_string();
    // The lead's transcription, head-scoped: clears.
    fake.decisions_stdout = Some(
        row(
            "Approved: PR 7 at <head> as built",
            "superuser in chat: approved",
            "crown",
            "unscoped",
        )
        .replace("<head>", &head)
        .into_bytes(),
    );
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_none());
    // A head push invalidates the approval: held again.
    assert!(
        crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &pushed).is_some(),
        "an approval recorded for head a..a must not clear head b..b"
    );
    // No chat attestation in the rationale: held.
    fake.decisions_stdout = Some(
        row(
            "Approved: PR 7 at <head> as built",
            "lead judges it good",
            "crown",
            "unscoped",
        )
        .replace("<head>", &head)
        .into_bytes(),
    );
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_some());
    // The row's authority is not crown: held.
    fake.decisions_stdout = Some(
        row(
            "Approved: PR 7 at <head> as built",
            "user in chat: approved",
            "agent",
            "unscoped",
        )
        .replace("<head>", &head)
        .into_bytes(),
    );
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_some());
    // A retracted approval never clears: held.
    fake.decisions_stdout = Some(
        row(
            "Approved: PR 7 at <head> as built",
            "user in chat: approved",
            "crown",
            "retracted",
        )
        .replace("<head>", &head)
        .into_bytes(),
    );
    assert!(crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).is_some());
    // The gate is closed again with the empty index the Fake serves by
    // default; the head-scoped grant text stays out of a held detail.
    fake.decisions_stdout = None;
    let held = crate::merge_gates::visual_approval_blocker(&fake, &tmp, 7, &head).expect("held");
    assert!(
        held.detail.contains("crown-recorded decision"),
        "{}",
        held.detail
    );
    std::fs::remove_dir_all(&tmp).ok();
}
