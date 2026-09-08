//! Acceptance evidence: the outcome-probe substrate, and the bindings that
//! connect a plan's compiled acceptance criteria to it.
//!
//! ONE runner (`run_probe`), ONE parser (`parse_probes_for`), ONE verb
//! (`probe-run`) serve both gates: `done_probes` (session termination, via
//! `evaluate_done_probes`) and `close_probes` (node closure, shelled by the
//! close verbs). The `acceptance_evidence` frontmatter binds a compiled AC id
//! to a probe index, so a criterion's satisfaction is MEASURED by a fresh run
//! with its own positive marker, never witnessed (x-d098).
//!
//! Terminal scoping: a binding names its list, and each terminal validates and
//! evaluates only its own scope. `done_probes` evidence stops a session;
//! `close_probes` evidence closes a node. A session may stop with close
//! evidence still pending; node closure holds until its own probes pass.
//! Compiling the criteria themselves stays in `fno.plan.criteria` (Python) -
//! this module validates structure and evaluates outcomes, never prose.

use serde_json::Value;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;

use crate::bounded_spawn::{kill_process_group, killpg};

// ── done_probes ──────────────────────────────────────────────────────
//
// A plan may declare `done_probes` in its frontmatter: runnable commands whose
// success is the operational evidence that the shipped thing actually RUNS.
// DonePRGreen measures artifacts (PR + CI + review), which operational silence
// cannot falsify - grooming shipped three times without ever running. Probes are
// the enforcement arm: the gate refuses done until the declared observation
// holds, forcing the session to perform the last mile before claiming done.

/// Wall-clock ceiling per probe. The bound is native (spawn, poll `try_wait`,
/// kill): the plugin must run on hosts where `timeout(1)` and `gtimeout` are
/// absent, so no bound may depend on either binary existing.
pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A probe list is a gate, not a test suite.
const PROBE_CAP: usize = 3;

/// Probe stderr is quoted back in the block reason so the agent knows which
/// last-mile action to perform; cap it so the reason stays readable.
const PROBE_STDERR_CAP: usize = 500;

/// Probe stdout is the run's positive marker and travels on the probe row;
/// cap it so one row cannot blow the event's data-size budget. The cap keeps
/// the END of the output (a verdict line is a tail), and the row's truncation
/// marker names what was cut.
const PROBE_OUTPUT_CAP: usize = 2000;

enum ProbeOutcome {
    /// Exit 0 AND captured stdout: the run produced its own positive marker.
    /// Output is the evidence; exit status alone proves a shell ran.
    Pass { stdout: String },
    /// Exit 0 with nothing on stdout: no marker, so no verdict on the change.
    /// A silent probe is exactly the vacuous pass (`test -f residue`) the
    /// probe contract records as a trap; SKIP holds the gate without claiming
    /// the probe failed.
    Skip,
    Fail {
        code: Option<i32>,
        stderr: String,
        stdout: String,
    },
    /// The RUNNER could not answer. Distinct from FAIL on purpose - the
    /// probe never executed, so its subject is untested, not failing. `kind`
    /// keeps the three causes distinguishable in the event map: a real
    /// timeout renders as the legacy `timeout` token, spawn and wait
    /// failures render as `blocked:<kind>` instead of being absorbed into it.
    Blocked { kind: &'static str, why: String },
}

impl ProbeOutcome {
    /// Event rendering: `pass` | `fail:<code>` | `timeout` | `blocked:<kind>`
    /// | `skip`. The legacy vocabulary the scoreboard folds join on;
    /// `verdict` is the four-state contract prove-it reads.
    fn render(&self) -> String {
        match self {
            ProbeOutcome::Pass { .. } => "pass".to_string(),
            ProbeOutcome::Fail { code: Some(c), .. } => format!("fail:{c}"),
            ProbeOutcome::Fail { code: None, .. } => "fail:signal".to_string(),
            ProbeOutcome::Skip => "skip".to_string(),
            ProbeOutcome::Blocked {
                kind: "timeout", ..
            } => "timeout".to_string(),
            ProbeOutcome::Blocked { kind, .. } => format!("blocked:{kind}"),
        }
    }

    /// PASS satisfies, FAIL blocks, SKIP and BLOCKED carry NO verdict (the
    /// UNANSWERED shape: they hold the gate without naming a failure).
    fn verdict(&self) -> &'static str {
        match self {
            ProbeOutcome::Pass { .. } => "PASS",
            ProbeOutcome::Skip => "SKIP",
            ProbeOutcome::Fail { .. } => "FAIL",
            ProbeOutcome::Blocked { .. } => "BLOCKED",
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProbeGate {
    /// No declaration: zero subprocesses, gate behavior byte-identical to before.
    Absent,
    Pass(Value),
    Fail {
        reason: String,
        results: Value,
    },
}

/// Unwrap a YAML scalar to the string a YAML parser would produce.
///
/// Decoding escapes is not cosmetic: the recommended block form routinely
/// carries an inner quote (`- "test -n \"$(cmd)\""`). Leaving the backslashes in
/// would hand `sh -c` literal `\"` characters - a DIFFERENT command than the
/// plan declared, whose result the gate would then trust - and would also key
/// the event by a string the PyYAML-side grader never matches.
fn unquote_scalar(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                // `\"`, `\\`, `\/` and anything else: keep the escaped char.
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        return out;
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        // YAML single-quoted scalars escape only the quote, by doubling it.
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

/// Split a YAML inline list body. Quoted segments win over comma-splitting
/// because probe commands routinely contain commas (`--jq '.a,.b'`); only an
/// unquoted body falls back to a naive split.
fn split_inline_list(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            let mut item = String::new();
            let mut escaped = false;
            for c2 in chars.by_ref() {
                if escaped {
                    item.push(c2);
                    escaped = false;
                } else if c2 == '\\' {
                    escaped = true;
                } else if c2 == c {
                    break;
                } else {
                    item.push(c2);
                }
            }
            out.push(item);
        }
    }
    if out.is_empty() {
        out = body
            .split(',')
            .map(unquote_scalar)
            .filter(|s| !s.is_empty())
            .collect();
    }
    out
}

/// What a plan doc's frontmatter says about `done_probes`.
#[derive(Debug, PartialEq)]
enum ProbeDecl {
    /// No `done_probes` key, or explicitly `[]` - both mean "no gate".
    None,
    Probes(Vec<String>),
    /// The key is present but no probes could be recovered from it. This is
    /// NEVER treated as "no probes": a declaration this parser cannot read is
    /// the vacuous-pass shape the whole feature exists to prevent, so it fails
    /// closed and asks a human to look.
    Unparseable,
}

/// Read `done_probes` from a plan doc's frontmatter. Accepts the block form
/// (`done_probes:\n  - "cmd"`) and the single-line inline form
/// (`done_probes: ["cmd"]`); anything else declared is `Unparseable`.
fn parse_done_probes(content: &str) -> ProbeDecl {
    parse_probes_for(content, "done_probes")
}

/// Key-parameterized probe-list parser. `done_probes` (loop-check's
/// session-termination gate) and `close_probes` (the close verbs' node-closure
/// gate) share one parser so the two gates cannot drift on what counts as a
/// declared probe list. Anything else declared under `key` is `Unparseable`.
fn parse_probes_for(content: &str, key: &str) -> ProbeDecl {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return ProbeDecl::None;
    }
    let after_first = &content[3..];
    let Some(end) = after_first.find("\n---") else {
        return ProbeDecl::None;
    };

    let key_prefix = format!("{key}:");
    let mut out = Vec::new();
    let mut declared = false;
    let mut in_block = false;
    for line in after_first[..end].lines() {
        let trimmed = line.trim();
        if !in_block {
            let Some(rest) = trimmed.strip_prefix(&key_prefix) else {
                continue;
            };
            declared = true;
            let rest = rest.trim();
            if rest == "[]" {
                return ProbeDecl::None;
            }
            if let Some(inner) = rest.strip_prefix('[') {
                // strip_suffix, not trim_end_matches: the latter eats EVERY
                // trailing ']' (mangling a command that ends in one) and would
                // silently accept an unterminated list.
                let Some(inner) = inner.strip_suffix(']') else {
                    return ProbeDecl::Unparseable;
                };
                let items = split_inline_list(inner);
                // An empty result means a multi-line inline list (items live on
                // following lines) - unrecoverable here, so refuse rather than
                // report the declaration as absent.
                return if items.is_empty() {
                    ProbeDecl::Unparseable
                } else {
                    ProbeDecl::Probes(items)
                };
            }
            // A plain scalar (`close_probes: cmd`) is ONE probe. The plan
            // schema advertises `str | list`, so refusing the scalar here made
            // a documented-legal declaration an unevaluable gate (a hard
            // refusal at the close verbs). A YAML block scalar (`|` / `>`) puts
            // the value on the following lines and is still unreadable here.
            if !rest.is_empty() {
                if rest.starts_with('|') || rest.starts_with('>') {
                    return ProbeDecl::Unparseable;
                }
                let item = unquote_scalar(rest);
                return if item.is_empty() {
                    ProbeDecl::Unparseable
                } else {
                    ProbeDecl::Probes(vec![item])
                };
            }
            in_block = true;
            continue;
        }
        // Inside the block: a comment is not the end of it (treating one as a
        // terminator would silently drop every probe below it).
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(item) = trimmed.strip_prefix("- ") else {
            break; // the next frontmatter key ends the block
        };
        let item = unquote_scalar(item);
        if !item.is_empty() {
            out.push(item);
        }
    }

    match (declared, out.is_empty()) {
        (false, _) => ProbeDecl::None,
        (true, true) => ProbeDecl::Unparseable,
        (true, false) => ProbeDecl::Probes(out),
    }
}

/// Keep at most the LAST `cap` bytes, without splitting a UTF-8 character.
///
/// The tail, not the head: a failing command's real error is almost always its
/// last line, so keeping the prefix would routinely drop the one diagnostic the
/// block reason exists to surface. Char-boundary aware because `String::drain`
/// and `truncate` panic mid-character, and probe stderr regularly carries
/// arrows, box-drawing, and accented words.
fn keep_last_on_char_boundary(s: &mut String, cap: usize) -> Option<usize> {
    if s.len() <= cap {
        return None;
    }
    let total = s.len();
    let start = s.len() - cap;
    let cut = (start..=s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    s.drain(..cut);
    Some(total)
}

/// Run one probe under a native timeout.
///
/// Two things here are load-bearing rather than defensive. stderr is drained by
/// a reader thread because reading a piped stderr only after exit deadlocks any
/// probe that writes past the pipe buffer. And the child leads its own process
/// group, which is killed on EVERY exit path - not just the timeout.
///
/// The group kill has to cover normal exit too, because `sh` is not the only
/// process holding the stderr write end. A pipeline (`... | grep -q x`) forks,
/// and a probe that backgrounds anything (`sleep 3600 &`, or any command that
/// daemonizes) lets `sh` exit IMMEDIATELY while the descendant keeps the pipe
/// open. `try_wait` then reports success and leaves the timeout loop, so the
/// timer is never consulted again and the drain join blocks for the
/// descendant's whole lifetime - wedging the stop hook well past the 60s the
/// gate promises. Killing the group closes the pipe and bounds the join.
fn run_probe(cmd: &str, cwd: &Path, timeout: std::time::Duration) -> ProbeOutcome {
    // The pipefail preamble closes the `| tail -5` trap at the runner: a
    // pipeline whose real command fails can no longer read as pass through
    // the truncating tail's exit 0. Bash-only: on Linux /bin/sh is commonly
    // dash, and dash treats an illegal option on the special builtin `set`
    // as an ABORT with status 2 regardless of the `2>/dev/null` redirect (a
    // special builtin's own errors are not an ordinary command failure a
    // redirect can swallow), so the preamble would kill every probe before
    // its command ran. A no-bash host falls back to plain sh with NO
    // preamble, losing only the pipeline-trap closure, not the probe.
    let bash_wrapped = format!("set -o pipefail 2>/dev/null; {cmd}");
    // Through `spawn_bounded` for the same reason `run_bounded` goes through
    // it: a done_probe is the strongest rung loop-check has, and a fork that
    // answers EAGAIN under load would otherwise return Blocked and hold the
    // gate on a probe that was never run. NotFound still returns on attempt
    // one, so the sh fallback below fires as promptly as it did before.
    let spawned = {
        let build = |shell: &str, script: &str| {
            crate::bounded_spawn::spawn_bounded(OsStr::new(shell), &["-c", script], cwd)
        };
        match build("bash", &bash_wrapped) {
            Ok(child) => Ok(child),
            Err(std::io::ErrorKind::NotFound) => build("sh", cmd),
            Err(kind) => Err(kind),
        }
    };

    let mut child = match spawned {
        Ok(c) => c,
        Err(kind) => {
            return ProbeOutcome::Blocked {
                kind: "spawn",
                why: format!("probe spawn failed: {kind:?}"),
            }
        }
    };

    // Capture the pgid before any wait() could reap the leader.
    let pgid = child.id() as i32;

    let mut out_pipe = child.stdout.take();
    let out_drain = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(ref mut p) = out_pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let mut err_pipe = child.stderr.take();
    let err_drain = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(ref mut p) = err_pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Exit 0 alone is not a PASS: the run must also have produced
                // its own output, the positive marker. A silent success is
                // SKIP - held, never counted.
                break if status.success() {
                    ProbeOutcome::Pass {
                        stdout: String::new(),
                    }
                } else {
                    ProbeOutcome::Fail {
                        code: status.code(),
                        stderr: String::new(),
                        stdout: String::new(),
                    }
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_process_group(&mut child);
                    break ProbeOutcome::Blocked {
                        kind: "timeout",
                        why: format!("timed out after {}s (killed)", timeout.as_secs()),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                kill_process_group(&mut child);
                break ProbeOutcome::Blocked {
                    kind: "wait",
                    why: format!("probe wait failed: {e}"),
                };
            }
        }
    };

    // Reap any descendant still holding a pipe's write end, so the drains see
    // EOF. Without this a backgrounding probe blocks the join indefinitely even
    // though the shell itself exited cleanly.
    killpg(pgid);

    // On a runner failure the output tail is worthless (the reason names the
    // block) and joining risks the very hang we just escaped if anything
    // outlived the group kill. Drop the handles instead: the threads end when
    // the pipes close.
    if matches!(outcome, ProbeOutcome::Blocked { .. }) {
        return outcome;
    }

    let mut stdout = out_drain.join().unwrap_or_default();
    let stdout_total = keep_last_on_char_boundary(&mut stdout, PROBE_OUTPUT_CAP);
    if let Some(total) = stdout_total {
        // The truncation marker travels IN the captured text: a reader of the
        // row must be able to tell cut evidence from naturally short output,
        // which the cap's docstring promises.
        stdout.insert_str(
            0,
            &format!("[fno probe: truncated, last {PROBE_OUTPUT_CAP} of {total} bytes] "),
        );
    }
    let mut stderr = err_drain.join().unwrap_or_default();
    let _stderr_total = keep_last_on_char_boundary(&mut stderr, PROBE_STDERR_CAP);
    match outcome {
        ProbeOutcome::Pass { .. } if stdout.trim().is_empty() => ProbeOutcome::Skip,
        ProbeOutcome::Pass { .. } => ProbeOutcome::Pass { stdout },
        ProbeOutcome::Fail { code, .. } => ProbeOutcome::Fail {
            code,
            stderr,
            stdout,
        },
        other => other,
    }
}
/// Event payload for a refusal where probes were DECLARED but none ran (plan
/// unreadable, unparseable, over cap). It must be a non-empty object: recording
/// a bare null would make the refusal invisible to `prior_fires_declared_probes`,
/// so a plan that tripped the cap and then went missing would silently degrade
/// to "no gate" - the exact fail-open this records history to prevent. The key
/// is underscore-prefixed so it cannot collide with a probe command string.
fn undeterminable_marker(cause: &str) -> Value {
    serde_json::json!({ "_undeterminable": cause })
}

/// True when any prior loop_check fire for this session recorded probe results.
/// Used to fail closed on an unreadable plan only when probes are known to have
/// existed - a probe-less session with a stale plan_path keeps today's behavior.
fn prior_fires_declared_probes(events_path: &Path, session_id: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(events_path) else {
        return false;
    };
    content.lines().any(|line| {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        val.get("type").and_then(|v| v.as_str()) == Some("loop_check")
            && val.pointer("/data/session_id").and_then(|v| v.as_str()) == Some(session_id)
            && val
                .pointer("/data/done_probes")
                .and_then(|v| v.as_object())
                .is_some_and(|m| !m.is_empty())
    })
}

/// Resolve the PLAN source to its probe list and acceptance-evidence
/// declaration, or the gate that must block.
///
/// Split out from `evaluate_done_probes` so the project source can be resolved
/// independently: a plan that declares nothing (or whose doc is missing on a
/// probe-less session) must still let the project's own probes run, which a
/// single early-return-Absent path cannot express.
fn plan_declared_probes(
    plan_path: Option<&str>,
    cwd: &Path,
    events_path: &Path,
    session_id: &str,
) -> Result<(Vec<String>, EvidenceDecl), ProbeGate> {
    // Resolve a relative plan_path against the session's cwd, not the process
    // cwd: plan_path is repo-relative in practice, and reading nothing here
    // would degrade to Absent - a silent gate bypass.
    let plan = plan_path.and_then(|p| {
        // A plan_path may carry a `#wave-1`-style fragment; the Python plan
        // readers strip it, and reading the literal name would fail, which on
        // the first fire (no probe history) degrades to Absent - a silent
        // bypass of a gate the plan actually declared.
        let p = Path::new(p.split('#').next().unwrap_or(p));
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        std::fs::read_to_string(abs).ok()
    });
    let Some(plan) = plan else {
        // Fail closed only when probes were observed before; otherwise a stale
        // plan_path on a probe-less session must not start refusing done.
        if prior_fires_declared_probes(events_path, session_id) {
            return Err(ProbeGate::Fail {
                reason: format!(
                    "done_probes undeterminable: plan {} is unreadable but a prior fire declared probes; restore the plan doc",
                    plan_path.unwrap_or("(unset)")
                ),
                results: undeterminable_marker("plan-unreadable"),
            });
        }
        return Ok((Vec::new(), EvidenceDecl::None));
    };

    let decl = match parse_acceptance_evidence(&plan) {
        EvidenceDecl::Unparseable => {
            return Err(ProbeGate::Fail {
                reason: "acceptance_evidence undeterminable: the plan declares the field but it could not be parsed (use `required:` plus a `bindings:` map of AC id to `done_probes[n]`/`close_probes[n]`)".to_string(),
                results: undeterminable_marker("unparseable-acceptance-evidence"),
            })
        }
        other => other,
    };
    let probes = match parse_done_probes(&plan) {
        ProbeDecl::None => return Ok((Vec::new(), decl)),
        ProbeDecl::Unparseable => {
            return Err(ProbeGate::Fail {
                reason: format!(
                    "done_probes undeterminable: plan {} declares the field but no probe could be read from it (use a block list, or a single-line inline list)",
                    plan_path.unwrap_or("(unset)")
                ),
                results: undeterminable_marker("unparseable-declaration"),
            })
        }
        ProbeDecl::Probes(p) => p,
    };
    if probes.len() > PROBE_CAP {
        return Err(ProbeGate::Fail {
            reason: format!(
                "plan declares {} done_probes; the cap is {PROBE_CAP} per source (a probe list is a gate, not a test suite)",
                probes.len()
            ),
            results: undeterminable_marker("over-cap"),
        });
    }
    Ok((probes, decl))
}

/// Evaluate the probe conjunct across BOTH sources. Called ONLY once every
/// other DonePRGreen conjunct already holds, so probes run at most once per
/// would-be-done fire.
///
/// `config_probes` is the repo-wide `done_probes` off config.toml. Both lists
/// run and both must pass: a plan can ADD guardrails and can never silence the
/// project's, including via an explicit `done_probes: []`. A repo-wide guard a
/// plan doc can switch off is a guard on one of two paths, which is decorative.
pub(crate) fn evaluate_done_probes(
    plan_path: Option<&str>,
    config_probes: Option<&Result<Vec<String>, String>>,
    cwd: &Path,
    events_path: &Path,
    session_id: &str,
    timeout: std::time::Duration,
) -> ProbeGate {
    // The project source resolves first: a declaration this parser cannot read
    // must block before anything runs, in the same vocabulary the plan side
    // uses. A config key that degrades to no-gate is a guardrail that
    // disappears when you typo it.
    let project = match config_probes {
        None => Vec::new(),
        Some(Err(why)) => {
            return ProbeGate::Fail {
                reason: format!(
                    "done_probes undeterminable: config.toml declares `done_probes` but {why}"
                ),
                results: undeterminable_marker("unparseable-config-declaration"),
            }
        }
        Some(Ok(p)) => p.clone(),
    };
    // PROBE_CAP applies PER SOURCE, not to the union. The cap encodes
    // per-declaration discipline ("a gate, not a test suite"); one shared
    // budget would instead make two independent authors compete for one number
    // and let a project's policy eat a plan's operational probes.
    if project.len() > PROBE_CAP {
        return ProbeGate::Fail {
            reason: format!(
                "config.toml declares {} done_probes; the cap is {PROBE_CAP} per source (a probe list is a gate, not a test suite)",
                project.len()
            ),
            results: undeterminable_marker("over-cap"),
        };
    }

    let (plan_probes, decl) = match plan_declared_probes(plan_path, cwd, events_path, session_id) {
        Ok((p, d)) => (p, d),
        Err(gate) => return gate,
    };

    // Bindings and requiredness: the session terminal evaluates ONLY the
    // done_probes scope (a close_probes binding is Pending here - session
    // evidence may pass while close evidence is still owed), and it validates
    // only its own indices. The close terminal validates its own in
    // `decide_probe_run`.
    let (required, bindings) = match &decl {
        EvidenceDecl::None => (false, &[][..]),
        EvidenceDecl::Unparseable => unreachable!("plan_declared_probes refuses it"),
        EvidenceDecl::Evidence { required, bindings } => {
            // Only THIS terminal's bindings are validated here, against this
            // terminal's list: a close_probes index says nothing about the
            // done_probes list length, and vice versa.
            let own: Vec<AcceptanceBinding> = bindings
                .iter()
                .filter(|b| b.key == "done_probes")
                .cloned()
                .collect();
            if let Err(why) = validate_bindings(&own, plan_probes.len()) {
                return ProbeGate::Fail {
                    reason: why,
                    results: undeterminable_marker("invalid-acceptance-evidence"),
                };
            }
            (*required, bindings.as_slice())
        }
    };

    if project.is_empty() && plan_probes.is_empty() {
        // A required plan cannot pass on unarmed evidence: declaring `required`
        // with no done_probes binding means the session terminal has no rung to
        // stand on, so close-scope-only bindings do not arm this terminal.
        // Finalize refuses the declaration too; this is the runtime backstop.
        if required && !bindings.iter().any(|b| b.key == "done_probes") {
            return ProbeGate::Fail {
                reason: "acceptance_evidence: the plan asserts runnable acceptance evidence is required, but no done_probes probe is bound to a criterion (unarmed)"
                    .to_string(),
                results: undeterminable_marker("unarmed-required"),
            };
        }
        return ProbeGate::Absent;
    }

    let mut results = serde_json::Map::new();
    let mut failures = Vec::new();
    let mut plan_outcomes: Vec<(String, String)> = Vec::new();
    for (source, cmd) in project
        .iter()
        .map(|c| ("project", c))
        .chain(plan_probes.iter().map(|c| ("plan", c)))
    {
        let outcome = run_probe(cmd, cwd, timeout);
        // Keyed by the BARE command: cli/src/fno/scoreboard/fold.py joins this
        // map back to the plan's frontmatter by exact command string, so the
        // source label belongs in the failure reason - which is what the
        // operator reads to know which file to edit - and never in the key.
        results.insert(cmd.clone(), Value::String(outcome.render()));
        if source == "plan" {
            plan_outcomes.push((cmd.clone(), outcome.render()));
        }
        match &outcome {
            ProbeOutcome::Pass { .. } => {}
            ProbeOutcome::Skip => failures.push(format!(
                "{source} probe `{cmd}` exited 0 with no output - SKIP, not a pass (a probe must emit its own positive marker)"
            )),
            ProbeOutcome::Blocked { why, .. } => {
                failures.push(format!("{source} probe `{cmd}` BLOCKED: {why}"))
            }
            ProbeOutcome::Fail { code, stderr, .. } => {
                let code = code.map(|c| c.to_string()).unwrap_or("signal".to_string());
                let tail = if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                };
                failures.push(format!("{source} probe `{cmd}` exited {code}{tail}"));
            }
        }
    }

    // Criterion-level coverage: which bound criterion each probe answered.
    // The rows travel IN the results map under an underscore key, which the
    // scoreboard fold never joins on (it looks up exact probe commands), so
    // the map keeps its cmd->render shape and the record rides along.
    let coverage = acceptance_coverage(&bindings, "done_probes", &plan_outcomes);
    if !coverage.is_empty() {
        results.insert(
            "_acceptance_coverage".to_string(),
            Value::Array(coverage.clone()),
        );
        for row in &coverage {
            if row["status"] != "satisfied" {
                failures.push(format!(
                    "criterion {} bound to {} is not satisfied ({})",
                    row["ac"].as_str().unwrap_or("?"),
                    row["probe"].as_str().unwrap_or("?"),
                    row["status"].as_str().unwrap_or("?"),
                ));
            }
        }
    }
    if required && !coverage.iter().any(|r| r["status"] == "satisfied") {
        failures.push(
            "acceptance_evidence: the plan asserts runnable acceptance evidence is required, but no done_probes probe satisfied a bound criterion"
                .to_string(),
        );
    }

    let results = Value::Object(results);
    if failures.is_empty() {
        ProbeGate::Pass(results)
    } else {
        ProbeGate::Fail {
            reason: format!(
                "done_probes failed - the shipped thing has no evidence of running: {}",
                failures.join("; ")
            ),
            results,
        }
    }
}

// ── acceptance evidence bindings (x-d098) ────────────────────────────
//
// `acceptance_evidence` frontmatter: a compiled AC id wired to one probe by
// index, so the loop gates report criterion-level coverage instead of leaving
// acceptance as prose a reader is trusted to have honored.

/// One binding: a compiled acceptance criterion wired to one probe by index.
/// The terminal is the list the probe lives in; a binding's index is validated
/// against that list at the terminal that evaluates it, never across both.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AcceptanceBinding {
    pub(crate) ac: String,
    pub(crate) key: &'static str,
    pub(crate) index: usize,
}

/// What a plan doc's frontmatter says about `acceptance_evidence`.
#[derive(Debug, PartialEq)]
pub(crate) enum EvidenceDecl {
    /// No key: legacy plan, unarmed by absence, byte-identical behavior.
    None,
    Evidence {
        required: bool,
        bindings: Vec<AcceptanceBinding>,
    },
    /// The key is present but no declaration could be read from it. NEVER
    /// treated as `None`: a declaration this parser cannot read is the
    /// vacuous-pass shape, so it fails closed like an unparseable probe list.
    Unparseable,
}

/// Probe-reference grammar: `<list>[<index>]` against `done_probes` or
/// `close_probes`. Anything else refuses; prose is never executed.
fn parse_probe_ref(s: &str) -> Option<(&'static str, usize)> {
    let (key, idx) = s.split_once('[')?;
    let idx = idx.strip_suffix(']')?;
    let key = match key {
        "done_probes" => "done_probes",
        "close_probes" => "close_probes",
        _ => return None,
    };
    idx.parse::<usize>().ok().map(|n| (key, n))
}

/// Read `acceptance_evidence` from a plan doc's frontmatter. Accepted shape:
///
/// ```yaml
/// acceptance_evidence:
///   required: true
///   bindings:
///     AC1-HP: done_probes[0]
///     AC2-HP: close_probes[0]
/// ```
///
/// `acceptance_evidence: {}` (or `[]`) is an explicit unarmed declaration.
/// An unknown sub-key, a malformed reference, or a duplicated criterion is
/// `Unparseable`, never a guess.
pub(crate) fn parse_acceptance_evidence(content: &str) -> EvidenceDecl {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return EvidenceDecl::None;
    }
    let after_first = &content[3..];
    let Some(end) = after_first.find("\n---") else {
        return EvidenceDecl::None;
    };

    let mut required: Option<bool> = None;
    let mut bindings: Vec<AcceptanceBinding> = Vec::new();
    let mut declared = false;
    let mut in_bindings = false;
    for line in after_first[..end].lines() {
        let trimmed = line.trim();
        if !declared {
            let Some(rest) = trimmed.strip_prefix("acceptance_evidence:") else {
                continue;
            };
            declared = true;
            match rest.trim() {
                "" => {}
                "{}" | "[]" => {
                    return EvidenceDecl::Evidence {
                        required: false,
                        bindings: vec![],
                    }
                }
                _ => return EvidenceDecl::Unparseable,
            }
            continue;
        }
        // Inside the block: a zero-indent non-empty line is the next
        // frontmatter key and ends the declaration.
        if !trimmed.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if in_bindings {
            let Some((ac, probe_ref)) = trimmed.split_once(':') else {
                return EvidenceDecl::Unparseable;
            };
            let ac = ac.trim();
            let Some((key, index)) = parse_probe_ref(probe_ref.trim()) else {
                return EvidenceDecl::Unparseable;
            };
            if ac.is_empty() || bindings.iter().any(|b| b.ac == ac) {
                return EvidenceDecl::Unparseable;
            }
            bindings.push(AcceptanceBinding {
                ac: ac.to_string(),
                key,
                index,
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("bindings:") {
            match rest.trim() {
                "" => in_bindings = true,
                "{}" | "[]" => {}
                _ => return EvidenceDecl::Unparseable,
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("required:") {
            if required.is_some() {
                return EvidenceDecl::Unparseable;
            }
            match rest.trim() {
                "true" => required = Some(true),
                "false" => required = Some(false),
                _ => return EvidenceDecl::Unparseable,
            }
            continue;
        }
        return EvidenceDecl::Unparseable;
    }

    if !declared {
        return EvidenceDecl::None;
    }
    EvidenceDecl::Evidence {
        required: required.unwrap_or(false),
        bindings,
    }
}

/// Structural validation against the plan's own probe list for ONE terminal:
/// a bound index must exist (the "missing probe" refusal), and one criterion
/// must not claim both terminals. AC ids are checked against the compiled
/// criteria at finalize (`fno.plan.execution_validation`), which owns the one
/// compiler; this check is the runtime backstop for structure only.
pub(crate) fn validate_bindings(
    bindings: &[AcceptanceBinding],
    own_count: usize,
) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for b in bindings {
        if b.index >= own_count {
            return Err(format!(
                "acceptance_evidence binds {} to {}[{}], but {} declares {own_count} probe(s): the bound probe is missing",
                b.ac, b.key, b.index, b.key
            ));
        }
        if seen.contains(&b.ac.as_str()) {
            return Err(format!(
                "acceptance_evidence binds {} more than once: one criterion, one terminal",
                b.ac
            ));
        }
        seen.push(&b.ac);
    }
    Ok(())
}

/// Coverage rows for ONE terminal's bindings, given that terminal's outcomes
/// as (cmd, render) in declared order. `pass` is the only satisfied verdict;
/// a FAIL names the criterion; SKIP/BLOCKED stay Unknown (the run answered
/// nothing); a binding on the OTHER list is Pending here - session evidence
/// may pass while close evidence is still owed.
pub(crate) fn acceptance_coverage(
    bindings: &[AcceptanceBinding],
    key: &str,
    outcomes: &[(String, String)],
) -> Vec<Value> {
    bindings
        .iter()
        .filter(|b| b.key == key)
        .filter_map(|b| {
            let (cmd, render) = outcomes.get(b.index)?;
            let status = if render == "pass" {
                "satisfied"
            } else if render.starts_with("fail") {
                "failed"
            } else if render == "skip" || render == "timeout" || render.starts_with("blocked") {
                "unknown"
            } else {
                return None;
            };
            Some(serde_json::json!({
                "ac": b.ac,
                "probe": format!("{}[{}]", b.key, b.index),
                "cmd": cmd,
                "status": status,
                "terminal": if key == "done_probes" { "session" } else { "close" },
            }))
        })
        .collect()
}

/// `fno-agents probe-run --plan <path> --key <name> --cwd <root> --json`.
///
/// Evaluates one named probe list (`done_probes` or `close_probes`) from a plan
/// doc and exits 0 when every probe passes. The close verbs shell out to this
/// for `close_probes`, mirroring how `active_backlog` shells out to
/// `fno backlog done`: the node-closure gate and the session-termination gate
/// share ONE runner. The process-group kill, the pipe-buffer drain, and
/// 127-as-failure all live in `run_probe` and are NOT reimplemented here.
///
/// Exit contract: 0 = every probe passed (or none declared under `key`);
/// 1 = at least one probe failed; 2 = undeterminable (plan unreadable,
/// declaration unparseable, over cap). stdout is always one JSON object.
pub fn run_probe_run(args: &[String]) -> i32 {
    let (code, json) = decide_probe_run(args);
    println!("{json}");
    code
}

fn decide_probe_run(args: &[String]) -> (i32, String) {
    let mut plan: Option<String> = None;
    let mut key = String::from("done_probes");
    let mut cwd: Option<String> = None;
    let mut want_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plan" => {
                i += 1;
                if i < args.len() {
                    plan = Some(args[i].clone());
                }
            }
            "--key" => {
                i += 1;
                if i < args.len() {
                    key = args[i].clone();
                }
            }
            "--cwd" => {
                i += 1;
                if i < args.len() {
                    cwd = Some(args[i].clone());
                }
            }
            "--json" => want_json = true,
            _ => {}
        }
        i += 1;
    }
    // JSON is always emitted; the flag is accepted so callers stay explicit.
    let _ = want_json;

    let Some(plan_raw) = plan else {
        return probe_run_payload(2, &key, false, vec![], "--plan is required", Vec::new());
    };
    // A plan_path may carry a `#wave-1` fragment; reading the literal name
    // would fail, degrading an asserted gate to absent.
    let plan_clean = plan_raw.split('#').next().unwrap_or(&plan_raw);
    let plan_path = std::path::Path::new(plan_clean);
    let abs = if plan_path.is_absolute() {
        plan_path.to_path_buf()
    } else {
        std::path::Path::new(cwd.as_deref().unwrap_or(".")).join(plan_path)
    };
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) => {
            return probe_run_payload(
                2,
                &key,
                false,
                vec![],
                &format!("plan {plan_clean} unreadable: {e}"),
                Vec::new(),
            )
        }
    };

    let probes = match parse_probes_for(&content, &key) {
        ProbeDecl::None => {
            // A required plan with no probe list under `key` is unarmed at
            // this terminal and must not read as a pass. A plan with no
            // declaration at all keeps today's behavior byte for byte.
            if let EvidenceDecl::Evidence { required: true, .. } =
                parse_acceptance_evidence(&content)
            {
                return probe_run_payload(
                    1,
                    &key,
                    false,
                    vec![],
                    "acceptance_evidence: the plan asserts runnable acceptance evidence is required, but no probe under this key is bound to a criterion (unarmed)",
                    Vec::new(),
                );
            }
            return probe_run_payload(0, &key, false, vec![], "no probes declared", Vec::new());
        }
        ProbeDecl::Unparseable => {
            return probe_run_payload(
                2,
                &key,
                true,
                vec![],
                &format!("`{key}` declared but no probe could be read from it"),
                Vec::new(),
            )
        }
        ProbeDecl::Probes(p) => p,
    };
    if probes.len() > PROBE_CAP {
        return probe_run_payload(
            2,
            &key,
            true,
            vec![],
            &format!("{} probes declared; cap is {PROBE_CAP}", probes.len()),
            Vec::new(),
        );
    }

    // The terminal validates only its own scope: a binding's index is checked
    // against the list this terminal evaluates. The other terminal validates
    // its own when it runs.
    let decl = match parse_acceptance_evidence(&content) {
        EvidenceDecl::Unparseable => {
            return probe_run_payload(
                2,
                &key,
                true,
                vec![],
                "acceptance_evidence declared but it could not be parsed (use `required:` plus a `bindings:` map of AC id to `done_probes[n]`/`close_probes[n]`)",
                Vec::new(),
            )
        }
        EvidenceDecl::None => None,
        EvidenceDecl::Evidence { required, bindings } => {
            // Same per-terminal rule as the session gate: this terminal's
            // bindings against this terminal's list only.
            let own: Vec<AcceptanceBinding> = bindings
                .iter()
                .filter(|b| b.key == key)
                .cloned()
                .collect();
            if let Err(why) = validate_bindings(&own, probes.len()) {
                return probe_run_payload(2, &key, true, vec![], &why, Vec::new());
            }
            Some((required, own))
        }
    };
    let (required, bindings) = match &decl {
        Some((required, bindings)) => (*required, bindings.as_slice()),
        None => (false, &[][..]),
    };

    let work_dir = std::path::Path::new(cwd.as_deref().unwrap_or("."));
    let mut results: Vec<Value> = Vec::new();
    let mut outcomes: Vec<(String, String)> = Vec::new();
    let mut failed_reason: Option<String> = None;
    for cmd in &probes {
        // A probe may carry its claim as a trailing ` # <claim>` comment; the
        // claim travels on the row so a reader sees what the run answered,
        // not just what it ran. Bare commands keep today's shape.
        let claim = cmd.find(" # ").map(|idx| cmd[idx + 3..].trim().to_string());
        let outcome = run_probe(cmd, work_dir, PROBE_TIMEOUT);
        outcomes.push((cmd.clone(), outcome.render()));
        // One source for the row's verdict vocabulary, borrowed before the
        // consuming match moves the outcome's fields.
        let verdict = outcome.verdict();
        let entry = match outcome {
            ProbeOutcome::Pass { stdout } => serde_json::json!({
                "cmd": cmd,
                "verdict": verdict,
                "claim": claim,
                "stdout": stdout,
            }),
            ProbeOutcome::Skip => {
                if failed_reason.is_none() {
                    failed_reason = Some(format!(
                        "`{cmd}` exited 0 with no output - SKIP, not a pass"
                    ));
                }
                serde_json::json!({
                    "cmd": cmd,
                    "verdict": verdict,
                    "claim": claim,
                    "stdout": "",
                })
            }
            ProbeOutcome::Blocked { why, .. } => {
                if failed_reason.is_none() {
                    failed_reason = Some(format!("`{cmd}` BLOCKED: {why}"));
                }
                serde_json::json!({
                    "cmd": cmd,
                    "verdict": verdict,
                    "claim": claim,
                    "why": why,
                })
            }
            ProbeOutcome::Fail {
                code,
                stderr,
                stdout,
            } => {
                let c = code
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                if failed_reason.is_none() {
                    failed_reason = Some(format!("`{cmd}` exited {c}"));
                }
                serde_json::json!({
                    "cmd": cmd,
                    "verdict": verdict,
                    "claim": claim,
                    "code": code,
                    "stdout": stdout,
                    "stderr": stderr,
                })
            }
        };
        results.push(entry);
    }

    // Criterion-level coverage for this terminal's bindings, and the
    // required-unarmed refusal: a required plan whose armed criteria all
    // answered nothing cannot read as a pass.
    let coverage = acceptance_coverage(bindings, &key, &outcomes);
    for row in &coverage {
        if row["status"] != "satisfied" {
            let probe_detail = failed_reason.take();
            failed_reason = Some(format!(
                "criterion {} bound to {} is not satisfied ({}): {}",
                row["ac"].as_str().unwrap_or("?"),
                row["probe"].as_str().unwrap_or("?"),
                row["status"].as_str().unwrap_or("?"),
                probe_detail.unwrap_or_else(|| "the probe did not pass".to_string()),
            ));
            break;
        }
    }
    if required && !coverage.iter().any(|r| r["status"] == "satisfied") {
        if failed_reason.is_none() {
            failed_reason = Some(
                "acceptance_evidence: the plan asserts runnable acceptance evidence is required, but no probe under this key satisfied a bound criterion"
                    .to_string(),
            );
        }
    }

    let (code, reason) = match failed_reason {
        Some(r) => (1, r),
        None => (0, "every probe passed".to_string()),
    };
    probe_run_payload(code, &key, true, results, &reason, coverage)
}

fn probe_run_payload(
    code: i32,
    key: &str,
    declared: bool,
    results: Vec<Value>,
    reason: &str,
    coverage: Vec<Value>,
) -> (i32, String) {
    let payload = serde_json::json!({
        "key": key,
        "declared": declared,
        "passed": code == 0,
        "results": results,
        "acceptance_coverage": coverage,
        "reason": reason,
    });
    (
        code,
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()),
    )
}
#[cfg(test)]
mod done_probe_tests {
    use super::*;
    use std::time::Duration;
    // Config-root parsing and the gh-quota classifier stayed with the session
    // side in loopcheck; these tests cover them through the same facade.
    use crate::loopcheck::{
        coverage_adapter, is_graphql_read, parse_settings, value_as_probe_list,
    };

    fn fm(body: &str) -> String {
        format!("---\ntitle: t\n{body}\n---\n\n# doc\n")
    }

    fn probes_of(doc: &str) -> Vec<String> {
        match parse_done_probes(doc) {
            ProbeDecl::Probes(p) => p,
            other => panic!("expected probes, got {other:?}"),
        }
    }

    #[test]
    fn parses_block_list() {
        let doc = fm("done_probes:\n  - \"fno agents mail list --since 24h | grep -q groom\"\n  - 'echo ok'\nstatus: ready");
        assert_eq!(
            probes_of(&doc),
            vec![
                "fno agents mail list --since 24h | grep -q groom".to_string(),
                "echo ok".to_string()
            ]
        );
    }

    #[test]
    fn parses_inline_list_keeping_commas_inside_commands() {
        let doc = fm(r#"done_probes: ["gh api x --jq '.a,.b'", "echo ok"]"#);
        assert_eq!(
            probes_of(&doc),
            vec!["gh api x --jq '.a,.b'".to_string(), "echo ok".to_string()],
            "a comma inside a quoted command must not split it into two probes"
        );
    }

    #[test]
    fn absent_field_and_explicit_empty_list_are_both_no_gate() {
        assert_eq!(parse_done_probes(&fm("done_probes: []")), ProbeDecl::None);
        assert_eq!(parse_done_probes(&fm("status: ready")), ProbeDecl::None);
        assert_eq!(parse_done_probes("no frontmatter here"), ProbeDecl::None);
    }

    #[test]
    fn a_plain_scalar_is_one_probe_but_a_block_scalar_refuses() {
        // The plan schema advertises `str | list` for close_probes/done_probes,
        // so a scalar must EVALUATE, not refuse - refusing turned a legal
        // declaration into an unevaluable gate at the close verbs.
        assert_eq!(
            parse_done_probes(&fm("done_probes: \"echo ok\"")),
            ProbeDecl::Probes(vec!["echo ok".to_string()])
        );
        // A YAML block scalar's value lives on the following lines; this parser
        // cannot read it, so it stays fail-closed.
        assert_eq!(
            parse_done_probes(&fm("done_probes: |\n  echo ok")),
            ProbeDecl::Unparseable
        );
    }

    #[test]
    fn a_declaration_this_parser_cannot_read_is_never_no_gate() {
        // The vacuous-pass shape: the field is there, so the plan MEANT to gate.
        // Reporting None here would silently drop the gate entirely.
        let multiline_inline = fm("done_probes: [\n  \"echo a\",\n  \"echo b\"\n]");
        assert_eq!(parse_done_probes(&multiline_inline), ProbeDecl::Unparseable);
        assert_eq!(
            parse_done_probes(&fm("done_probes:\nstatus: ready")),
            ProbeDecl::Unparseable,
            "a declared-but-empty block must refuse, not pass"
        );
    }

    #[test]
    fn inline_list_keeps_escaped_quotes_inside_a_command() {
        // A mis-parsed probe is worse than a refused one: it would run a
        // DIFFERENT command than the plan declared and gate on its result.
        let doc = fm(r#"done_probes: ["sh -c \"echo hi\"", "echo ok"]"#);
        assert_eq!(
            probes_of(&doc),
            vec![r#"sh -c "echo hi""#.to_string(), "echo ok".to_string()]
        );
    }

    #[test]
    fn inline_list_preserves_a_trailing_bracket_and_refuses_an_unterminated_one() {
        assert_eq!(
            probes_of(&fm(r#"done_probes: ["echo [hi]"]"#)),
            vec!["echo [hi]".to_string()],
            "only the list's own closing bracket may be stripped"
        );
        assert_eq!(
            parse_done_probes(&fm(r#"done_probes: ["echo a""#)),
            ProbeDecl::Unparseable,
            "an unterminated inline list must refuse, not silently parse"
        );
    }

    #[test]
    fn a_comment_inside_the_block_does_not_swallow_the_probes() {
        let doc = fm("done_probes:\n  # why this probe exists\n  - echo a\n  - echo b\ntags: []");
        assert_eq!(
            probes_of(&doc),
            vec!["echo a".to_string(), "echo b".to_string()]
        );
    }

    #[test]
    fn block_list_stops_at_the_next_key() {
        let doc = fm("done_probes:\n  - echo a\ntags: []\nother: x");
        assert_eq!(probes_of(&doc), vec!["echo a".to_string()]);
    }

    #[test]
    fn probe_outcomes_render_pass_fail_and_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Duration::from_secs(10);
        // PASS requires exit 0 AND output: the run's own positive marker.
        assert_eq!(run_probe("echo probe-ok", tmp.path(), t).render(), "pass");
        // A silent exit 0 is SKIP, not pass: `test -f residue` reading as a
        // pass is the vacuous-probe trap this vocabulary exists to close.
        assert_eq!(run_probe("exit 0", tmp.path(), t).render(), "skip");
        assert_eq!(run_probe("exit 3", tmp.path(), t).render(), "fail:3");
        assert_eq!(
            run_probe("fno-no-such-binary-xyz", tmp.path(), t).render(),
            "fail:127",
            "a missing binary must fail closed as 127, never pass"
        );
    }

    #[test]
    fn probe_run_verdicts_map_the_four_states() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Duration::from_secs(10);
        assert_eq!(run_probe("echo marker", tmp.path(), t).verdict(), "PASS");
        assert_eq!(run_probe("exit 0", tmp.path(), t).verdict(), "SKIP");
        assert_eq!(
            run_probe("echo out; exit 4", tmp.path(), t).verdict(),
            "FAIL"
        );
        assert_eq!(
            run_probe("sleep 30", tmp.path(), Duration::from_millis(150)).verdict(),
            "BLOCKED",
            "a runner timeout is BLOCKED, distinct from FAIL: the probe never ran"
        );
    }

    #[test]
    fn probe_run_pipefail_closes_the_tail_trap() {
        // `failing | tail -5` reads exit 0 through the tail's status; the
        // pipefail preamble must surface the real failure.
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run_probe(
            "echo buried | grep -q nothing && echo found; exit 3 | tail -1",
            tmp.path(),
            Duration::from_secs(10),
        );
        assert_eq!(
            outcome.verdict(),
            "FAIL",
            "the trap must read FAIL, not PASS"
        );
    }

    #[test]
    fn a_blocked_probe_keeps_the_timeout_token_and_names_other_causes() {
        // A real timeout renders as the legacy token the scoreboard joins
        // on; spawn and wait failures must not be absorbed into it, or a
        // misconfigured probe reads as "too slow" when it never ran.
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run_probe("sleep 5", tmp.path(), Duration::from_millis(100));
        assert!(matches!(
            &outcome,
            ProbeOutcome::Blocked {
                kind: "timeout",
                ..
            }
        ));
        assert_eq!(outcome.render(), "timeout");
        assert_eq!(
            ProbeOutcome::Blocked {
                kind: "spawn",
                why: "boom".into()
            }
            .render(),
            "blocked:spawn"
        );
    }

    #[test]
    fn oversized_probe_output_travels_with_its_marker() {
        // The cap's contract: cut evidence is NAMED, so a short-looking tail
        // cannot pass as naturally short output.
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("p.md");
        std::fs::write(&plan, fm("done_probes:\n  - \"seq 1 5000\"")).unwrap();
        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            plan.to_string_lossy().into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
        ]);
        assert_eq!(code, 0);
        let body: Value = serde_json::from_str(&json).unwrap();
        let out = body["results"][0]["stdout"].as_str().unwrap();
        assert!(
            out.contains("[fno probe: truncated"),
            "row must name the cut: {out}"
        );
    }

    #[test]
    fn probe_run_rows_carry_verdict_claim_and_captured_output() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("p.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - \"echo header-present # the route returns the header\""),
        )
        .unwrap();
        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            plan.to_string_lossy().into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
        ]);
        assert_eq!(code, 0);
        let body: Value = serde_json::from_str(&json).unwrap();
        let row = &body["results"][0];
        assert_eq!(row["verdict"], "PASS");
        assert_eq!(row["claim"], "the route returns the header");
        // Captured output travels IN the row, not by path alone.
        assert!(row["stdout"].as_str().unwrap().contains("header-present"));
    }

    #[test]
    fn probe_run_silent_success_is_not_a_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("p.md");
        std::fs::write(&plan, fm("done_probes:\n  - \"exit 0\"")).unwrap();
        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            plan.to_string_lossy().into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
        ]);
        assert_eq!(code, 1, "a silent probe must not satisfy the gate");
        let body: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(body["results"][0]["verdict"], "SKIP");
        assert!(body["reason"].as_str().unwrap().contains("SKIP"));
    }

    /// The close-probe gate reads its OWN key, not done_probes. One parser,
    /// two keys; this pins that the parameterization did not silently collapse
    /// both onto done_probes.
    #[test]
    fn parse_probes_for_reads_close_probes_key() {
        let doc = fm("close_probes:\n  - \"exit 0\"\ndone_probes:\n  - \"exit 1\"\nstatus: ready");
        assert_eq!(
            probes_for(&doc, "close_probes"),
            vec!["exit 0".to_string()],
            "close_probes must read the close_probes list, not done_probes"
        );
        assert_eq!(probes_for(&doc, "done_probes"), vec!["exit 1".to_string()]);
    }

    /// `probe-run` exit contract: 0 passes, 1 fails a probe, 2 is undeterminable
    /// (unparseable / unreadable). stdout is always JSON.
    #[test]
    fn probe_run_exit_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let passing = tmp.path().join("pass.md");
        std::fs::write(&passing, fm("close_probes:\n  - \"echo probe-passed\"")).unwrap();
        let failing = tmp.path().join("fail.md");
        std::fs::write(&failing, fm("close_probes:\n  - \"exit 7\"")).unwrap();
        let bad = tmp.path().join("bad.md");
        std::fs::write(&bad, fm("close_probes: [unterminated")).unwrap();

        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            passing.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
            "--json".into(),
        ]);
        assert_eq!(code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap()["passed"],
            true
        );

        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            failing.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
            "--cwd".into(),
            tmp.path().to_string_lossy().into(),
        ]);
        assert_eq!(code, 1);
        let body = serde_json::from_str::<Value>(&json).unwrap();
        assert_eq!(body["passed"], false);
        assert!(body["reason"].as_str().unwrap().contains("exited 7"));

        // Unparseable declaration: undeterminable, fail closed.
        let (code, _) = decide_probe_run(&[
            "--plan".into(),
            bad.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
        ]);
        assert_eq!(code, 2);

        // Unreadable plan: undeterminable, fail closed.
        let (code, _) = decide_probe_run(&[
            "--plan".into(),
            tmp.path().join("nope.md").to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
        ]);
        assert_eq!(code, 2);
    }

    fn probes_for(doc: &str, key: &str) -> Vec<String> {
        match parse_probes_for(doc, key) {
            ProbeDecl::Probes(p) => p,
            other => panic!("expected probes for {key}, got {other:?}"),
        }
    }

    #[test]
    fn hanging_probe_is_killed_within_the_timeout_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let outcome = run_probe("sleep 30", tmp.path(), Duration::from_millis(200));
        assert_eq!(outcome.render(), "timeout");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "run_probe must return on its own timeout, not wait out the child"
        );
    }

    #[test]
    fn chatty_probe_does_not_deadlock_on_the_stderr_pipe() {
        // A probe writing past the 64KB pipe buffer would hang forever if
        // stderr were drained only after exit.
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run_probe(
            "head -c 200000 /dev/zero | tr '\\0' 'x' >&2; exit 1",
            tmp.path(),
            Duration::from_secs(20),
        );
        assert_eq!(outcome.render(), "fail:1");
        match outcome {
            ProbeOutcome::Fail { stderr, .. } => assert!(
                stderr.len() <= PROBE_STDERR_CAP,
                "stderr must be truncated to {PROBE_STDERR_CAP}"
            ),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn over_cap_declaration_refuses_without_running_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        let sentinel = tmp.path().join("ran");
        std::fs::write(
            &plan,
            fm(&format!(
                "done_probes:\n  - touch {0}\n  - echo b\n  - echo c\n  - echo d",
                sentinel.display()
            )),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => {
                assert!(
                    reason.contains("cap is 3"),
                    "reason names the cap: {reason}"
                )
            }
            _ => panic!("over-cap declaration must refuse"),
        }
        assert!(!sentinel.exists(), "an over-cap list must not execute");
    }

    #[test]
    fn unreadable_plan_fails_closed_only_when_probes_were_seen_before() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        let missing = tmp.path().join("gone.md");

        // AC2-FR: no probe history -> today's behavior exactly.
        assert!(matches!(
            evaluate_done_probes(
                missing.to_str(),
                None,
                tmp.path(),
                &events,
                "s1",
                Duration::from_secs(10)
            ),
            ProbeGate::Absent
        ));

        // AC1-FR: a prior fire recorded probes -> undeterminable, fail closed.
        std::fs::write(
            &events,
            "{\"type\":\"loop_check\",\"data\":{\"session_id\":\"s1\",\"done_probes\":{\"echo ok\":\"pass\"}}}\n",
        )
        .unwrap();
        match evaluate_done_probes(
            missing.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(
                reason.contains("undeterminable"),
                "reason must say undeterminable: {reason}"
            ),
            _ => panic!("unreadable plan with probe history must fail closed"),
        }
    }

    #[test]
    fn a_refusal_where_nothing_ran_still_records_probe_history() {
        // Otherwise prior_fires_declared_probes sees no history, and a plan that
        // tripped the cap and then went missing degrades to "no gate".
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - echo a\n  - echo b\n  - echo c\n  - echo d"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        let ProbeGate::Fail { results, .. } = evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) else {
            panic!("over-cap must refuse");
        };
        std::fs::write(
            &events,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "loop_check",
                    "data": {"session_id": "s1", "done_probes": results}
                })
            ),
        )
        .unwrap();
        assert!(
            prior_fires_declared_probes(&events, "s1"),
            "a declared-but-never-ran refusal must be visible as probe history"
        );
    }

    #[test]
    fn relative_plan_path_resolves_against_the_session_cwd() {
        // plan_path is repo-relative in practice; resolving against the process
        // cwd would read nothing and silently drop the gate.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("plan.md"),
            fm("done_probes:\n  - echo probe-ok"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        assert!(
            matches!(
                evaluate_done_probes(
                    Some("plan.md"),
                    None,
                    tmp.path(),
                    &events,
                    "s1",
                    Duration::from_secs(10)
                ),
                ProbeGate::Pass(_)
            ),
            "a relative plan_path must resolve against cwd, not the process cwd"
        );
    }

    #[test]
    fn timeout_reaches_the_gate_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(&plan, fm("done_probes:\n  - sleep 30")).unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_millis(200),
        ) {
            ProbeGate::Fail { reason, results } => {
                assert!(
                    reason.contains("timed out"),
                    "reason names the timeout: {reason}"
                );
                assert_eq!(results["sleep 30"], "timeout");
            }
            _ => panic!("a hanging probe must refuse done"),
        }
    }

    #[test]
    fn a_pipeline_probe_timeout_does_not_hang_the_gate() {
        // `sh -c "a | b"` forks: killing only sh leaves grandchildren holding
        // the stderr pipe, so the drain thread never sees EOF. This is the
        // documented probe shape, so a regression here wedges every session.
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let outcome = run_probe("sleep 30 | cat", tmp.path(), Duration::from_millis(200));
        assert_eq!(outcome.render(), "timeout");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a pipeline probe must not outlive its timeout (took {:?})",
            start.elapsed()
        );
    }

    #[test]
    fn multibyte_stderr_is_truncated_without_panicking() {
        // String::drain panics off a char boundary; probe stderr routinely
        // carries arrows and box-drawing characters.
        let mut s = "→".repeat(400); // 3 bytes each, straddles the cut
        keep_last_on_char_boundary(&mut s, PROBE_STDERR_CAP);
        assert!(s.len() <= PROBE_STDERR_CAP);
        assert!(s.chars().all(|c| c == '→'), "must not split a character");
    }

    #[test]
    fn stderr_cap_keeps_the_tail_where_the_error_is() {
        let mut s = format!("{}\nthe actual error", "noise ".repeat(200));
        keep_last_on_char_boundary(&mut s, PROBE_STDERR_CAP);
        assert!(
            s.ends_with("the actual error"),
            "the last line is the diagnostic; keeping the prefix drops it: {s}"
        );
    }

    #[test]
    fn block_scalar_escapes_decode_to_the_command_the_plan_meant() {
        // Leaving `\"` in would hand sh a DIFFERENT command than declared, and
        // would key the event by a string the PyYAML-side grader never matches.
        let doc = fm("done_probes:\n  - \"test -n \\\"$(echo hi)\\\"\"");
        assert_eq!(probes_of(&doc), vec![r#"test -n "$(echo hi)""#.to_string()]);
    }

    #[test]
    fn single_quoted_scalar_undoubles_its_quote() {
        let doc = fm("done_probes:\n  - 'echo it''s fine'");
        assert_eq!(probes_of(&doc), vec!["echo it's fine".to_string()]);
    }

    #[test]
    fn plan_path_fragment_is_stripped_before_reading() {
        // `plans/p.md#wave-1` must resolve to plans/p.md, not a literal filename
        // containing the fragment (which would read nothing -> silent Absent).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("plan.md"),
            fm("done_probes:\n  - echo probe-ok"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        assert!(
            matches!(
                evaluate_done_probes(
                    Some("plan.md#wave-1"),
                    None,
                    tmp.path(),
                    &events,
                    "s1",
                    Duration::from_secs(10)
                ),
                ProbeGate::Pass(_)
            ),
            "a fragment in plan_path must not silently disable the gate"
        );
    }

    #[test]
    fn a_backgrounding_probe_does_not_block_the_drain() {
        // sh exits immediately while the descendant keeps stderr open, so the
        // timeout loop is already over and only the group kill bounds the join.
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let outcome = run_probe(
            "sleep 300 & echo probe-ok",
            tmp.path(),
            Duration::from_secs(30),
        );
        assert_eq!(outcome.render(), "pass");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a backgrounded descendant must not hold the drain open (took {:?})",
            start.elapsed()
        );
    }

    // ── project-level done_probes (x-a534) ────────────────────────────────
    //
    // A repo-wide guardrail must apply to every plan in the repo, and no plan
    // doc may switch it off - a guard on one of two reachable paths is
    // decorative.

    fn project(cmds: &[&str]) -> Result<Vec<String>, String> {
        Ok(cmds.iter().map(|c| c.to_string()).collect())
    }

    /// A plan doc that declares no probes of its own.
    fn bare_plan(dir: &Path) -> std::path::PathBuf {
        let plan = dir.join("plan.md");
        std::fs::write(&plan, fm("title: p")).unwrap();
        plan
    }

    #[test]
    fn a_project_probe_gates_a_plan_that_declares_none() {
        // AC1-HP: the repo-wide guardrail runs without being retyped per plan,
        // and its result reaches the event payload.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["echo probe-ok"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Pass(results) => assert_eq!(results["echo probe-ok"], "pass"),
            _ => panic!("a passing project probe must let the gate through"),
        }
    }

    #[test]
    fn a_failing_project_probe_blocks_and_names_its_source() {
        // AC2-ERR: `probe X exited 1` is ambiguous once there are two
        // declarations; the operator has to know which file to edit.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["false"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(
                reason.contains("project probe `false`"),
                "the reason must name the source: {reason}"
            ),
            _ => panic!("a failing project probe must block"),
        }
    }

    // AC3-INV (a plan declaring `done_probes: []` cannot silence the project's
    // gate) is covered end to end by
    // done_probes_ac3_inv_a_plan_cannot_silence_the_project_gate in
    // tests/loop_check.rs, which drives the real settings merge rather than a
    // hand-built probe list. A unit-level twin would assert strictly less.

    #[test]
    fn an_unparseable_project_declaration_blocks_rather_than_degrading() {
        // AC4-ERR: a config key that degrades to no-gate is a guardrail that
        // disappears when you typo it.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        let junk: Result<Vec<String>, String> = value_as_probe_list(
            &"done_probes = { a = 1 }".parse::<toml::Value>().unwrap()["done_probes"],
        );
        assert!(junk.is_err(), "a mapping is not a probe list");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&junk),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, results } => {
                assert!(
                    reason.contains("undeterminable"),
                    "must use the plan side's vocabulary: {reason}"
                );
                assert_eq!(results["_undeterminable"], "unparseable-config-declaration");
            }
            _ => panic!("an unreadable project declaration must block"),
        }
    }

    #[test]
    fn the_cap_is_per_source_not_per_union() {
        // AC5-BOUND: 3 + 3 all run. Sharing one budget would make two
        // independent authors compete for one number, so a project policy
        // would eat a plan's operational probes.
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - echo d\n  - echo e\n  - echo f"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["echo a", "echo b", "echo c"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Pass(results) => assert_eq!(
                results.as_object().unwrap().len(),
                6,
                "all six probes must run: {results}"
            ),
            other => panic!(
                "3 + 3 is within the per-source cap: {}",
                match other {
                    ProbeGate::Fail { reason, .. } => reason,
                    _ => "Absent".to_string(),
                }
            ),
        }

        // A 4th in the project declaration is still a loud refusal.
        match evaluate_done_probes(
            plan.to_str(),
            Some(&project(&["echo a", "echo b", "echo c", "echo d"])),
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(
                reason.contains("config.toml declares 4") && reason.contains("per source"),
                "an over-cap project list must refuse loudly: {reason}"
            ),
            _ => panic!("4 project probes must refuse"),
        }
    }

    #[test]
    fn no_declaration_on_either_source_stays_absent() {
        // The zero-subprocess path must survive the second source.
        let tmp = tempfile::tempdir().unwrap();
        let plan = bare_plan(tmp.path());
        let events = tmp.path().join("events.jsonl");
        assert!(matches!(
            evaluate_done_probes(
                plan.to_str(),
                Some(&project(&[])),
                tmp.path(),
                &events,
                "s1",
                Duration::from_secs(10)
            ),
            ProbeGate::Absent
        ));
    }

    #[test]
    fn config_done_probes_parses_off_the_flat_root() {
        // The file is flat: `done_probes` at the root, not nested under a
        // `config` table.
        let s = parse_settings("done_probes = [\"make a11y-check\"]\n");
        assert_eq!(s.done_probes, Some(Ok(vec!["make a11y-check".to_string()])),);
        assert_eq!(parse_settings("plans_dir = \"x\"\n").done_probes, None);
        assert!(parse_settings("done_probes = \"nope\"\n")
            .done_probes
            .unwrap()
            .is_err());
        assert!(parse_settings("done_probes = [1]\n")
            .done_probes
            .unwrap()
            .is_err());
    }

    #[test]
    fn quota_promised_done_check_uses_the_fixed_coverage_adapter() {
        assert_eq!(coverage_adapter("fno-gh-loopcheck"), "fno-gh-coverage");
        assert_eq!(
            coverage_adapter("/tmp/bin/fno-gh-loopcheck"),
            "/tmp/bin/fno-gh-coverage"
        );
        assert_eq!(coverage_adapter("/tmp/fake-gh"), "/tmp/fake-gh");
    }

    #[test]
    fn quota_rest_failures_do_not_claim_graphql_exhaustion() {
        assert!(is_graphql_read("pr_reviews"));
        assert!(is_graphql_read("pr_reviews_parse"));
        assert!(!is_graphql_read("pr_info_rest"));
        assert!(!is_graphql_read("pr_status_rest"));
        assert!(!is_graphql_read("pr_status_rest_parse"));
    }
}

#[cfg(test)]
mod acceptance_evidence_tests {
    // x-d098: bindings parse fail-closed, validate structurally, and evaluate
    // to criterion-level coverage in the probe-run payload and the session
    // gate's event map.
    use super::*;
    use std::time::Duration;

    fn fm(body: &str) -> String {
        format!("---\ntitle: t\n{body}\n---\n\n# doc\n")
    }

    fn decl_of(doc: &str) -> EvidenceDecl {
        parse_acceptance_evidence(doc)
    }

    #[test]
    fn a_binding_block_parses_to_bound_criteria() {
        let doc = fm(
            "acceptance_evidence:\n  required: true\n  bindings:\n    AC1-HP: done_probes[0]\n    AC2-HP: close_probes[0]",
        );
        match decl_of(&doc) {
            EvidenceDecl::Evidence { required, bindings } => {
                assert!(required);
                assert_eq!(bindings.len(), 2);
                assert_eq!(bindings[0].ac, "AC1-HP");
                assert_eq!(bindings[0].key, "done_probes");
                assert_eq!(bindings[0].index, 0);
                assert_eq!(bindings[1].key, "close_probes");
                assert_eq!(bindings[1].index, 0);
            }
            other => panic!("expected Evidence, got {other:?}"),
        }
    }

    #[test]
    fn absent_and_explicitly_empty_are_unarmed_not_required() {
        assert_eq!(decl_of(&fm("status: ready")), EvidenceDecl::None);
        assert_eq!(
            decl_of(&fm("acceptance_evidence: {}")),
            EvidenceDecl::Evidence {
                required: false,
                bindings: vec![]
            }
        );
        assert_eq!(
            decl_of(&fm("acceptance_evidence: []")),
            EvidenceDecl::Evidence {
                required: false,
                bindings: vec![]
            }
        );
    }

    #[test]
    fn unreadable_declarations_refuse_not_degrade() {
        assert_eq!(
            decl_of(&fm(
                "acceptance_evidence:\n  bindings:\n    AC1-HP: probes[0]"
            )),
            EvidenceDecl::Unparseable,
            "a malformed reference must refuse, never silently disarm"
        );
        assert_eq!(
            decl_of(&fm("acceptance_evidence:\n  who_knows: yes")),
            EvidenceDecl::Unparseable,
            "an unknown sub-key must refuse"
        );
        assert_eq!(
            decl_of(&fm("acceptance_evidence:\n  bindings:\n    AC1-HP: done_probes[0]\n    AC1-HP: close_probes[0]")),
            EvidenceDecl::Unparseable,
            "a criterion bound in both terminals must refuse"
        );
        assert_eq!(
            decl_of(&fm("acceptance_evidence:\n  required: maybe")),
            EvidenceDecl::Unparseable
        );
    }

    #[test]
    fn validation_refuses_a_missing_probe_by_name() {
        let decl = decl_of(&fm(
            "acceptance_evidence:\n  bindings:\n    AC1-HP: done_probes[1]",
        ));
        let EvidenceDecl::Evidence { bindings, .. } = decl else {
            panic!("expected Evidence")
        };
        let why = validate_bindings(&bindings, 1).unwrap_err();
        assert!(why.contains("AC1-HP") && why.contains("missing"), "{why}");
    }

    #[test]
    fn coverage_maps_each_verdict_to_its_criterion() {
        let decl = decl_of(&fm(
            "acceptance_evidence:\n  bindings:\n    AC1-HP: done_probes[0]\n    AC2-HP: done_probes[1]\n    AC3-HP: done_probes[2]\n    AC4-HP: close_probes[0]",
        ));
        let EvidenceDecl::Evidence { bindings, .. } = decl else {
            panic!("expected Evidence")
        };
        let outcomes = vec![
            ("echo pass".to_string(), "pass".to_string()),
            ("exit 3".to_string(), "fail:3".to_string()),
            ("sleep 90".to_string(), "timeout".to_string()),
        ];
        let rows = acceptance_coverage(&bindings, "done_probes", &outcomes);
        assert_eq!(rows.len(), 3, "the close binding is Pending here");
        assert_eq!(rows[0]["status"], "satisfied");
        assert_eq!(rows[0]["ac"], "AC1-HP");
        assert_eq!(rows[0]["terminal"], "session");
        assert_eq!(rows[1]["status"], "failed");
        assert_eq!(rows[2]["status"], "unknown");
    }

    #[test]
    fn a_failing_bound_probe_names_its_criterion_at_the_close_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("close_probes:\n  - \"echo shipped-marker\"\n  - \"exit 4\"\nacceptance_evidence:\n  bindings:\n    AC1-HP: close_probes[0]\n    AC2-HP: close_probes[1]"),
        )
        .unwrap();
        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            plan.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
            "--json".into(),
        ]);
        assert_eq!(code, 1);
        let payload: Value = serde_json::from_str(&json).unwrap();
        assert!(
            payload["reason"].as_str().unwrap().contains("AC2-HP"),
            "{json}"
        );
        let rows = payload["acceptance_coverage"].as_array().unwrap();
        assert_eq!(rows[0]["status"], "satisfied");
        assert_eq!(rows[1]["status"], "failed");
    }

    #[test]
    fn required_unarmed_close_evidence_cannot_read_as_a_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(&plan, fm("acceptance_evidence:\n  required: true")).unwrap();
        let (code, json) = decide_probe_run(&[
            "--plan".into(),
            plan.to_string_lossy().into(),
            "--key".into(),
            "close_probes".into(),
            "--json".into(),
        ]);
        assert_eq!(code, 1, "unarmed required must refuse, not exit 0");
        assert!(json.contains("unarmed"), "{json}");
    }

    #[test]
    fn session_gate_refuses_a_required_plan_with_no_done_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(&plan, fm("acceptance_evidence:\n  required: true")).unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(reason.contains("unarmed"), "{reason}"),
            other => panic!("unarmed required must refuse, got {other:?}"),
        }
    }

    #[test]
    fn close_only_bindings_do_not_arm_a_required_session_terminal() {
        // A close_probes binding is Pending at the session terminal, so it
        // cannot be the satisfied evidence a required declaration promises.
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("close_probes:\n  - \"echo close-marker\"\nacceptance_evidence:\n  required: true\n  bindings:\n    AC1-HP: close_probes[0]"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Fail { reason, .. } => assert!(reason.contains("unarmed"), "{reason}"),
            other => panic!("close-only bindings must not arm the session, got {other:?}"),
        }
    }

    #[test]
    fn session_gate_passes_with_coverage_when_a_bound_probe_satisfies() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - \"echo runs-marker\"\nacceptance_evidence:\n  required: true\n  bindings:\n    AC1-HP: done_probes[0]"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Pass(results) => {
                let rows = results["_acceptance_coverage"].as_array().unwrap();
                assert_eq!(rows[0]["status"], "satisfied");
                assert_eq!(rows[0]["ac"], "AC1-HP");
            }
            other => panic!("a satisfied required binding must pass, got {other:?}"),
        }
    }

    #[test]
    fn session_gate_passes_while_close_scope_is_pending() {
        // AC2-EDGE: session evidence satisfied, close evidence pending - the
        // session may stop; node closure is the close terminal's business.
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.md");
        std::fs::write(
            &plan,
            fm("done_probes:\n  - \"echo session-marker\"\nclose_probes:\n  - \"exit 1\"\nacceptance_evidence:\n  bindings:\n    AC1-HP: done_probes[0]\n    AC2-HP: close_probes[0]"),
        )
        .unwrap();
        let events = tmp.path().join("events.jsonl");
        match evaluate_done_probes(
            plan.to_str(),
            None,
            tmp.path(),
            &events,
            "s1",
            Duration::from_secs(10),
        ) {
            ProbeGate::Pass(results) => {
                let rows = results["_acceptance_coverage"].as_array().unwrap();
                assert_eq!(rows.len(), 1, "close-scope binding is Pending here");
                assert_eq!(rows[0]["status"], "satisfied");
            }
            other => panic!("session must pass while close is pending, got {other:?}"),
        }
    }
}
