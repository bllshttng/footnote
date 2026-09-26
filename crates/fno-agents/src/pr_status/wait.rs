//! The wait loop (`fno do pr wait`): the one sanctioned watcher. Ported
//! from `fno.pr._wait` on top of the status cache: every tick coalesces, a
//! backoff window serves the row row degraded and the loop rides it out, and
//! the exit prints the gh spend, so a caller re-arms on truth.

use crate::pr_status_facts::{GhProbe, RealGhProbe};
use serde_json::Value;
use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(crate) const MIN_INTERVAL: f64 = 5.0;

fn duration_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^(\d+(?:\.\d+)?)([smh])?$").unwrap())
}

/// `30m` / `90s` / `1.5h` / plain seconds -> seconds; 0 on garbage.
pub(crate) fn parse_duration(text: &str) -> f64 {
    let Some(c) = duration_re().captures(text.trim()) else {
        return 0.0;
    };
    let n: f64 = c[1].parse().unwrap_or(0.0);
    let mult = match c.get(2).map(|m| m.as_str().to_lowercase()).as_deref() {
        Some("m") => 60.0,
        Some("h") => 3600.0,
        _ => 1.0,
    };
    n * mult
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The note that separates this verb from the hand-rolled loops it
/// replaces: the spender sees its spend at exit.
fn spend_note(calls: usize, ticks: usize, noun: &str) -> String {
    let snap = crate::gh_budget::snapshot(&crate::gh_budget::ledger_path(), now_ms());
    format!(
        "note: {calls} gh call(s) over {ticks} {noun} read(s) this invocation, \
         fleet budget {} of {} points in the last 60s\n",
        snap.points_60s, snap.cap
    )
}

/// One cache-coalesced tick's re-emit: the payload JSON on stdout, the
/// verdict and serve lines on stderr, then the spend note. Only the FINAL
/// tick's output is emitted (`_wait._emit`).
fn emit_tick(payload: &Value, lines: &[String], calls: usize, ticks: usize) -> (String, String) {
    let stdout = format!("{}\n", serde_json::to_string(payload).unwrap_or_default());
    let mut stderr = String::new();
    if !lines.is_empty() {
        stderr.push_str(&lines.join("\n"));
        stderr.push('\n');
    }
    stderr.push_str(&spend_note(calls, ticks, "status"));
    (stdout, stderr)
}

/// The settled/green loop. `poll` is one cache-coalesced tick returning
/// (code, payload, stderr lines, this tick's gh calls); `sleeper` and `now`
/// are injectable so tests stage the clock. Only the final tick re-emits.
pub(crate) fn wait_status(
    pr: u64,
    until: &str,
    timeout: f64,
    interval: f64,
    mut poll: impl FnMut() -> (i32, Value, Vec<String>, usize),
    mut sleeper: impl FnMut(f64),
    now: impl Fn() -> f64,
) -> (i32, String, String) {
    let interval = interval.max(MIN_INTERVAL);
    let deadline = now() + timeout.max(0.0);
    let mut ticks = 0usize;
    let mut calls = 0usize;
    loop {
        ticks += 1;
        let (rc, payload, lines, tick_calls) = poll();
        calls += tick_calls;
        // The payload already names the conflict. A degraded backoff serve
        // (`stale_reason`) cannot prove the head is still the conflicting
        // one, so it rides out; a non-open PR is not a rebase errand.
        let conflicting = payload.get("mergeable").and_then(Value::as_str) == Some("CONFLICTING")
            && payload.get("pr_state").and_then(Value::as_str) == Some("OPEN")
            && payload.get("stale_reason").is_none();
        if conflicting {
            let head = payload
                .get("head")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(8)
                .collect::<String>();
            let (stdout, stderr) = emit_tick(&payload, &lines, calls, ticks);
            let stderr = format!(
                "{stderr}wait: PR {pr} is CONFLICTING at {head}. Rebase onto the \
                 base (`fno do pr rebase`), push, then re-arm the wait.\n"
            );
            return (5, stdout, stderr);
        }
        let done = match until {
            "green" => payload.get("green").and_then(Value::as_bool),
            _ => payload.get("settled").and_then(Value::as_bool),
        };
        if done == Some(true) {
            let (stdout, stderr) = emit_tick(&payload, &lines, calls, ticks);
            return (rc, stdout, stderr);
        }
        if now() + interval > deadline {
            let (stdout, stderr) = emit_tick(&payload, &lines, calls, ticks);
            let verdict = payload
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("None");
            let stderr = format!(
                "{stderr}wait: still not {until} after {}s; last verdict {verdict}. \
                 Re-arm the wait or read the PR.\n",
                timeout as i64
            );
            return (if rc == 0 { 2 } else { rc }, stdout, stderr);
        }
        sleeper(interval);
    }
}

/// The review loop: wake when a NEW review posts past the first-read
/// baseline. An unreadable tick is no-answer, never "no new review", so an
/// API blip cannot fake a wake.
pub(crate) fn wait_review(
    pr: u64,
    timeout: f64,
    interval: f64,
    mut counter: impl FnMut() -> Option<i64>,
    mut sleeper: impl FnMut(f64),
    now: impl Fn() -> f64,
    calls: impl Fn() -> usize,
) -> (i32, String, String) {
    let interval = interval.max(MIN_INTERVAL);
    let deadline = now() + timeout.max(0.0);
    let mut baseline: Option<i64> = None;
    let mut last: Option<i64> = None;
    let mut ticks = 0usize;
    loop {
        ticks += 1;
        if let Some(count) = counter() {
            last = Some(count);
            if baseline.is_none() {
                baseline = Some(count);
            } else if count > baseline.unwrap_or(0) {
                let b = baseline.unwrap_or(0);
                let stderr = format!(
                    "wait: {} new review(s) on PR {pr} ({b} -> {count}).\n{}",
                    count - b,
                    spend_note(calls(), ticks, "review")
                );
                return (0, String::new(), stderr);
            }
        }
        if now() + interval > deadline {
            let observed = match last {
                None => "no read succeeded".to_string(),
                Some(n) => format!("last count {n}"),
            };
            let stderr = format!(
                "wait: still no new review after {}s; {observed}. \
                 Re-arm the wait or read the PR.\n",
                timeout as i64,
            );
            return (2, String::new(), stderr);
        }
        sleeper(interval);
    }
}

/// The `status-wait` door op: `{cwd, pr, until, timeout, interval}`.
/// Durations parse from the CLI's raw strings (`30m`, `60`).
pub(crate) fn run_wait(payload: &Value) -> (i32, String, String) {
    let cwd_str = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
    let pr = payload.get("pr").and_then(Value::as_u64).unwrap_or(0);
    let field = |key: &str, default: f64| -> f64 {
        match payload.get(key) {
            Some(Value::String(s)) => parse_duration(s),
            Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
            _ => default,
        }
    };
    let timeout = field("timeout", 1800.0);
    let interval = field("interval", 60.0);
    if timeout <= 0.0 || interval <= 0.0 {
        return (
            2,
            String::new(),
            "fno do pr wait: --timeout/--interval must parse (30m, 90s, 1h)\n".into(),
        );
    }
    let until = payload
        .get("until")
        .and_then(Value::as_str)
        .unwrap_or("settled");
    if !matches!(until, "settled" | "green" | "review") {
        return (
            2,
            String::new(),
            "fno do pr wait: --until must be one of settled/green/review\n".into(),
        );
    }
    let cwd = Path::new(cwd_str);
    let sleeper = |s: f64| std::thread::sleep(std::time::Duration::from_secs_f64(s));
    let start = Instant::now();
    let clock = move || start.elapsed().as_secs_f64();
    if until == "review" {
        // The slug resolves once, not per tick; an unresolvable slug retries
        // per tick, so a transient failure never wedges the wait.
        let slug = super::cache::git_slug(cwd);
        let probe = crate::pr_status::cache::CountingProbe {
            inner: RealGhProbe,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let counter = || {
            let slug = match &slug {
                Some(s) => s.clone(),
                None => match super::cache::git_slug(cwd) {
                    Some(s) => s,
                    None => return None,
                },
            };
            review_count(&probe, cwd, &slug, pr)
        };
        let calls = || probe.calls.load(Ordering::SeqCst);
        return wait_review(pr, timeout, interval, counter, sleeper, clock, calls);
    }
    let mut poll = || {
        let (code, payload, lines, tick_calls) = super::cache::cached_status(cwd_str, pr, false);
        (code, payload, lines, tick_calls)
    };
    wait_status(pr, until, timeout, interval, &mut poll, sleeper, clock)
}

/// Reviews on `pr` via one REST read; None when the read fails (no-answer,
/// never "no new review"). per_page=100 is load-bearing: gh api fetches ONE
/// page, so at the default 30 the count saturates and a 31st review can
/// never wake the wait.
fn review_count<P: GhProbe>(probe: &P, cwd: &Path, slug: &str, pr: u64) -> Option<i64> {
    let args = vec![
        "api".to_string(),
        format!("repos/{slug}/pulls/{pr}/reviews?per_page=100"),
        "--jq".to_string(),
        "length".to_string(),
    ];
    let (ok, stdout, _) = probe.run_gh(cwd, &args).ok()?;
    if !ok {
        return None;
    }
    stdout.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations_parse_like_the_python_leg() {
        assert_eq!(parse_duration("30m"), 1800.0);
        assert_eq!(parse_duration("90s"), 90.0);
        assert_eq!(parse_duration("1.5h"), 5400.0);
        assert_eq!(parse_duration("45"), 45.0);
        assert_eq!(parse_duration("garbage"), 0.0);
    }

    fn staged_poll<'a>(
        ticks: Vec<(i32, Value)>,
    ) -> impl FnMut() -> (i32, Value, Vec<String>, usize) + 'a {
        let mut it = ticks.into_iter();
        move || {
            let (code, payload) = it.next().unwrap_or((2, json!({"verdict": "pending"})));
            (code, payload, Vec::new(), 1)
        }
    }

    #[test]
    fn settles_on_the_third_tick_and_prints_the_spend() {
        let payloads = vec![
            (2, json!({"verdict": "pending", "settled": false})),
            (2, json!({"verdict": "pending", "settled": false})),
            (
                0,
                json!({"verdict": "green", "settled": true, "green": true}),
            ),
        ];
        let mut slept = 0.0;
        let (code, stdout, stderr) = wait_status(
            9,
            "settled",
            1800.0,
            60.0,
            staged_poll(payloads),
            |s| slept += s,
            || 0.0,
        );
        assert_eq!(code, 0, "the settled verdict's code");
        assert!(stdout.contains("\"settled\":true"), "{stdout}");
        assert!(
            stderr.contains("over 3 status read(s) this invocation"),
            "{stderr}"
        );
        assert_eq!(slept, 120.0, "two 60s sleeps between three ticks");
    }

    #[test]
    fn a_conflicting_pr_exits_five_with_the_rebase_receipt() {
        let payloads = vec![(
            2,
            json!({
                "verdict": "pending",
                "settled": false,
                "mergeable": "CONFLICTING",
                "pr_state": "OPEN",
                "head": "abcdef1234567890",
            }),
        )];
        let (code, _stdout, stderr) = wait_status(
            9,
            "settled",
            1800.0,
            60.0,
            staged_poll(payloads),
            |_| {},
            || 0.0,
        );
        assert_eq!(code, 5, "the rebase receipt exit");
        assert!(stderr.contains("is CONFLICTING at abcdef12"), "{stderr}");
        assert!(stderr.contains("Rebase onto the base"), "{stderr}");
    }

    #[test]
    fn a_timeout_exits_with_the_last_code_and_the_still_note() {
        let t = Cell::new(0.0);
        let payloads: Vec<(i32, Value)> = (0..100)
            .map(|_| (1, json!({"verdict": "red", "settled": false})))
            .collect();
        let (code, _stdout, stderr) = wait_status(
            9,
            "settled",
            30.0,
            10.0,
            staged_poll(payloads),
            |s| t.set(t.get() + s),
            || t.get(),
        );
        assert_eq!(code, 1, "the LAST observed code");
        assert!(
            stderr.contains("still not settled after 30s; last verdict red"),
            "{stderr}"
        );
    }

    #[test]
    fn a_new_review_wakes_the_review_loop() {
        let counts = std::rc::Rc::new(std::cell::RefCell::new(vec![2i64, 1, 1]));
        let c = counts.clone();
        let mut counter = move || c.borrow_mut().pop();
        let t = Cell::new(0.0);
        let (code, stdout, stderr) = wait_review(
            9,
            1800.0,
            60.0,
            &mut counter,
            |s| t.set(t.get() + s),
            || t.get(),
            || 3,
        );
        assert_eq!(code, 0, "a grown count wakes the wait");
        assert!(stdout.is_empty(), "no stdout on the review arm");
        assert!(
            stderr.contains("1 new review(s) on PR 9 (1 -> 2)"),
            "{stderr}"
        );
        assert!(stderr.contains("over 3 review read(s)"), "{stderr}");
    }

    #[test]
    fn an_unreadable_tick_is_a_rideout_never_a_wake() {
        let counts = std::rc::Rc::new(std::cell::RefCell::new(vec![
            None,
            Some(3i64),
            None,
            Some(2i64),
        ]));
        let c = counts.clone();
        let mut counter = move || c.borrow_mut().pop().and_then(std::convert::identity);
        let t = Cell::new(0.0);
        let (code, _stdout, stderr) = wait_review(
            9,
            1800.0,
            60.0,
            &mut counter,
            |s| t.set(t.get() + s),
            || t.get(),
            || 4,
        );
        assert_eq!(code, 0, "None rides out; 3 > baseline 2 wakes");
        assert!(stderr.contains("(2 -> 3)"), "{stderr}");
    }

    #[test]
    fn a_review_timeout_reports_the_last_observed_count() {
        let counts = std::rc::Rc::new(std::cell::RefCell::new(vec![Some(4i64)]));
        let c = counts.clone();
        let mut counter = move || c.borrow_mut().pop().and_then(std::convert::identity);
        let t = Cell::new(0.0);
        let (code, _stdout, stderr) = wait_review(
            9,
            60.0,
            30.0,
            &mut counter,
            |s| t.set(t.get() + s),
            || t.get(),
            || 2,
        );
        assert_eq!(code, 2, "the bound fired with no new review");
        assert!(
            stderr.contains("still no new review after 60s; last count 4"),
            "{stderr}"
        );
    }
}
