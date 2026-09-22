//! `corrections-verify`: scores each applied correction against the friction
//! it targeted, native under d-b6cc1a2a (all new code lands in crates; the
//! bash pack is a thin caller). Transport-only, dispatched in client.rs
//! beside `evals-trend`; registers no verb.
//!
//! Inputs: `corrections.log` rows whose SOURCE is `git-rule-edit` or
//! `skill-commit` (the post-commit hook's applied-correction rows) and
//! `events.jsonl`
//! `termination` + `loop_check` rows. Friction per session: 1 when the
//! termination reason classifies stuck (`run_outcome::classify_legacy`, the
//! one stuck definition) plus the session's `loop_check` block rows. Each
//! correction reads the mean friction of the 3 sessions that ended before it
//! and the first 3 that ended after it; the verdict copies the
//! `evals_trend` improved/regressed shape with `flat` and
//! `insufficient-data` added. Read-only over both files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::evals_trend::round4;

const EXIT_USAGE: i32 = 2;
const USAGE: &str = "usage: fno-agents corrections-verify (--json | -J | --markdown) [--since <Nd>] [--log <path>] [--events <path>] [--now <rfc3339>]";

/// One applied correction from corrections.log.
/// Line shape: `{ts} | {severity} | {source} | {location} | {details}`
/// (scripts/lib/corrections-lock.sh `corrections_build_line`). The
/// post-commit hook appends ` sha=<12 hex>` and, when the commit carries
/// the triage-minted trailer, ` ref=<review>#<N>` to DETAILS.
struct AppliedCorrection {
    ts: DateTime<Utc>,
    file: String,
    details: String,
    sha: Option<String>,
    reference: Option<String>,
}

/// One ended session: when it ended, whether the end was stuck, and how
/// many loop_check block rows it accumulated.
struct SessionEnd {
    ts: DateTime<Utc>,
    stuck: bool,
    blocks: usize,
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn resolve_log(flag: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(PathBuf::from(p));
    }
    // One resolution with the finalize writer: the same override chain, no
    // second copy of the literals (the reachable-paths twin holds them).
    let home = std::env::var_os("HOME").map(PathBuf::from);
    crate::finalize::corrections_log_path(home.as_deref())
}

fn resolve_events(flag: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    crate::finalize::loop_state_root(home.as_deref()).map(|p| p.join("events.jsonl"))
}

/// The `key=value` token a writer appended to DETAILS, as its value.
fn detail_token(details: &str, key: &str) -> Option<String> {
    details
        .split_whitespace()
        .find_map(|t| t.strip_prefix(key))
        .map(str::to_string)
}

/// The applied corrections: the post-commit hook's rows only
/// (`git-rule-edit` and `skill-commit`); a `target-postmortem` pointer is
/// not a correction and is never scored.
fn read_corrections(log: &str, since: DateTime<Utc>) -> Vec<AppliedCorrection> {
    let mut out = Vec::new();
    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.splitn(5, " | ").collect();
        let [ts, _sev, source, location, details] = fields.as_slice() else {
            continue;
        };
        if !matches!(*source, "git-rule-edit" | "skill-commit") {
            continue;
        }
        let (Some(ts), location) = (parse_ts(ts), (*location).trim()) else {
            continue;
        };
        if ts < since {
            continue;
        }
        let details = (*details).trim();
        out.push(AppliedCorrection {
            ts,
            file: location.to_string(),
            details: details.to_string(),
            sha: detail_token(details, "sha="),
            reference: detail_token(details, "ref="),
        });
    }
    out.sort_by_key(|c| c.ts);
    out
}

/// Session ends from events.jsonl: the latest termination row per
/// session_id, joined with the session's loop_check block count. Stuck
/// comes from `run_outcome::classify_legacy` so the friction set never
/// forks from the termination classifier.
fn read_sessions(events: &str) -> Vec<SessionEnd> {
    let mut ends: BTreeMap<String, (DateTime<Utc>, bool)> = BTreeMap::new();
    let mut blocks: BTreeMap<String, usize> = BTreeMap::new();
    for line in events.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(data) = v.get("data") else {
            continue;
        };
        let Some(session_id) = data
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("termination") => {
                let Some(ts) = v.get("ts").and_then(Value::as_str).and_then(parse_ts) else {
                    continue;
                };
                let reason = data.get("reason").and_then(Value::as_str).unwrap_or("");
                let stuck = crate::run_outcome::classify_legacy(reason)
                    .map(|o| o.projection().stuck)
                    .unwrap_or(false);
                // Last termination wins: a session ends once, a repeated
                // row re-reads the same end.
                ends.entry(session_id.to_string())
                    .and_modify(|(keep, was_stuck)| {
                        if ts > *keep {
                            *keep = ts;
                            *was_stuck = stuck;
                        }
                    })
                    .or_insert((ts, stuck));
            }
            Some("loop_check") => {
                if data.get("decision").and_then(Value::as_str) == Some("block") {
                    *blocks.entry(session_id.to_string()).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    // window_mean slices the head/tail by ts, so return ends in ts order;
    // the BTreeMap above iterates by session id, which correlates with
    // nothing.
    let mut out: Vec<SessionEnd> = ends
        .into_iter()
        .map(|(session_id, (ts, stuck))| SessionEnd {
            ts,
            stuck,
            blocks: blocks.get(&session_id).copied().unwrap_or(0),
        })
        .collect();
    out.sort_by_key(|s| s.ts);
    out
}

fn friction(s: &SessionEnd) -> f64 {
    let stuck_part = if s.stuck { 1.0 } else { 0.0 };
    stuck_part + s.blocks as f64
}

/// Mean friction over up to `n` sessions: the `n` latest that ended before
/// `at`, or the `n` earliest that ended after it.
fn window_mean(sessions: &[&SessionEnd], at: DateTime<Utc>, after: bool, n: usize) -> (f64, usize) {
    let mut picked: Vec<&SessionEnd> = sessions
        .iter()
        .copied()
        .filter(|s| if after { s.ts > at } else { s.ts < at })
        .collect();
    // Sessions sort ascending by ts: "latest before" takes the tail,
    // "earliest after" keeps the head.
    if after {
        picked = picked.into_iter().take(n).collect();
    } else {
        let len = picked.len();
        picked = picked[len.saturating_sub(n)..].to_vec();
    }
    let count = picked.len();
    let mean = if count == 0 {
        0.0
    } else {
        picked.iter().map(|s| friction(s)).sum::<f64>() / count as f64
    };
    (mean, count)
}

/// One scored correction: the before/after means, the ratio, and the
/// verdict (improved under 0.7x, worse over 1.3x, flat between,
/// insufficient-data when either side has fewer than 3 sessions).
#[derive(Debug, Clone)]
struct Verdict {
    ts: DateTime<Utc>,
    file: String,
    details: String,
    sha: Option<String>,
    reference: Option<String>,
    before: f64,
    after: f64,
    ratio: Option<f64>,
    sessions_before: usize,
    sessions_after: usize,
    verdict: &'static str,
}

fn score(corrections: &[AppliedCorrection], sessions: &[SessionEnd]) -> Vec<Verdict> {
    let refs: Vec<&SessionEnd> = sessions.iter().collect();
    corrections
        .iter()
        .map(|c| {
            let (before, sb) = window_mean(&refs, c.ts, false, 3);
            let (after, sa) = window_mean(&refs, c.ts, true, 3);
            let ratio = if sb < 3 || sa < 3 {
                None
            } else if before == 0.0 {
                Some(if after == 0.0 { 1.0 } else { f64::INFINITY })
            } else {
                Some(after / before)
            };
            let verdict = match ratio {
                None => "insufficient-data",
                Some(r) if r < 0.7 => "improved",
                Some(r) if r > 1.3 => "worse",
                Some(_) => "flat",
            };
            Verdict {
                ts: c.ts,
                file: c.file.clone(),
                details: c.details.clone(),
                sha: c.sha.clone(),
                reference: c.reference.clone(),
                before,
                after,
                ratio,
                sessions_before: sb,
                sessions_after: sa,
                verdict,
            }
        })
        .collect()
}

/// The roll-back sha for a `worse` correction: the last commit to that rule
/// file before the correction was applied, read from the claude dir's git
/// history (the same repo the post-commit hook fired in). None when the
/// repo or the file has no such commit.
fn revert_sha(claude_dir: &str, file: &str, at: DateTime<Utc>) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            claude_dir,
            "log",
            "--before",
            &at.to_rfc3339(),
            "-1",
            "--format=%H",
            "--",
            file,
        ])
        .output()
        .ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && sha.len() == 40 {
        Some(sha)
    } else {
        None
    }
}

fn resolve_claude_dir() -> String {
    crate::claude_roster::config_dir()
        .to_string_lossy()
        .into_owned()
}

fn markdown(verdicts: &[Verdict]) -> String {
    if verdicts.is_empty() {
        return "no applied corrections in window\n".to_string();
    }
    let claude_dir = resolve_claude_dir();
    let mut lines = Vec::new();
    for v in verdicts {
        let verb = match v.verdict {
            "improved" => "keep".to_string(),
            "flat" => "improve".to_string(),
            "worse" => match v.sha.as_deref().map_or_else(
                || revert_sha(&claude_dir, &v.file, v.ts),
                |s| Some(s.to_string()),
            ) {
                Some(sha) => format!("git revert {sha}"),
                None => format!(
                    "roll back (sha unresolved: git -C {claude_dir} log --before={} -- {})",
                    v.ts.to_rfc3339(),
                    v.file
                ),
            },
            _ => format!(
                "insufficient-data (before={}, after={})",
                v.sessions_before, v.sessions_after
            ),
        };
        // The ref names the proposal the commit applied, closing the chain
        // evidence rows -> review item -> node -> commit -> verdict.
        let ref_suffix = match v.reference.as_deref() {
            Some(r) => format!("; ref={r}"),
            None => String::new(),
        };
        lines.push(format!(
            "- {} {}: {} ({verb}{ref_suffix})",
            v.ts.to_rfc3339(),
            v.file,
            v.verdict
        ));
    }
    lines.join("\n") + "\n"
}

fn json_rows(verdicts: &[Verdict]) -> Value {
    Value::Array(
        verdicts
            .iter()
            .map(|v| {
                json!({
                    "ts": v.ts.to_rfc3339(),
                    "file": v.file,
                    "details": v.details,
                    "sha": v.sha,
                    "ref": v.reference,
                    "before": round4(v.before),
                    "after": round4(v.after),
                    "ratio": v.ratio.map(round4),
                    "verdict": v.verdict,
                    "sessions_before": v.sessions_before,
                    "sessions_after": v.sessions_after,
                })
            })
            .collect(),
    )
}

/// The value for a flag: an inline `--flag=v`, else the next non-flag arg
/// (consuming it).
fn flag_value(args: &[String], i: &mut usize, inline: Option<String>) -> Option<String> {
    if inline.is_some() {
        return inline;
    }
    let next = args
        .get(*i)
        .filter(|t| !t.starts_with('-'))
        .cloned()
        .inspect(|_| *i += 1);
    next
}

pub fn run(args: &[String]) -> i32 {
    let mut markdown_out = false;
    let mut json_out = false;
    let mut since_days: i64 = 90;
    let mut log: Option<String> = None;
    let mut events: Option<String> = None;
    let mut now: Option<DateTime<Utc>> = None;
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        match name.as_str() {
            "--markdown" => markdown_out = true,
            x if crate::json_output::is_flag(x) => json_out = true,
            "--since" => match flag_value(args, &mut i, inline) {
                Some(v) => match v.strip_suffix('d').unwrap_or(&v).parse::<i64>() {
                    Ok(d) if d >= 0 => since_days = d,
                    _ => {
                        eprintln!("corrections-verify: --since needs <Nd>");
                        return EXIT_USAGE;
                    }
                },
                None => {
                    eprintln!("corrections-verify: --since needs <Nd>");
                    return EXIT_USAGE;
                }
            },
            "--log" => match flag_value(args, &mut i, inline) {
                Some(v) => log = Some(v),
                None => {
                    eprintln!("corrections-verify: --log needs a path");
                    return EXIT_USAGE;
                }
            },
            "--events" => match flag_value(args, &mut i, inline) {
                Some(v) => events = Some(v),
                None => {
                    eprintln!("corrections-verify: --events needs a path");
                    return EXIT_USAGE;
                }
            },
            "--now" => match flag_value(args, &mut i, inline)
                .as_deref()
                .and_then(parse_ts)
            {
                Some(ts) => now = Some(ts),
                None => {
                    eprintln!("corrections-verify: --now needs an rfc3339 timestamp");
                    return EXIT_USAGE;
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            _ => {
                eprintln!("corrections-verify: unknown argument: {arg}");
                eprintln!("{USAGE}");
                return EXIT_USAGE;
            }
        }
    }
    if markdown_out == json_out {
        eprintln!("corrections-verify: pick exactly one of --json / --markdown");
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }
    let now = now.unwrap_or_else(Utc::now);
    let since = now - chrono::Duration::days(since_days);
    let log_path = match resolve_log(log.as_deref()) {
        Some(p) => p,
        None => {
            eprintln!("corrections-verify: no home resolved for corrections.log");
            return 1;
        }
    };
    let log_text = std::fs::read_to_string(&log_path).unwrap_or_default();
    let events_text = match resolve_events(events.as_deref()) {
        Some(p) => std::fs::read_to_string(&p).unwrap_or_default(),
        None => String::new(),
    };
    let verdicts = score(
        &read_corrections(&log_text, since),
        &read_sessions(&events_text),
    );
    if markdown_out {
        print!("{}", markdown(&verdicts));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_rows(&verdicts)).unwrap_or_else(|_| "[]".into())
        );
    }
    0
}

#[cfg(test)]
mod tests;
