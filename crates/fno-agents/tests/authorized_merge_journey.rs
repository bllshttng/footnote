//! The queue-arm journey against a fake GitHub, end to end.
//!
//! One PR walks the whole arc a queue-armed merge takes: unarmed, armed by the
//! terminal, pending while checks run, merged later by GitHub itself, then
//! observed again by a repeat reconcile. Nothing here talks to a network; the
//! fake is the only source of PR state, and every decision reads it the same
//! way production does.
//!
//! What it pins is the pair of facts a queue arm makes easy to get wrong: an
//! arm is never a merge, and the arm never happens twice or unpinned.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use fno_agents::authorized_merge::{run, Effect, Outcome, PrFacts, ProbeOutcome, Probes, Request};

const HEAD: &str = "c0ffee1234567890";

/// A GitHub that remembers what was done to it.
#[derive(Default)]
struct FakeGitHub {
    state: RefCell<String>,
    armed: RefCell<bool>,
    /// Set once the queue lands the merge. Until then an armed PR is pending.
    calls: RefCell<Vec<Vec<String>>>,
    head: RefCell<String>,
}

impl FakeGitHub {
    fn open() -> Self {
        FakeGitHub {
            state: RefCell::new("OPEN".to_string()),
            armed: RefCell::new(false),
            calls: RefCell::new(Vec::new()),
            head: RefCell::new(HEAD.to_string()),
        }
    }

    /// GitHub's queue lands the merge, some time after the arm.
    fn queue_merges(&self) {
        assert!(*self.armed.borrow(), "the queue only merges what was armed");
        *self.state.borrow_mut() = "MERGED".to_string();
    }

    fn merge_calls(&self) -> Vec<Vec<String>> {
        self.calls
            .borrow()
            .iter()
            .filter(|args| args.first().map(String::as_str) == Some("pr"))
            .cloned()
            .collect()
    }
}

impl Probes for FakeGitHub {
    fn pr_facts(&self, _cwd: &Path, _pr: Option<u64>) -> Result<PrFacts, String> {
        Ok(PrFacts {
            number: 1042,
            head_sha: self.head.borrow().clone(),
            head_ref: "feature/x-c676".to_string(),
            base_ref: "main".to_string(),
            state: self.state.borrow().clone(),
            armed: *self.armed.borrow(),
        })
    }
    fn dispatch_hold(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
        ProbeOutcome::Clear
    }
    fn review_hold(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
        ProbeOutcome::Clear
    }
    fn base_lineage(&self, _cwd: &Path, _pr: u64) -> ProbeOutcome {
        ProbeOutcome::Clear
    }
    fn checks_verdict(&self, _cwd: &Path, _pr: u64) -> String {
        "green".to_string()
    }
    fn covered_head(&self, _cwd: &Path) -> Option<String> {
        Some(self.head.borrow().clone())
    }
    fn auto_merge_enabled(&self, _cwd: &Path) -> bool {
        true
    }
    fn posture_floor_block(&self, _cwd: &Path) -> Option<String> {
        None
    }
    fn strategy(&self, _cwd: &Path) -> String {
        "squash".to_string()
    }
    fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String), String> {
        self.calls.borrow_mut().push(args.to_vec());
        if args.first().map(String::as_str) == Some("pr") && args.iter().any(|a| a == "--auto") {
            *self.armed.borrow_mut() = true;
        }
        Ok((true, String::new()))
    }
}

fn ask(effect: Effect) -> Request {
    Request {
        cwd: PathBuf::from("/tmp/journey"),
        pr: Some(1042),
        effect,
        approved: Some(true),
        auto_merge_source: Some("config".to_string()),
        require_checks: false,
        covered_head: None,
        decide_only: false,
    }
}

#[test]
fn a_queue_arm_walks_arm_pending_merge_and_reconcile_without_ever_merging_twice() {
    let gh = FakeGitHub::open();

    // 1. The terminal arms. One gh call, pinned to the covered head, and the
    //    receipt says armed - not merged. A caller that read this as a merge
    //    would stop watching a PR that has not landed.
    let armed = run(&gh, &ask(Effect::Arm));
    assert_eq!(
        armed,
        Outcome::Armed {
            head: HEAD.to_string()
        }
    );
    let calls = gh.merge_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(&"--auto".to_string()));
    assert!(calls[0].contains(&"--match-head-commit".to_string()));
    assert!(calls[0].contains(&HEAD.to_string()));
    assert_eq!(gh.state.borrow().as_str(), "OPEN", "an arm is not a merge");

    // 2. Pending. The terminal fires again (a retried stop hook); the PR is
    //    already in the queue, so the receipt is armed and no request is spent.
    let again = run(&gh, &ask(Effect::Arm));
    assert_eq!(
        again,
        Outcome::Armed {
            head: HEAD.to_string()
        }
    );
    assert_eq!(gh.merge_calls().len(), 1, "re-arming spends no request");

    // 3. An explicit merge while the queue owns the PR stands down rather than
    //    racing it.
    let raced = run(&gh, &ask(Effect::Merge));
    assert_eq!(raced.word(), "held");
    assert!(raced.detail().contains("auto-merge queue"));
    assert_eq!(gh.merge_calls().len(), 1);

    // 4. GitHub lands it. Every later ask is a terminal hold, so nothing
    //    re-merges and nothing reads as a fresh merge.
    gh.queue_merges();
    for effect in [Effect::Arm, Effect::Merge] {
        let after = run(&gh, &ask(effect));
        assert_eq!(after.word(), "held", "{effect:?} after the queue merged");
        assert!(after.detail().contains("already merged"));
    }
    assert_eq!(gh.merge_calls().len(), 1, "one arm, one request, ever");
}

#[test]
fn a_push_after_the_arm_is_never_re_armed_on_the_new_head() {
    let gh = FakeGitHub::open();
    assert_eq!(run(&gh, &ask(Effect::Arm)).word(), "armed");

    // Someone pushes. The covered head this fake reports moves with the PR, so
    // the arm the queue holds now describes an older commit. The owner must not
    // quietly re-arm the new head: nothing has reviewed it.
    *gh.head.borrow_mut() = "deadbeefdeadbeef".to_string();
    let mut request = ask(Effect::Arm);
    request.covered_head = Some(HEAD.to_string());
    let outcome = run(&gh, &request);
    assert_eq!(
        outcome,
        Outcome::HeadChanged {
            expected: HEAD.to_string(),
            actual: "deadbeefdeadbeef".to_string(),
        }
    );
    assert_eq!(gh.merge_calls().len(), 1);
}
